use std::fs;
use std::num::NonZeroU16;
use std::ops::Bound::{self, Excluded, Included, Unbounded};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::Catalog;
use ferro_db::config::PAGE_SIZE;
use ferro_db::execution::{
    BinaryOp, ExecutionContext, Executor, Expr, Filter, IndexScan, Insert, Row, SeqScan, Update,
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
        Self(std::env::temp_dir().join(format!(
            "ferro-index-scan-it-{}-{id}.db",
            std::process::id()
        )))
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

const ROWS: i32 = 3_000;

fn run(bpm: &mut BufferPoolManager, catalog: &mut Catalog, plan: &mut dyn Executor) -> Vec<Row> {
    execute(plan, &mut ExecutionContext { catalog, bpm }).unwrap()
}

fn int(value: i32) -> Expr {
    Expr::Literal(Value::Integer(value))
}

fn text(value: &str) -> Expr {
    Expr::Literal(Value::Text(value.to_string()))
}

fn email(id: i32) -> String {
    format!("user{id}@example.com")
}

/// `users (id INTEGER NOT NULL, email VARCHAR(400))` with unique indexes on `id` and `email`,
/// filled with 3,000 rows in scattered id order, so heap order is not key order.
fn create_users(bpm: &mut BufferPoolManager, catalog: &mut Catalog) {
    let schema = Schema::new(vec![
        Column::not_null("id", DataType::Integer),
        Column::new("email", DataType::Varchar(NonZeroU16::new(400).unwrap())),
    ])
    .unwrap();
    catalog.create_table(bpm, "users", schema).unwrap();
    catalog
        .create_index(bpm, "users_id", "users", "id")
        .unwrap();
    catalog
        .create_index(bpm, "users_email", "users", "email")
        .unwrap();
    let rows: Vec<Vec<Expr>> = (0..ROWS)
        .map(|i| (i * 7_919) % ROWS)
        .map(|id| vec![int(id), text(&email(id))])
        .collect();
    run(
        bpm,
        catalog,
        &mut Insert::new("users", Box::new(Values::new(rows))),
    );
}

fn index_scan(
    bpm: &mut BufferPoolManager,
    catalog: &mut Catalog,
    index: &str,
    from: Bound<Expr>,
    to: Bound<Expr>,
) -> Vec<Row> {
    run(bpm, catalog, &mut IndexScan::new(index, from, to))
}

/// The same rows through `Filter(SeqScan)`, sorted by the column.
fn filtered(
    bpm: &mut BufferPoolManager,
    catalog: &mut Catalog,
    column: usize,
    predicate: Expr,
) -> Vec<Row> {
    let mut rows = run(
        bpm,
        catalog,
        &mut Filter::new(Box::new(SeqScan::new("users")), predicate),
    );
    rows.sort_by(|lhs, rhs| lhs.values[column].sort_cmp(&rhs.values[column]));
    rows
}

fn compare(column: usize, op: BinaryOp, value: Expr) -> Expr {
    Expr::binary(op, Expr::Column(column), value)
}

fn ids(rows: &[Row]) -> Vec<i32> {
    rows.iter()
        .map(|row| match row.values[0] {
            Value::Integer(id) => id,
            ref other => panic!("{other:?}"),
        })
        .collect()
}

/// Each index holds exactly the keys of its column, with their rows' `Rid`s.
fn assert_indexes_match_table(bpm: &mut BufferPoolManager, catalog: &mut Catalog) {
    let rows = run(bpm, catalog, &mut SeqScan::new("users"));
    for (name, column) in [("users_id", 0), ("users_email", 1)] {
        let mut expected: Vec<(Value, Rid)> = rows
            .iter()
            .map(|row| (row.values[column].clone(), row.rid.unwrap()))
            .collect();
        expected.sort_by(|lhs, rhs| lhs.0.sort_cmp(&rhs.0));
        let tree = catalog.index_tree(bpm, name).unwrap();
        let found: Vec<(Value, Rid)> = tree
            .range(bpm, Unbounded, Unbounded)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect();
        assert_eq!(found, expected, "{name}");
    }
}

#[test]
fn index_scans_should_find_points_and_ranges_after_reopening() {
    let db = TempDb::new();
    {
        let (mut bpm, mut catalog) = db.open();
        create_users(&mut bpm, &mut catalog);
    }
    let (mut bpm, mut catalog) = db.open();
    let (bpm, catalog) = (&mut bpm, &mut catalog);

    let by_id = run(bpm, catalog, &mut IndexScan::point("users_id", int(1_234)));
    let by_email = run(
        bpm,
        catalog,
        &mut IndexScan::point("users_email", text(&email(2_718))),
    );
    let id_range = index_scan(
        bpm,
        catalog,
        "users_id",
        Included(int(1_000)),
        Included(int(1_099)),
    );
    let email_range = index_scan(
        bpm,
        catalog,
        "users_email",
        Included(text("user1")),
        Excluded(text("user2")),
    );
    let everything = index_scan(bpm, catalog, "users_id", Unbounded, Unbounded);

    assert_eq!(
        by_id,
        filtered(bpm, catalog, 0, compare(0, BinaryOp::Eq, int(1_234)))
    );
    assert_eq!(ids(&by_email), [2_718]);
    assert_eq!(ids(&id_range), (1_000..1_100).collect::<Vec<_>>());
    let between = Expr::binary(
        BinaryOp::And,
        compare(0, BinaryOp::Ge, int(1_000)),
        compare(0, BinaryOp::Le, int(1_099)),
    );
    assert_eq!(id_range, filtered(bpm, catalog, 0, between));
    // user1, user10..user19, user100..user199 and user1000..user1999.
    assert_eq!(email_range.len(), 1_111);
    let starts_with_1 = Expr::binary(
        BinaryOp::And,
        compare(1, BinaryOp::Ge, text("user1")),
        compare(1, BinaryOp::Lt, text("user2")),
    );
    assert_eq!(email_range, filtered(bpm, catalog, 1, starts_with_1));
    assert_eq!(ids(&everything), (0..ROWS).collect::<Vec<_>>());
}

#[test]
fn update_through_the_index_should_keep_table_and_indexes_in_step_after_reopening() {
    let db = TempDb::new();
    {
        let (mut bpm, mut catalog) = db.open();
        create_users(&mut bpm, &mut catalog);
        // UPDATE users SET id = id + 10000, email = email || <300 bytes>
        //   WHERE id BETWEEN 1000 AND 1999, read through users_id: the keys move past the
        //   scan's cursor and the rows outgrow their pages.
        let scan = IndexScan::new("users_id", Included(int(1_000)), Included(int(1_999)));
        let assignments = vec![
            (0, compare(0, BinaryOp::Add, int(10_000))),
            (1, compare(1, BinaryOp::Concat, text(&"x".repeat(300)))),
        ];
        let updated = run(
            &mut bpm,
            &mut catalog,
            &mut Update::new("users", Box::new(scan), assignments),
        );
        assert_eq!(updated[0].values, [Value::BigInt(1_000)]);
    }

    let (mut bpm, mut catalog) = db.open();
    let (bpm, catalog) = (&mut bpm, &mut catalog);
    let moved = index_scan(bpm, catalog, "users_id", Included(int(10_000)), Unbounded);
    let old_range = index_scan(
        bpm,
        catalog,
        "users_id",
        Included(int(1_000)),
        Included(int(1_999)),
    );

    assert_eq!(ids(&moved), (11_000..12_000).collect::<Vec<_>>());
    for row in &moved {
        let id = ids(std::slice::from_ref(row))[0] - 10_000;
        let expected = format!("{}{}", email(id), "x".repeat(300));
        assert_eq!(row.values[1], Value::Text(expected));
    }
    assert!(old_range.is_empty());
    assert_eq!(
        index_scan(bpm, catalog, "users_id", Unbounded, Unbounded).len(),
        ROWS as usize
    );
    assert_indexes_match_table(bpm, catalog);
}
