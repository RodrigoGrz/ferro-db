use std::collections::BTreeMap;
use std::fs;
use std::ops::Bound::Included;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::Catalog;
use ferro_db::config::PAGE_SIZE;
use ferro_db::execution::{
    Aggregate, Aggregation, BinaryOp, Delete, ExecutionContext, Executor, Expr, Filter, IndexScan,
    Insert, SeqScan, Values, execute,
};
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::table::{Column, Schema};
use ferro_db::types::{DataType, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "ferro-aggregation-it-{}-{id}.db",
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

const ROWS: i32 = 4_000;

fn run(
    bpm: &mut BufferPoolManager,
    catalog: &mut Catalog,
    plan: &mut dyn Executor,
) -> Vec<Vec<Value>> {
    let rows = execute(plan, &mut ExecutionContext { catalog, bpm }).unwrap();
    rows.into_iter().map(|row| row.values).collect()
}

fn int(value: i32) -> Expr {
    Expr::Literal(Value::Integer(value))
}

fn col(column: usize) -> Expr {
    Expr::Column(column)
}

/// Every 11th order has no customer, every 13th no amount.
fn customer(id: i32) -> Option<i16> {
    (id % 11 != 0).then_some((id % 7) as i16)
}

fn amount(id: i32) -> Option<i32> {
    (id % 13 != 0).then_some(id % 100)
}

/// `orders (id INTEGER NOT NULL, customer SMALLINT, amount INTEGER)` with an index on `id`.
fn create_orders(bpm: &mut BufferPoolManager, catalog: &mut Catalog) {
    let schema = Schema::new(vec![
        Column::not_null("id", DataType::Integer),
        Column::new("customer", DataType::SmallInt),
        Column::new("amount", DataType::Integer),
    ])
    .unwrap();
    catalog.create_table(bpm, "orders", schema).unwrap();
    catalog
        .create_index(bpm, "orders_id", "orders", "id")
        .unwrap();
    let rows: Vec<Vec<Expr>> = (0..ROWS)
        .map(|id| {
            vec![
                int(id),
                Expr::Literal(customer(id).map_or(Value::Null, Value::SmallInt)),
                Expr::Literal(amount(id).map_or(Value::Null, Value::Integer)),
            ]
        })
        .collect();
    run(
        bpm,
        catalog,
        &mut Insert::new("orders", Box::new(Values::new(rows))),
    );
}

/// `SELECT customer, COUNT(*), COUNT(amount), SUM(amount), MIN(amount), MAX(amount)
/// FROM <child> GROUP BY customer`.
fn per_customer(child: Box<dyn Executor>) -> Aggregation {
    Aggregation::new(
        child,
        vec![col(1)],
        vec![
            Aggregate::CountStar,
            Aggregate::Count(col(2)),
            Aggregate::Sum(col(2)),
            Aggregate::Min(col(2)),
            Aggregate::Max(col(2)),
        ],
    )
}

fn sorted(mut groups: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    groups.sort_by(|lhs, rhs| lhs[0].sort_cmp(&rhs[0]));
    groups
}

/// What `per_customer` gives over the orders `ids`, computed without the executors.
fn expected_per_customer(ids: impl Iterator<Item = i32>) -> Vec<Vec<Value>> {
    let mut groups: BTreeMap<Option<i16>, (i64, Vec<i32>)> = BTreeMap::new();
    for id in ids {
        let (rows, amounts) = groups.entry(customer(id)).or_default();
        *rows += 1;
        amounts.extend(amount(id));
    }
    let groups = groups
        .into_iter()
        .map(|(customer, (rows, amounts))| {
            let sum = amounts.iter().map(|&amount| i64::from(amount)).sum::<i64>();
            let or_null = |value: Option<Value>| value.unwrap_or(Value::Null);
            vec![
                customer.map_or(Value::Null, Value::SmallInt),
                Value::BigInt(rows),
                Value::BigInt(amounts.len() as i64),
                or_null((!amounts.is_empty()).then_some(Value::BigInt(sum))),
                or_null(amounts.iter().min().map(|&min| Value::Integer(min))),
                or_null(amounts.iter().max().map(|&max| Value::Integer(max))),
            ]
        })
        .collect();
    sorted(groups)
}

#[test]
fn group_by_over_a_seq_scan_should_give_the_same_groups_after_reopening() {
    let db = TempDb::new();
    {
        let (mut bpm, mut catalog) = db.open();
        create_orders(&mut bpm, &mut catalog);
    }
    let mut results = Vec::new();

    for _ in 0..2 {
        let (mut bpm, mut catalog) = db.open();
        let plan = &mut per_customer(Box::new(SeqScan::new("orders")));
        results.push(sorted(run(&mut bpm, &mut catalog, plan)));
    }

    let expected = expected_per_customer(0..ROWS);
    assert_eq!(expected.len(), 8, "seven customers and NULL");
    assert_eq!(results[0], expected);
    assert_eq!(results[1], expected);
}

#[test]
fn group_by_over_an_index_scan_with_having_should_match_a_filtered_seq_scan() {
    let db = TempDb::new();
    let (mut bpm, mut catalog) = db.open();
    create_orders(&mut bpm, &mut catalog);
    // ... WHERE id BETWEEN 1000 AND 1999 GROUP BY customer HAVING SUM(amount) > 5940
    let having = |groups: Aggregation| {
        let sum_above = Expr::binary(BinaryOp::Gt, col(3), Expr::Literal(Value::BigInt(5_940)));
        Filter::new(Box::new(groups), sum_above)
    };
    let between = Expr::binary(
        BinaryOp::And,
        Expr::binary(BinaryOp::Ge, col(0), int(1_000)),
        Expr::binary(BinaryOp::Le, col(0), int(1_999)),
    );
    let by_index = IndexScan::new("orders_id", Included(int(1_000)), Included(int(1_999)));
    let by_scan = Filter::new(Box::new(SeqScan::new("orders")), between);

    let from_index = run(
        &mut bpm,
        &mut catalog,
        &mut having(per_customer(Box::new(by_index))),
    );
    let from_scan = run(
        &mut bpm,
        &mut catalog,
        &mut having(per_customer(Box::new(by_scan))),
    );

    let expected: Vec<Vec<Value>> = expected_per_customer(1_000..2_000)
        .into_iter()
        .filter(|group| matches!(group[3], Value::BigInt(sum) if sum > 5_940))
        .collect();
    assert!(!expected.is_empty() && expected.len() < 8, "{expected:?}");
    assert_eq!(sorted(from_index), expected);
    assert_eq!(sorted(from_scan), expected);
}

#[test]
fn a_delete_should_change_the_groups() {
    let db = TempDb::new();
    let (mut bpm, mut catalog) = db.open();
    create_orders(&mut bpm, &mut catalog);
    let customer_3 = Expr::binary(BinaryOp::Eq, col(1), Expr::Literal(Value::SmallInt(3)));
    let matching = Filter::new(Box::new(SeqScan::new("orders")), customer_3);
    run(
        &mut bpm,
        &mut catalog,
        &mut Delete::new("orders", Box::new(matching)),
    );

    let groups = run(
        &mut bpm,
        &mut catalog,
        &mut per_customer(Box::new(SeqScan::new("orders"))),
    );
    let total = run(
        &mut bpm,
        &mut catalog,
        &mut Aggregation::new(
            Box::new(SeqScan::new("orders")),
            vec![],
            vec![Aggregate::CountStar],
        ),
    );

    let remaining = || (0..ROWS).filter(|&id| customer(id) != Some(3));
    assert_eq!(sorted(groups), expected_per_customer(remaining()));
    assert_eq!(total, [[Value::BigInt(remaining().count() as i64)]]);
}
