use std::fs;
use std::ops::Bound::Unbounded;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::Catalog;
use ferro_db::config::PAGE_SIZE;
use ferro_db::execution::{ExecutionContext, Expr, IndexScan, Row, SeqScan, execute};
use ferro_db::planner::{StatementOutcome, run_sql};
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::types::Value;

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-planner-it-{}-{id}.db", std::process::id())))
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

const ACCOUNTS: i32 = 3_000;

fn sql(bpm: &mut BufferPoolManager, catalog: &mut Catalog, text: &str) -> Vec<StatementOutcome> {
    run_sql(text, &mut ExecutionContext { catalog, bpm }).unwrap_or_else(|err| panic!("{err}"))
}

fn rows(
    bpm: &mut BufferPoolManager,
    catalog: &mut Catalog,
    root: &mut dyn ferro_db::execution::Executor,
) -> Vec<Row> {
    execute(root, &mut ExecutionContext { catalog, bpm }).unwrap()
}

/// Every 10th account has no owner.
fn owner(id: i32) -> Value {
    match id % 10 {
        0 => Value::Null,
        _ => Value::Text(format!("owner {id}")),
    }
}

fn create_and_fill(bpm: &mut BufferPoolManager, catalog: &mut Catalog) {
    let created = sql(
        bpm,
        catalog,
        "CREATE TABLE accounts (id INTEGER NOT NULL, owner VARCHAR(40), balance BIGINT); \
         CREATE UNIQUE INDEX accounts_id ON accounts (id)",
    );
    assert_eq!(
        created,
        [StatementOutcome::CreateTable, StatementOutcome::CreateIndex]
    );
    let ids: Vec<i32> = (0..ACCOUNTS).collect();
    for chunk in ids.chunks(100) {
        let values: Vec<String> = chunk
            .iter()
            .map(|id| match owner(*id) {
                Value::Text(name) => format!("({id}, '{name}', {})", id * 100),
                _ => format!("({id}, NULL, {})", id * 100),
            })
            .collect();
        let inserted = sql(
            bpm,
            catalog,
            &format!("INSERT INTO accounts VALUES {}", values.join(", ")),
        );
        assert_eq!(inserted, [StatementOutcome::Insert(chunk.len() as u64)]);
    }
}

/// What `accounts` holds after the updates and deletes of the test, by id.
fn expected_accounts() -> Vec<Vec<Value>> {
    (0..ACCOUNTS)
        .filter(|id| !(1_000..2_000).contains(id))
        .map(|id| {
            let raise = i64::from(id % 2 == 0);
            vec![
                Value::Integer(id),
                owner(id),
                Value::BigInt(i64::from(id) * 100 + raise),
            ]
        })
        .collect()
}

#[test]
fn sql_should_create_fill_and_change_a_table_that_survives_reopening() {
    let db = TempDb::new();
    {
        let (mut bpm, mut catalog) = db.open();
        create_and_fill(&mut bpm, &mut catalog);

        let changed = sql(
            &mut bpm,
            &mut catalog,
            "UPDATE accounts SET balance = balance + 1 WHERE id % 2 = 0; \
             DELETE FROM accounts WHERE id BETWEEN 1000 AND 1999",
        );

        assert_eq!(
            changed,
            [
                StatementOutcome::Update(1_500),
                StatementOutcome::Delete(1_000)
            ]
        );
    }
    let (mut bpm, mut catalog) = db.open();

    let mut scanned: Vec<Vec<Value>> = rows(&mut bpm, &mut catalog, &mut SeqScan::new("accounts"))
        .into_iter()
        .map(|row| row.values)
        .collect();
    let indexed = rows(
        &mut bpm,
        &mut catalog,
        &mut IndexScan::new("accounts_id", Unbounded, Unbounded),
    );
    let deleted = rows(
        &mut bpm,
        &mut catalog,
        &mut IndexScan::point("accounts_id", Expr::Literal(Value::Integer(1_500))),
    );

    scanned.sort_by(|lhs, rhs| lhs[0].sort_cmp(&rhs[0]));
    assert_eq!(scanned, expected_accounts());
    let indexed: Vec<Vec<Value>> = indexed.into_iter().map(|row| row.values).collect();
    assert_eq!(indexed, expected_accounts());
    assert!(deleted.is_empty());
}

#[test]
fn moving_keys_by_sql_should_keep_the_index_after_reopening() {
    let db = TempDb::new();
    {
        let (mut bpm, mut catalog) = db.open();
        create_and_fill(&mut bpm, &mut catalog);

        let moved = sql(
            &mut bpm,
            &mut catalog,
            "UPDATE accounts SET id = id + 10000 WHERE id < 500",
        );

        assert_eq!(moved, [StatementOutcome::Update(500)]);
    }
    let (mut bpm, mut catalog) = db.open();

    let keys: Vec<Value> = rows(
        &mut bpm,
        &mut catalog,
        &mut IndexScan::new("accounts_id", Unbounded, Unbounded),
    )
    .into_iter()
    .map(|row| row.values[0].clone())
    .collect();

    let expected: Vec<Value> = (500..ACCOUNTS)
        .chain(10_000..10_500)
        .map(Value::Integer)
        .collect();
    assert_eq!(keys, expected);
}

#[test]
fn an_error_midway_through_a_script_should_keep_the_statements_before_it() {
    let db = TempDb::new();
    {
        let (mut bpm, mut catalog) = db.open();
        sql(
            &mut bpm,
            &mut catalog,
            "CREATE TABLE t (id INTEGER NOT NULL); CREATE UNIQUE INDEX t_id ON t (id)",
        );

        let err = run_sql(
            "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2), (1); INSERT INTO t VALUES (3)",
            &mut ExecutionContext {
                catalog: &mut catalog,
                bpm: &mut bpm,
            },
        )
        .unwrap_err();

        assert_eq!(err.sqlstate(), "23505");
    }
    let (mut bpm, mut catalog) = db.open();

    let stored = rows(&mut bpm, &mut catalog, &mut SeqScan::new("t"));

    let values: Vec<Vec<Value>> = stored.into_iter().map(|row| row.values).collect();
    assert_eq!(values, [[Value::Integer(1)]]);
}
