use std::fs;
use std::ops::Bound::{Included, Unbounded};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::Catalog;
use ferro_db::config::PAGE_SIZE;
use ferro_db::execution::{
    Aggregate, Aggregation, ExecutionContext, Executor, Expr, IndexScan, Insert, Limit, Row,
    SeqScan, Sort, SortKey, TopN, Values, execute,
};
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::table::{Column, Schema};
use ferro_db::types::{DataType, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-sort-it-{}-{id}.db", std::process::id())))
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

fn values(rows: Vec<Row>) -> Vec<Vec<Value>> {
    rows.into_iter().map(|row| row.values).collect()
}

fn col(column: usize) -> Expr {
    Expr::Column(column)
}

fn bigint(value: i64) -> Expr {
    Expr::Literal(Value::BigInt(value))
}

/// Every 9th player has no team, every 13th no score.
fn team(id: i32) -> Option<i16> {
    (id % 9 != 0).then_some((id % 5) as i16)
}

fn score(id: i32) -> Option<i32> {
    (id % 13 != 0).then_some((id * 31) % 100)
}

fn player(id: i32) -> Vec<Value> {
    vec![
        Value::Integer(id),
        team(id).map_or(Value::Null, Value::SmallInt),
        score(id).map_or(Value::Null, Value::Integer),
    ]
}

/// `players (id INTEGER NOT NULL, team SMALLINT, score INTEGER)` with an index on `id`,
/// filled in scattered id order, so heap order is not id order.
fn create_players(bpm: &mut BufferPoolManager, catalog: &mut Catalog) {
    let schema = Schema::new(vec![
        Column::not_null("id", DataType::Integer),
        Column::new("team", DataType::SmallInt),
        Column::new("score", DataType::Integer),
    ])
    .unwrap();
    catalog.create_table(bpm, "players", schema).unwrap();
    catalog
        .create_index(bpm, "players_id", "players", "id")
        .unwrap();
    let rows: Vec<Vec<Expr>> = (0..ROWS)
        .map(|i| (i * 7_919) % ROWS)
        .map(|id| player(id).into_iter().map(Expr::Literal).collect())
        .collect();
    run(
        bpm,
        catalog,
        &mut Insert::new("players", Box::new(Values::new(rows))),
    );
}

/// `ORDER BY team, score DESC, id`: NULL teams last, NULL scores first within a team.
fn by_team_then_best_score() -> Vec<SortKey> {
    vec![
        SortKey::asc(col(1)),
        SortKey::desc(col(2)),
        SortKey::asc(col(0)),
    ]
}

/// The players in the order of `by_team_then_best_score`, computed without the executors.
fn expected_by_team_then_best_score() -> Vec<Vec<Value>> {
    let mut ids: Vec<i32> = (0..ROWS).collect();
    ids.sort_by_key(|&id| {
        (
            team(id).is_none(),
            team(id),
            score(id).is_some(),
            std::cmp::Reverse(score(id)),
            id,
        )
    });
    ids.into_iter().map(player).collect()
}

#[test]
fn order_by_several_keys_should_give_the_same_rows_after_reopening() {
    let db = TempDb::new();
    {
        let (mut bpm, mut catalog) = db.open();
        create_players(&mut bpm, &mut catalog);
    }
    let mut results = Vec::new();

    for _ in 0..2 {
        let (mut bpm, mut catalog) = db.open();
        let plan = &mut Sort::new(Box::new(SeqScan::new("players")), by_team_then_best_score());
        results.push(run(&mut bpm, &mut catalog, plan));
    }

    assert_eq!(results[0], results[1]);
    assert_eq!(
        values(results.remove(0)),
        expected_by_team_then_best_score()
    );
}

#[test]
fn top_n_should_give_the_rows_of_limit_over_sort_on_a_whole_table() {
    let db = TempDb::new();
    let (mut bpm, mut catalog) = db.open();
    create_players(&mut bpm, &mut catalog);
    let keys = || vec![SortKey::desc(col(2)), SortKey::asc(col(0))];

    let from_top_n = run(
        &mut bpm,
        &mut catalog,
        &mut TopN::new(
            Box::new(SeqScan::new("players")),
            keys(),
            bigint(25),
            Some(bigint(100)),
        ),
    );
    let from_limit = run(
        &mut bpm,
        &mut catalog,
        &mut Limit::new(
            Box::new(Sort::new(Box::new(SeqScan::new("players")), keys())),
            Some(bigint(25)),
            Some(bigint(100)),
        ),
    );

    assert_eq!(from_top_n.len(), 25);
    assert_eq!(from_top_n, from_limit);
}

#[test]
fn limit_over_an_index_scan_should_give_the_first_keys_of_a_range() {
    let db = TempDb::new();
    let (mut bpm, mut catalog) = db.open();
    create_players(&mut bpm, &mut catalog);
    let from_1000 = IndexScan::new(
        "players_id",
        Included(Expr::Literal(Value::Integer(1_000))),
        Unbounded,
    );

    let rows = run(
        &mut bpm,
        &mut catalog,
        &mut Limit::new(Box::new(from_1000), Some(bigint(5)), Some(bigint(2))),
    );

    assert!(rows.iter().all(|row| row.rid.is_some()));
    assert_eq!(values(rows), (1_002..1_007).map(player).collect::<Vec<_>>());
}

#[test]
fn order_by_an_aggregate_should_sort_the_groups() {
    let db = TempDb::new();
    let (mut bpm, mut catalog) = db.open();
    create_players(&mut bpm, &mut catalog);
    // SELECT team, COUNT(*) FROM players GROUP BY team ORDER BY COUNT(*) DESC, team
    let groups = Aggregation::new(
        Box::new(SeqScan::new("players")),
        vec![col(1)],
        vec![Aggregate::CountStar],
    );

    let rows = run(
        &mut bpm,
        &mut catalog,
        &mut Sort::new(
            Box::new(groups),
            vec![SortKey::desc(col(1)), SortKey::asc(col(0))],
        ),
    );

    let mut expected: Vec<(Option<i16>, i64)> = (0..5)
        .map(Some)
        .chain([None])
        .map(|key| (key, (0..ROWS).filter(|&id| team(id) == key).count() as i64))
        .collect();
    expected.sort_by_key(|&(key, count)| (std::cmp::Reverse(count), key.is_none(), key));
    let expected: Vec<Vec<Value>> = expected
        .into_iter()
        .map(|(key, count)| {
            vec![
                key.map_or(Value::Null, Value::SmallInt),
                Value::BigInt(count),
            ]
        })
        .collect();
    assert_eq!(values(rows), expected);
}
