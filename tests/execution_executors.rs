use std::fs;
use std::num::NonZeroU16;
use std::ops::Bound;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::Catalog;
use ferro_db::config::PAGE_SIZE;
use ferro_db::execution::{
    BinaryOp, Delete, ExecutionContext, Executor, Expr, Filter, Insert, Row, SeqScan, Update,
    Values, execute,
};
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::storage::rid::Rid;
use ferro_db::table::{Column, Schema};
use ferro_db::types::{DataType, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(
            std::env::temp_dir().join(format!("ferro-execution-it-{}-{id}.db", std::process::id())),
        )
    }

    fn open(&self) -> (BufferPoolManager, Catalog) {
        let mut bpm = BufferPoolManager::new(8, PAGE_SIZE, &self.0).unwrap();
        let catalog = Catalog::open(&mut bpm).unwrap();
        (bpm, catalog)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn run(bpm: &mut BufferPoolManager, catalog: &mut Catalog, plan: &mut dyn Executor) -> Vec<Row> {
    execute(plan, &mut ExecutionContext { catalog, bpm }).unwrap()
}

fn int(value: i32) -> Expr {
    Expr::Literal(Value::Integer(value))
}

fn email(id: i32) -> String {
    format!("user{id}@example.com")
}

/// `users (id INTEGER NOT NULL, email VARCHAR(400), age SMALLINT)` with unique indexes on
/// `id` and `email`, filled with 3,000 rows through the Insert executor.
fn create_users(bpm: &mut BufferPoolManager, catalog: &mut Catalog) {
    let schema = Schema::new(vec![
        Column::not_null("id", DataType::Integer),
        Column::new("email", DataType::Varchar(NonZeroU16::new(400).unwrap())),
        Column::new("age", DataType::SmallInt),
    ])
    .unwrap();
    catalog.create_table(bpm, "users", schema).unwrap();
    catalog
        .create_index(bpm, "users_id", "users", "id")
        .unwrap();
    catalog
        .create_index(bpm, "users_email", "users", "email")
        .unwrap();
    let rows: Vec<Vec<Expr>> = (0..3_000)
        .map(|id| {
            let age = if id % 10 == 0 {
                Value::Null
            } else {
                Value::SmallInt((id % 90) as i16)
            };
            vec![
                int(id),
                Expr::Literal(Value::Text(email(id))),
                Expr::Literal(age),
            ]
        })
        .collect();
    let inserted = run(
        bpm,
        catalog,
        &mut Insert::new("users", Box::new(Values::new(rows))),
    );
    assert_eq!(inserted[0].values, [Value::BigInt(3_000)]);
}

fn scan(bpm: &mut BufferPoolManager, catalog: &mut Catalog) -> Vec<Row> {
    run(bpm, catalog, &mut SeqScan::new("users"))
}

/// Each index holds exactly the non-null keys of its column, with their rows' `Rid`s.
fn assert_indexes_match_table(bpm: &mut BufferPoolManager, catalog: &mut Catalog) {
    let rows = scan(bpm, catalog);
    for (name, column) in [("users_id", 0), ("users_email", 1)] {
        let mut expected: Vec<(Value, Rid)> = rows
            .iter()
            .filter(|row| !row.values[column].is_null())
            .map(|row| (row.values[column].clone(), row.rid.unwrap()))
            .collect();
        expected.sort_by(|lhs, rhs| lhs.0.sort_cmp(&rhs.0));
        let tree = catalog.index_tree(bpm, name).unwrap();
        let found: Vec<(Value, Rid)> = tree
            .range(bpm, Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect();
        assert_eq!(found, expected, "{name}");
    }
}

fn where_id(op: BinaryOp, value: i32) -> Box<dyn Executor> {
    Box::new(Filter::new(
        Box::new(SeqScan::new("users")),
        Expr::binary(op, Expr::Column(0), int(value)),
    ))
}

#[test]
fn statements_should_keep_tables_and_indexes_in_step_across_reopening() {
    let db = TempDb::new();
    {
        let (mut bpm, mut catalog) = db.open();
        create_users(&mut bpm, &mut catalog);
        // UPDATE users SET age = age + 1 WHERE id < 1000 (in place)
        let older = vec![(
            2,
            Expr::binary(
                BinaryOp::Add,
                Expr::Column(2),
                Expr::Literal(Value::SmallInt(1)),
            ),
        )];
        run(
            &mut bpm,
            &mut catalog,
            &mut Update::new("users", where_id(BinaryOp::Lt, 1_000), older),
        );
        // UPDATE users SET email = email || <300 bytes> WHERE id >= 2500 (rows move)
        let longer = vec![(
            1,
            Expr::binary(
                BinaryOp::Concat,
                Expr::Column(1),
                Expr::Literal(Value::Text("x".repeat(300))),
            ),
        )];
        run(
            &mut bpm,
            &mut catalog,
            &mut Update::new("users", where_id(BinaryOp::Ge, 2_500), longer),
        );
        // DELETE FROM users WHERE id % 7 = 0
        let multiples = Filter::new(
            Box::new(SeqScan::new("users")),
            Expr::binary(
                BinaryOp::Eq,
                Expr::binary(BinaryOp::Rem, Expr::Column(0), int(7)),
                int(0),
            ),
        );
        let deleted = run(
            &mut bpm,
            &mut catalog,
            &mut Delete::new("users", Box::new(multiples)),
        );
        assert_eq!(deleted[0].values, [Value::BigInt(429)]);
    }

    let (mut bpm, mut catalog) = db.open();
    let rows = scan(&mut bpm, &mut catalog);

    assert_eq!(rows.len(), 3_000 - 429);
    for row in &rows {
        let Value::Integer(id) = row.values[0] else {
            panic!("{row:?}")
        };
        assert_ne!(id % 7, 0);
        let expected_email = if id >= 2_500 {
            format!("{}{}", email(id), "x".repeat(300))
        } else {
            email(id)
        };
        assert_eq!(row.values[1], Value::Text(expected_email));
        let expected_age = match (id % 10, id < 1_000) {
            (0, _) => Value::Null,
            (_, true) => Value::SmallInt((id % 90) as i16 + 1),
            (_, false) => Value::SmallInt((id % 90) as i16),
        };
        assert_eq!(row.values[2], expected_age, "{id}");
    }
    assert_indexes_match_table(&mut bpm, &mut catalog);
}

#[test]
fn a_refused_statement_should_leave_no_trace_after_reopening() {
    let db = TempDb::new();
    let before = {
        let (mut bpm, mut catalog) = db.open();
        create_users(&mut bpm, &mut catalog);
        // A valid row first, then one whose email another row already has.
        let rows = vec![
            vec![
                int(5_000),
                Expr::Literal(Value::Text("new@example.com".to_string())),
                Expr::Literal(Value::Null),
            ],
            vec![
                int(5_001),
                Expr::Literal(Value::Text(email(7))),
                Expr::Literal(Value::Null),
            ],
        ];
        let mut insert = Insert::new("users", Box::new(Values::new(rows)));
        let err = execute(
            &mut insert,
            &mut ExecutionContext {
                catalog: &mut catalog,
                bpm: &mut bpm,
            },
        )
        .unwrap_err();
        assert_eq!(err.sqlstate(), "23505");
        scan(&mut bpm, &mut catalog)
    };

    let (mut bpm, mut catalog) = db.open();

    assert_eq!(scan(&mut bpm, &mut catalog), before);
    assert_eq!(before.len(), 3_000);
    assert_indexes_match_table(&mut bpm, &mut catalog);
}
