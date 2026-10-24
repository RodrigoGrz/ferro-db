use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::Catalog;
use ferro_db::config::PAGE_SIZE;
use ferro_db::execution::ExecutionContext;
use ferro_db::planner::{PlannerError, StatementOutcome, run_sql};
use ferro_db::storage::buffer_pool::BufferPoolManager;

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "ferro-planner-select-it-{}-{id}.db",
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

fn run(
    bpm: &mut BufferPoolManager,
    catalog: &mut Catalog,
    sql: &str,
) -> Result<Vec<StatementOutcome>, PlannerError> {
    run_sql(sql, &mut ExecutionContext { catalog, bpm })
}

fn explain(bpm: &mut BufferPoolManager, catalog: &mut Catalog, sql: &str) -> String {
    match run(bpm, catalog, &format!("EXPLAIN {sql}")).as_deref() {
        Ok([StatementOutcome::Explain(text)]) => text.clone(),
        other => panic!("{sql}: {other:?}"),
    }
}

const SCHEMA: &str = "CREATE TABLE customers (id INTEGER NOT NULL, name VARCHAR(40), city VARCHAR(20)); \
     CREATE UNIQUE INDEX customers_id ON customers (id); \
     CREATE TABLE orders (id INTEGER NOT NULL, customer_id INTEGER, total BIGINT, placed DATE)";

const REPORT: &str = "SELECT c.city, count(*) AS orders, sum(o.total) \
     FROM customers c LEFT JOIN orders o ON o.customer_id = c.id \
     WHERE o.placed >= DATE '2024-01-01' \
     GROUP BY c.city HAVING sum(o.total) > 100 \
     ORDER BY 3 DESC, c.city LIMIT 10";

const REPORT_PLAN: &str = "Project: c.city AS city, count(*) AS orders, sum(o.total) AS sum\n\
     \x20 Limit: 10\n\
     \x20   Sort: sum(o.total) DESC NULLS FIRST, c.city ASC NULLS LAST\n\
     \x20     Filter: sum(o.total) > 100\n\
     \x20       Aggregate: group by c.city; count(*), sum(o.total)\n\
     \x20         Filter: o.placed >= DATE '2024-01-01'\n\
     \x20           Join LEFT: o.customer_id = c.id\n\
     \x20             Scan: customers AS c\n\
     \x20             Scan: orders AS o";

#[test]
fn explain_should_show_the_same_plan_after_reopening() {
    let db = TempDb::new();
    {
        let (mut bpm, mut catalog) = db.open();
        run(&mut bpm, &mut catalog, SCHEMA).unwrap();

        assert_eq!(explain(&mut bpm, &mut catalog, REPORT), REPORT_PLAN);
    }

    let (mut bpm, mut catalog) = db.open();

    assert_eq!(explain(&mut bpm, &mut catalog, REPORT), REPORT_PLAN);
}

#[test]
fn a_script_should_mix_explain_and_writes_in_order() {
    let db = TempDb::new();
    let (mut bpm, mut catalog) = db.open();

    let outcomes = run(
        &mut bpm,
        &mut catalog,
        &format!(
            "{SCHEMA}; INSERT INTO customers VALUES (1, 'ana', 'porto'), (2, 'rui', NULL); \
             EXPLAIN SELECT * FROM customers WHERE city IS NULL; \
             DELETE FROM customers WHERE city IS NULL"
        ),
    )
    .unwrap();

    assert_eq!(
        outcomes,
        [
            StatementOutcome::CreateTable,
            StatementOutcome::CreateIndex,
            StatementOutcome::CreateTable,
            StatementOutcome::Insert(2),
            StatementOutcome::Explain(
                "Project: customers.id AS id, customers.name AS name, customers.city AS city\n\
                 \x20 Filter: customers.city IS NULL\n\
                 \x20   Scan: customers"
                    .to_string()
            ),
            StatementOutcome::Delete(1),
        ]
    );
}

#[test]
fn an_invalid_query_should_fail_without_running_the_rest() {
    let db = TempDb::new();
    let (mut bpm, mut catalog) = db.open();
    run(&mut bpm, &mut catalog, SCHEMA).unwrap();

    let grouping = run(
        &mut bpm,
        &mut catalog,
        "EXPLAIN SELECT name, count(*) FROM customers GROUP BY city; \
         INSERT INTO customers VALUES (1, 'ana', 'porto')",
    )
    .unwrap_err();
    let ambiguous = run(
        &mut bpm,
        &mut catalog,
        "EXPLAIN SELECT id FROM customers, orders",
    )
    .unwrap_err();

    assert_eq!(
        grouping.to_string(),
        "column \"customers.name\" must appear in the GROUP BY clause or be used in an aggregate \
         function"
    );
    assert_eq!(ambiguous.sqlstate(), "42702");
    assert!(
        explain(&mut bpm, &mut catalog, "SELECT count(*) FROM customers").contains("Aggregate")
    );
    let inserted = run(
        &mut bpm,
        &mut catalog,
        "INSERT INTO customers VALUES (1, 'ana', 'porto')",
    );
    assert_eq!(inserted.unwrap(), [StatementOutcome::Insert(1)]);
}
