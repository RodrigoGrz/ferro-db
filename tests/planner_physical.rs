use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::Catalog;
use ferro_db::config::PAGE_SIZE;
use ferro_db::execution::ExecutionContext;
use ferro_db::planner::{StatementOutcome, run_sql};
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::types::Value;

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "ferro-planner-physical-it-{}-{id}.db",
            std::process::id()
        )))
    }

    /// The database with an 8-frame pool, so that a query evicts pages as it goes.
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

fn run(bpm: &mut BufferPoolManager, catalog: &mut Catalog, sql: &str) -> Vec<StatementOutcome> {
    run_sql(sql, &mut ExecutionContext { catalog, bpm }).unwrap_or_else(|err| panic!("{err}"))
}

fn rows(bpm: &mut BufferPoolManager, catalog: &mut Catalog, sql: &str) -> Vec<Vec<Value>> {
    match run(bpm, catalog, sql).as_slice() {
        [StatementOutcome::Rows { rows, .. }] => rows.clone(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn explain(bpm: &mut BufferPoolManager, catalog: &mut Catalog, sql: &str) -> String {
    match run(bpm, catalog, &format!("EXPLAIN {sql}")).as_slice() {
        [StatementOutcome::Explain(text)] => text.clone(),
        other => panic!("{sql}: {other:?}"),
    }
}

const CUSTOMERS: usize = 300;
const ORDERS: usize = 1_000;
const CITIES: [&str; 4] = ["lisboa", "porto", "braga", "faro"];

/// The customer of order `id`, if it has one.
fn customer_of(id: usize) -> Option<usize> {
    (!id.is_multiple_of(50)).then_some(id * 7 % CUSTOMERS)
}

fn total_of(id: usize) -> i64 {
    (id % 97) as i64
}

/// `customers (id, name, city)` with a unique index on `id`, and `orders (id, customer_id,
/// total)` without an index on `customer_id`.
fn fill(bpm: &mut BufferPoolManager, catalog: &mut Catalog) {
    run(
        bpm,
        catalog,
        "CREATE TABLE customers (id INTEGER NOT NULL, name VARCHAR(10), city VARCHAR(10)); \
         CREATE UNIQUE INDEX customers_id ON customers (id); \
         CREATE TABLE orders (id INTEGER NOT NULL, customer_id INTEGER, total BIGINT)",
    );
    for start in (0..CUSTOMERS).step_by(100) {
        let values: Vec<String> = (start..start + 100)
            .map(|id| format!("({id}, 'c{id}', '{}')", CITIES[id % CITIES.len()]))
            .collect();
        run(
            bpm,
            catalog,
            &format!("INSERT INTO customers VALUES {}", values.join(", ")),
        );
    }
    for start in (0..ORDERS).step_by(100) {
        let values: Vec<String> = (start..start + 100)
            .map(|id| {
                let customer = customer_of(id).map_or("NULL".to_string(), |c| c.to_string());
                format!("({id}, {customer}, {})", total_of(id))
            })
            .collect();
        run(
            bpm,
            catalog,
            &format!("INSERT INTO orders VALUES {}", values.join(", ")),
        );
    }
}

const BY_KEY: &str = "SELECT name FROM customers WHERE id = 42";
const REPORT: &str = "SELECT c.city, count(*), sum(o.total) FROM orders o \
     JOIN customers c ON c.id = o.customer_id GROUP BY c.city ORDER BY 1";
const TOP: &str = "SELECT o.id, o.total FROM orders o ORDER BY o.total DESC, o.id LIMIT 5";
const EVERY_CUSTOMER: &str =
    "SELECT count(*) FROM customers c LEFT JOIN orders o ON o.customer_id = c.id";

/// A filter on the indexed table of a join: pushed down to it, it is read through the index.
const ORDERS_OF_ONE: &str = "SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id \
     WHERE c.id = 42 ORDER BY o.id";

fn expected_orders_of_one() -> Vec<Vec<Value>> {
    (0..ORDERS)
        .filter(|id| customer_of(*id) == Some(42))
        .map(|id| vec![Value::Integer(id as i32)])
        .collect()
}

fn expected_report() -> Vec<Vec<Value>> {
    let mut cities = BTreeMap::<&str, (i64, i64)>::new();
    for id in 0..ORDERS {
        if let Some(customer) = customer_of(id) {
            let city = cities.entry(CITIES[customer % CITIES.len()]).or_default();
            city.0 += 1;
            city.1 += total_of(id);
        }
    }
    cities
        .into_iter()
        .map(|(city, (count, sum))| {
            vec![
                Value::Text(city.to_string()),
                Value::BigInt(count),
                Value::BigInt(sum),
            ]
        })
        .collect()
}

fn expected_top() -> Vec<Vec<Value>> {
    let mut orders: Vec<usize> = (0..ORDERS).collect();
    orders.sort_by_key(|id| (std::cmp::Reverse(total_of(*id)), *id));
    orders
        .into_iter()
        .take(5)
        .map(|id| vec![Value::Integer(id as i32), Value::BigInt(total_of(id))])
        .collect()
}

fn expected_every_customer() -> i64 {
    let mut orders_of = vec![0_i64; CUSTOMERS];
    for id in 0..ORDERS {
        if let Some(customer) = customer_of(id) {
            orders_of[customer] += 1;
        }
    }
    orders_of.into_iter().map(|count| count.max(1)).sum()
}

fn assert_queries(bpm: &mut BufferPoolManager, catalog: &mut Catalog) {
    assert_eq!(
        rows(bpm, catalog, BY_KEY),
        [vec![Value::Text("c42".to_string())]]
    );
    assert_eq!(rows(bpm, catalog, REPORT), expected_report());
    assert_eq!(rows(bpm, catalog, TOP), expected_top());
    assert_eq!(rows(bpm, catalog, ORDERS_OF_ONE), expected_orders_of_one());
    assert_eq!(
        rows(bpm, catalog, EVERY_CUSTOMER),
        [vec![Value::BigInt(expected_every_customer())]]
    );
}

#[test]
fn queries_should_give_their_rows_before_and_after_reopening() {
    let db = TempDb::new();
    {
        let (mut bpm, mut catalog) = db.open();
        fill(&mut bpm, &mut catalog);

        assert_queries(&mut bpm, &mut catalog);
    }

    let (mut bpm, mut catalog) = db.open();

    assert_queries(&mut bpm, &mut catalog);
}

#[test]
fn explain_should_show_the_index_where_one_serves_and_a_hash_join_elsewhere() {
    let db = TempDb::new();
    let (mut bpm, mut catalog) = db.open();
    fill(&mut bpm, &mut catalog);

    let by_key = explain(&mut bpm, &mut catalog, BY_KEY);
    let report = explain(&mut bpm, &mut catalog, REPORT);
    let top = explain(&mut bpm, &mut catalog, TOP);
    let every_customer = explain(&mut bpm, &mut catalog, EVERY_CUSTOMER);
    let orders_of_one = explain(&mut bpm, &mut catalog, ORDERS_OF_ONE);

    assert!(
        by_key.contains("IndexScan: customers (index customers_id = 42)"),
        "{by_key}"
    );
    assert!(
        report.contains(
            "IndexNestedLoopJoin INNER: c.id = o.customer_id (index customers_id ON customers AS c)"
        ),
        "{report}"
    );
    assert!(top.contains("TopN:"), "{top}");
    assert!(
        orders_of_one.contains("IndexScan: customers AS c (index customers_id = 42)"),
        "{orders_of_one}"
    );
    // The index is on the left table, which a LEFT join cannot look up.
    assert!(
        every_customer.contains("HashJoin LEFT: c.id = o.customer_id"),
        "{every_customer}"
    );
}

#[test]
fn a_select_in_a_script_should_see_the_writes_before_it() {
    let db = TempDb::new();
    let (mut bpm, mut catalog) = db.open();
    fill(&mut bpm, &mut catalog);

    let outcomes = run(
        &mut bpm,
        &mut catalog,
        "DELETE FROM orders WHERE customer_id = 42; \
         SELECT count(*) FROM orders o JOIN customers c ON c.id = o.customer_id WHERE c.id = 42; \
         UPDATE customers SET name = 'renamed' WHERE id = 42; \
         SELECT name FROM customers WHERE id = 42",
    );

    let deleted = (0..ORDERS)
        .filter(|id| customer_of(*id) == Some(42))
        .count() as u64;
    assert_eq!(
        outcomes,
        [
            StatementOutcome::Delete(deleted),
            StatementOutcome::Rows {
                columns: vec!["count".to_string()],
                rows: vec![vec![Value::BigInt(0)]],
            },
            StatementOutcome::Update(1),
            StatementOutcome::Rows {
                columns: vec!["name".to_string()],
                rows: vec![vec![Value::Text("renamed".to_string())]],
            },
        ]
    );
}
