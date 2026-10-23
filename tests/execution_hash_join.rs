use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::Catalog;
use ferro_db::config::PAGE_SIZE;
use ferro_db::execution::{
    Aggregate, Aggregation, ExecutionContext, Executor, Expr, HashJoin, IndexNestedLoopJoin,
    Insert, JoinInput, JoinType, Row, SeqScan, Values, execute,
};
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::table::{Column, Schema};
use ferro_db::types::{DataType, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(
            std::env::temp_dir().join(format!("ferro-hash-join-it-{}-{id}.db", std::process::id())),
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

const CUSTOMERS: i32 = 400;
const ORDERS: i32 = 3_000;
/// Regions 1 to 5 exist; customers live in regions 0 to 3, so region 0 has no row and
/// regions 4 and 5 have no customer.
const REGION_CODES: std::ops::RangeInclusive<i64> = 1..=5;

fn run(bpm: &mut BufferPoolManager, catalog: &mut Catalog, plan: &mut dyn Executor) -> Vec<Row> {
    execute(plan, &mut ExecutionContext { catalog, bpm }).unwrap()
}

fn values(rows: Vec<Row>) -> Vec<Vec<Value>> {
    rows.into_iter().map(|row| row.values).collect()
}

fn col(column: usize) -> Expr {
    Expr::Column(column)
}

/// Every 9th customer has no region.
fn customer(id: i32) -> Vec<Value> {
    vec![
        Value::Integer(id),
        Value::Text(format!("c{id}")),
        if id % 9 == 0 {
            Value::Null
        } else {
            Value::SmallInt((id % 4) as i16)
        },
    ]
}

/// Every 17th order has no customer; some point past the last customer.
fn order(id: i32) -> Vec<Value> {
    vec![
        Value::Integer(id),
        if id % 17 == 0 {
            Value::Null
        } else {
            Value::Integer(id % 450)
        },
        Value::Integer(id % 100),
    ]
}

fn region(code: i64) -> Vec<Value> {
    vec![Value::BigInt(code), Value::Text(format!("region {code}"))]
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

/// `customers (id INTEGER NOT NULL, name VARCHAR, region SMALLINT)` indexed on `id`,
/// `orders (id INTEGER NOT NULL, customer_id INTEGER, amount INTEGER)` and
/// `regions (code BIGINT NOT NULL, name VARCHAR)`, which has no index.
fn create_tables(bpm: &mut BufferPoolManager, catalog: &mut Catalog) {
    let schemas = [
        (
            "customers",
            vec![
                Column::not_null("id", DataType::Integer),
                Column::new("name", DataType::VARCHAR_MAX),
                Column::new("region", DataType::SmallInt),
            ],
        ),
        (
            "orders",
            vec![
                Column::not_null("id", DataType::Integer),
                Column::new("customer_id", DataType::Integer),
                Column::new("amount", DataType::Integer),
            ],
        ),
        (
            "regions",
            vec![
                Column::not_null("code", DataType::BigInt),
                Column::new("name", DataType::VARCHAR_MAX),
            ],
        ),
    ];
    for (name, columns) in schemas {
        catalog
            .create_table(bpm, name, Schema::new(columns).unwrap())
            .unwrap();
    }
    catalog
        .create_index(bpm, "customers_id", "customers", "id")
        .unwrap();
    insert(
        bpm,
        catalog,
        "customers",
        (0..CUSTOMERS).map(customer).collect(),
    );
    insert(bpm, catalog, "orders", (0..ORDERS).map(order).collect());
    insert(bpm, catalog, "regions", REGION_CODES.map(region).collect());
}

fn scan(table: &str, width: usize) -> JoinInput {
    JoinInput {
        child: Box::new(SeqScan::new(table)),
        width,
    }
}

/// `customers c <join_type> regions r ON c.region = r.code` (SMALLINT against BIGINT).
fn customers_join_regions(join_type: JoinType) -> HashJoin {
    HashJoin::new(
        scan("customers", 3),
        scan("regions", 2),
        vec![(col(2), col(0))],
        join_type,
        None,
    )
}

/// The same join computed without the executors.
fn expected_customers_join_regions(join_type: JoinType) -> Vec<Vec<Value>> {
    let region_of = |customer: &[Value]| match customer[2] {
        Value::SmallInt(region) if REGION_CODES.contains(&i64::from(region)) => {
            Some(i64::from(region))
        }
        _ => None,
    };
    let mut output = Vec::new();
    for customer in (0..CUSTOMERS).map(customer) {
        match region_of(&customer) {
            Some(code) => output.push([customer, region(code)].concat()),
            None if matches!(join_type, JoinType::Left | JoinType::Full) => {
                output.push([customer, vec![Value::Null; 2]].concat());
            }
            None => {}
        }
    }
    if matches!(join_type, JoinType::Right | JoinType::Full) {
        let has_customer =
            |code: i64| (0..CUSTOMERS).any(|id| region_of(&customer(id)) == Some(code));
        for code in REGION_CODES.filter(|&code| !has_customer(code)) {
            output.push([vec![Value::Null; 3], region(code)].concat());
        }
    }
    output
}

#[test]
fn every_join_type_on_a_column_without_index_should_match_a_model_after_reopening() {
    let db = TempDb::new();
    {
        let (mut bpm, mut catalog) = db.open();
        create_tables(&mut bpm, &mut catalog);
    }
    let (mut bpm, mut catalog) = db.open();

    for join_type in [
        JoinType::Inner,
        JoinType::Left,
        JoinType::Right,
        JoinType::Full,
    ] {
        let rows = run(
            &mut bpm,
            &mut catalog,
            &mut customers_join_regions(join_type),
        );

        assert_eq!(
            values(rows),
            expected_customers_join_regions(join_type),
            "{join_type:?}"
        );
    }
}

#[test]
fn a_hash_join_should_equal_the_index_join_where_there_is_an_index() {
    let db = TempDb::new();
    let (mut bpm, mut catalog) = db.open();
    create_tables(&mut bpm, &mut catalog);

    for join_type in [JoinType::Inner, JoinType::Left] {
        let mut by_hash = HashJoin::new(
            scan("orders", 3),
            scan("customers", 3),
            vec![(col(1), col(0))],
            join_type,
            None,
        );
        let mut by_index = IndexNestedLoopJoin::new(
            Box::new(SeqScan::new("orders")),
            col(1),
            "customers_id",
            join_type,
            None,
        );

        let from_hash = run(&mut bpm, &mut catalog, &mut by_hash);
        let from_index = run(&mut bpm, &mut catalog, &mut by_index);

        assert!(from_hash.len() > 2_000, "{join_type:?}");
        assert_eq!(from_hash, from_index, "{join_type:?}");
    }
}

#[test]
fn hash_joins_should_feed_each_other_and_an_aggregation() {
    let db = TempDb::new();
    let (mut bpm, mut catalog) = db.open();
    create_tables(&mut bpm, &mut catalog);
    // SELECT r.name, COUNT(*) FROM orders o JOIN customers c ON o.customer_id = c.id
    //   JOIN regions r ON c.region = r.code GROUP BY r.name
    let orders_customers = HashJoin::new(
        scan("orders", 3),
        scan("customers", 3),
        vec![(col(1), col(0))],
        JoinType::Inner,
        None,
    );
    let with_regions = HashJoin::new(
        JoinInput {
            child: Box::new(orders_customers),
            width: 6,
        },
        scan("regions", 2),
        vec![(col(5), col(0))],
        JoinType::Inner,
        None,
    );
    let mut per_region = Aggregation::new(
        Box::new(with_regions),
        vec![col(7)],
        vec![Aggregate::CountStar],
    );

    let rows = run(&mut bpm, &mut catalog, &mut per_region);

    let mut expected: BTreeMap<String, i64> = BTreeMap::new();
    for order in (0..ORDERS).map(order) {
        let Value::Integer(customer_id) = order[1] else {
            continue;
        };
        if !(0..CUSTOMERS).contains(&customer_id) {
            continue;
        }
        if let Value::SmallInt(code) = customer(customer_id)[2]
            && REGION_CODES.contains(&i64::from(code))
        {
            *expected.entry(format!("region {code}")).or_default() += 1;
        }
    }
    let mut actual: Vec<(String, i64)> = values(rows)
        .into_iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::Text(name), Value::BigInt(count)) => (name.clone(), *count),
            other => panic!("{other:?}"),
        })
        .collect();
    actual.sort();
    assert_eq!(actual, expected.into_iter().collect::<Vec<_>>());
}
