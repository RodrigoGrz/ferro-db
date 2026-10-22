use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::Catalog;
use ferro_db::config::PAGE_SIZE;
use ferro_db::execution::{
    Aggregate, Aggregation, BinaryOp, Delete, ExecutionContext, Executor, Expr, Filter,
    IndexNestedLoopJoin, Insert, JoinType, Row, SeqScan, Sort, SortKey, Values, execute,
};
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::table::{Column, Schema};
use ferro_db::types::{DataType, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-join-it-{}-{id}.db", std::process::id())))
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

const ORDERS: i32 = 4_000;
const CUSTOMER_IDS: i32 = 300;

fn run(bpm: &mut BufferPoolManager, catalog: &mut Catalog, plan: &mut dyn Executor) -> Vec<Row> {
    execute(plan, &mut ExecutionContext { catalog, bpm }).unwrap()
}

fn col(column: usize) -> Expr {
    Expr::Column(column)
}

fn int(value: i32) -> Expr {
    Expr::Literal(Value::Integer(value))
}

/// Customer ids `id % 7 == 3` do not exist.
fn customer_exists(id: i32) -> bool {
    (0..CUSTOMER_IDS).contains(&id) && id % 7 != 3
}

/// Every 5th customer has no region.
fn customer(id: i32) -> Vec<Value> {
    vec![
        Value::Integer(id),
        Value::Text(format!("c{id}")),
        if id % 5 == 0 {
            Value::Null
        } else {
            Value::SmallInt((id % 4) as i16)
        },
    ]
}

/// Every 17th order has no customer; some point past the last customer id.
fn order(id: i32) -> Vec<Value> {
    vec![
        Value::Integer(id),
        if id % 17 == 0 {
            Value::Null
        } else {
            Value::Integer(id % 330)
        },
        Value::Integer(id % 100),
    ]
}

fn insert(bpm: &mut BufferPoolManager, catalog: &mut Catalog, table: &str, rows: Vec<Vec<Value>>) {
    let rows: Vec<Vec<Expr>> = rows
        .into_iter()
        .map(|row| row.into_iter().map(Expr::Literal).collect())
        .collect();
    run(
        bpm,
        catalog,
        &mut Insert::new(table, Box::new(Values::new(rows))),
    );
}

/// `customers (id INTEGER NOT NULL, name VARCHAR(40), region SMALLINT)` indexed on `id`,
/// and `orders (id INTEGER NOT NULL, customer_id INTEGER, amount INTEGER)`.
fn create_tables(bpm: &mut BufferPoolManager, catalog: &mut Catalog) {
    let customers = Schema::new(vec![
        Column::not_null("id", DataType::Integer),
        Column::new("name", DataType::VARCHAR_MAX),
        Column::new("region", DataType::SmallInt),
    ])
    .unwrap();
    let orders = Schema::new(vec![
        Column::not_null("id", DataType::Integer),
        Column::new("customer_id", DataType::Integer),
        Column::new("amount", DataType::Integer),
    ])
    .unwrap();
    catalog.create_table(bpm, "customers", customers).unwrap();
    catalog
        .create_index(bpm, "customers_id", "customers", "id")
        .unwrap();
    catalog.create_table(bpm, "orders", orders).unwrap();
    let customer_rows = (0..CUSTOMER_IDS)
        .filter(|&id| customer_exists(id))
        .map(customer)
        .collect();
    insert(bpm, catalog, "customers", customer_rows);
    insert(bpm, catalog, "orders", (0..ORDERS).map(order).collect());
}

fn orders_join_customers(join_type: JoinType) -> IndexNestedLoopJoin {
    IndexNestedLoopJoin::new(
        Box::new(SeqScan::new("orders")),
        col(1),
        "customers_id",
        join_type,
        None,
    )
}

fn sorted_values(rows: Vec<Row>) -> Vec<Vec<Value>> {
    let mut values: Vec<Vec<Value>> = rows.into_iter().map(|row| row.values).collect();
    values.sort_by(|lhs, rhs| lhs[0].sort_cmp(&rhs[0]));
    values
}

/// The join computed without the executors, by order id.
fn expected_join(join_type: JoinType, exists: impl Fn(i32) -> bool) -> Vec<Vec<Value>> {
    (0..ORDERS)
        .filter_map(|id| {
            let order = order(id);
            let matched = match order[1] {
                Value::Integer(customer_id) if exists(customer_id) => Some(customer(customer_id)),
                _ => None,
            };
            match (matched, join_type) {
                (Some(customer), _) => Some([order, customer].concat()),
                (None, JoinType::Left) => Some([order, vec![Value::Null; 3]].concat()),
                (None, JoinType::Inner) => None,
            }
        })
        .collect()
}

#[test]
fn inner_and_left_joins_should_match_a_model_after_reopening() {
    let db = TempDb::new();
    {
        let (mut bpm, mut catalog) = db.open();
        create_tables(&mut bpm, &mut catalog);
    }
    let (mut bpm, mut catalog) = db.open();

    let inner = run(
        &mut bpm,
        &mut catalog,
        &mut orders_join_customers(JoinType::Inner),
    );
    let left = run(
        &mut bpm,
        &mut catalog,
        &mut orders_join_customers(JoinType::Left),
    );

    let expected_inner = expected_join(JoinType::Inner, customer_exists);
    assert!(expected_inner.len() > 2_500 && expected_inner.len() < ORDERS as usize);
    assert_eq!(sorted_values(inner), expected_inner);
    assert_eq!(left.len(), ORDERS as usize);
    assert_eq!(
        sorted_values(left),
        expected_join(JoinType::Left, customer_exists)
    );
}

#[test]
fn a_join_should_feed_an_aggregation_and_a_sort() {
    let db = TempDb::new();
    let (mut bpm, mut catalog) = db.open();
    create_tables(&mut bpm, &mut catalog);
    // SELECT c.region, SUM(o.amount) FROM orders o JOIN customers c ON o.customer_id = c.id
    // GROUP BY c.region ORDER BY SUM(o.amount) DESC
    let revenue = Aggregation::new(
        Box::new(orders_join_customers(JoinType::Inner)),
        vec![col(5)],
        vec![Aggregate::Sum(col(2))],
    );
    let mut plan = Sort::new(Box::new(revenue), vec![SortKey::desc(col(1))]);

    let rows = run(&mut bpm, &mut catalog, &mut plan);

    let mut expected: BTreeMap<Option<i16>, i64> = BTreeMap::new();
    for joined in expected_join(JoinType::Inner, customer_exists) {
        let region = match joined[5] {
            Value::SmallInt(region) => Some(region),
            _ => None,
        };
        let Value::Integer(amount) = joined[2] else {
            panic!("{joined:?}")
        };
        *expected.entry(region).or_default() += i64::from(amount);
    }
    let mut expected: Vec<Vec<Value>> = expected
        .into_iter()
        .map(|(region, sum)| {
            vec![
                region.map_or(Value::Null, Value::SmallInt),
                Value::BigInt(sum),
            ]
        })
        .collect();
    expected.sort_by(|lhs, rhs| rhs[1].sort_cmp(&lhs[1]));
    let values: Vec<Vec<Value>> = rows.into_iter().map(|row| row.values).collect();
    assert_eq!(values, expected);
}

#[test]
fn a_delete_of_customers_should_change_the_join() {
    let db = TempDb::new();
    let (mut bpm, mut catalog) = db.open();
    create_tables(&mut bpm, &mut catalog);
    let below_100 = Expr::binary(BinaryOp::Lt, col(0), int(100));
    let matching = Filter::new(Box::new(SeqScan::new("customers")), below_100);
    run(
        &mut bpm,
        &mut catalog,
        &mut Delete::new("customers", Box::new(matching)),
    );

    let inner = run(
        &mut bpm,
        &mut catalog,
        &mut orders_join_customers(JoinType::Inner),
    );
    let left = run(
        &mut bpm,
        &mut catalog,
        &mut orders_join_customers(JoinType::Left),
    );

    let remaining = |id: i32| customer_exists(id) && id >= 100;
    assert_eq!(
        sorted_values(inner),
        expected_join(JoinType::Inner, remaining)
    );
    assert_eq!(
        sorted_values(left),
        expected_join(JoinType::Left, remaining)
    );
}
