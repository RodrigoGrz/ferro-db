//! Leaf executors: `Values` produces rows from expressions, `SeqScan` reads a table and
//! `IndexScan` reads it through an index.

use std::ops::Bound;

use crate::storage::rid::Rid;
use crate::types::Value;

use super::key_bounds::{Side, key_bound};
use super::{ExecutionContext, ExecutionError, Executor, Expr, Row};

/// The rows of an index's table whose key is within the bounds, in key order, each with its
/// `Rid`. The bounds are expressions without columns, evaluated on the first call and
/// converted to the key's type (`key_bounds`). The cursor is the last key returned: every
/// call asks the tree for the first entry after it, so no page stays pinned and no leaf is
/// held that a merge could free between calls. An unknown index is `UndefinedObject` (42704);
/// a `Rid` the table does not have, an index out of step with its table, is `XX000`.
pub struct IndexScan {
    index: String,
    state: IndexScanState,
}

enum IndexScanState {
    Start {
        from: Bound<Expr>,
        to: Bound<Expr>,
    },
    Scanning {
        table: String,
        lower: Bound<Value>,
        upper: Bound<Value>,
    },
    Done,
}

impl IndexScan {
    pub fn new(index: &str, from: Bound<Expr>, to: Bound<Expr>) -> Self {
        Self {
            index: index.to_string(),
            state: IndexScanState::Start { from, to },
        }
    }

    /// `key = value`: a range of one key.
    pub fn point(index: &str, key: Expr) -> Self {
        Self::new(index, Bound::Included(key.clone()), Bound::Included(key))
    }

    /// `Scanning` with the index's table and the bounds in the key's type, or `Done` when the
    /// bounds alone make the range empty.
    fn start(
        &self,
        ctx: &mut ExecutionContext<'_>,
        from: Bound<Expr>,
        to: Bound<Expr>,
    ) -> Result<IndexScanState, ExecutionError> {
        let from = eval_bound(from)?;
        let to = eval_bound(to)?;
        let table = ctx.catalog.index_table(&self.index)?.name.clone();
        let key_type = ctx.catalog.index_tree(ctx.bpm, &self.index)?.key_type();
        let (Some(lower), Some(upper)) = (
            key_bound(from, key_type, Side::Lower)?,
            key_bound(to, key_type, Side::Upper)?,
        ) else {
            return Ok(IndexScanState::Done);
        };
        Ok(IndexScanState::Scanning {
            table,
            lower,
            upper,
        })
    }
}

fn eval_bound(bound: Bound<Expr>) -> Result<Bound<Value>, ExecutionError> {
    Ok(match bound {
        Bound::Included(expr) => Bound::Included(expr.eval(&[])?),
        Bound::Excluded(expr) => Bound::Excluded(expr.eval(&[])?),
        Bound::Unbounded => Bound::Unbounded,
    })
}

impl Executor for IndexScan {
    fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError> {
        // `Done` until the next row is in hand, so an error also ends the scan.
        let state = match std::mem::replace(&mut self.state, IndexScanState::Done) {
            IndexScanState::Start { from, to } => self.start(ctx, from, to)?,
            state => state,
        };
        let IndexScanState::Scanning {
            table,
            lower,
            upper,
        } = state
        else {
            return Ok(None);
        };
        let tree = ctx.catalog.index_tree(ctx.bpm, &self.index)?;
        let Some(entry) = tree.range(ctx.bpm, lower.as_ref(), upper.as_ref())?.next() else {
            return Ok(None);
        };
        let (key, rid) = entry?;
        let values = ctx.catalog.table_heap(ctx.bpm, &table)?.get(ctx.bpm, rid)?;
        self.state = IndexScanState::Scanning {
            table,
            lower: Bound::Excluded(key),
            upper,
        };
        Ok(Some(Row {
            values,
            rid: Some(rid),
        }))
    }
}

/// The rows of a table in heap order, each with its `Rid`. A cursor (the last `Rid` read)
/// replaces an open scan, so no page stays pinned between calls; the table is looked up on
/// the first call (`UndefinedTable`, 42P01, if it does not exist).
pub struct SeqScan {
    table: String,
    after: Option<Rid>,
    is_done: bool,
}

impl SeqScan {
    pub fn new(table: &str) -> Self {
        Self {
            table: table.to_string(),
            after: None,
            is_done: false,
        }
    }
}

impl Executor for SeqScan {
    fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError> {
        if self.is_done {
            return Ok(None);
        }
        let heap = ctx.catalog.table_heap(ctx.bpm, &self.table)?;
        match heap.next_after(ctx.bpm, self.after)? {
            None => {
                self.is_done = true;
                Ok(None)
            }
            Some((rid, values)) => {
                self.after = Some(rid);
                Ok(Some(Row {
                    values,
                    rid: Some(rid),
                }))
            }
        }
    }
}

/// Rows of expressions with no columns to read (`VALUES (1, 'a'), (2, 'b')`).
pub struct Values {
    rows: std::vec::IntoIter<Vec<Expr>>,
}

impl Values {
    pub fn new(rows: Vec<Vec<Expr>>) -> Self {
        Self {
            rows: rows.into_iter(),
        }
    }
}

impl Executor for Values {
    fn next(&mut self, _: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError> {
        let Some(exprs) = self.rows.next() else {
            return Ok(None);
        };
        let values = exprs
            .iter()
            .map(|expr| expr.eval(&[]))
            .collect::<Result<_, _>>()?;
        Ok(Some(Row { values, rid: None }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::execute;
    use crate::execution::test_support::TestDb;
    use crate::types::Value;

    fn int(value: i32) -> Expr {
        Expr::Literal(Value::Integer(value))
    }

    use crate::table::{Column, Schema};
    use crate::types::DataType;

    fn accounts() -> Schema {
        Schema::new(vec![
            Column::not_null("id", DataType::Integer),
            Column::new("name", DataType::VARCHAR_MAX),
        ])
        .unwrap()
    }

    fn account(id: i32) -> Vec<Value> {
        vec![Value::Integer(id), Value::Text(format!("user {id}"))]
    }

    #[test]
    fn seq_scan_of_an_empty_table_should_produce_nothing() {
        let mut db = TestDb::new(4);
        db.create_table("accounts", accounts(), &[]);

        let rows = execute(&mut SeqScan::new("accounts"), &mut db.ctx()).unwrap();

        assert!(rows.is_empty());
    }

    #[test]
    fn seq_scan_should_produce_every_row_with_its_rid_across_pages() {
        let mut db = TestDb::new(2);
        let rows: Vec<Vec<Value>> = (0..80).map(account).collect();
        db.create_table("accounts", accounts(), &rows);
        let doomed: Vec<_> = db.stored_rows("accounts").into_iter().step_by(3).collect();
        for row in doomed {
            db.with_table("accounts", |heap, bpm| {
                heap.delete(bpm, row.rid.unwrap()).unwrap()
            });
        }
        let mut scan = SeqScan::new("accounts");

        let scanned = execute(&mut scan, &mut db.ctx()).unwrap();

        assert_eq!(scanned, db.stored_rows("accounts"));
        assert_eq!(scanned.len(), 80 - 27);
        let pages: std::collections::HashSet<u32> =
            scanned.iter().map(|row| row.rid.unwrap().page_id).collect();
        assert!(pages.len() > 2);
        assert_eq!(
            scan.next(&mut db.ctx()).unwrap(),
            None,
            "still done after the end"
        );
    }

    #[test]
    fn seq_scan_should_read_a_table_after_reopening() {
        let mut db = TestDb::new(4);
        let rows: Vec<Vec<Value>> = (0..30).map(account).collect();
        db.create_table("accounts", accounts(), &rows);
        db.reopen(4);

        let scanned = execute(&mut SeqScan::new("accounts"), &mut db.ctx()).unwrap();

        let values: Vec<Vec<Value>> = scanned.into_iter().map(|row| row.values).collect();
        assert_eq!(values, rows);
    }

    #[test]
    fn seq_scan_of_an_unknown_table_should_fail() {
        let mut db = TestDb::new(4);

        let err = SeqScan::new("missing").next(&mut db.ctx()).unwrap_err();

        assert_eq!(err.sqlstate(), "42P01");
    }

    use crate::execution::{BinaryOp, Delete, Filter, Insert, Update};
    use crate::types::{Date, Finite32, Finite64, Timestamp};
    use std::ops::Bound::{Excluded, Included, Unbounded};

    /// `accounts (id INTEGER NOT NULL, name VARCHAR, age SMALLINT, score REAL)` with indexes
    /// on `id`, `age` and `score`; every fourth age is NULL and the score is `id / 10`.
    fn indexed_accounts(ids: impl Iterator<Item = i32>, pool_size: usize) -> TestDb {
        let mut db = TestDb::new(pool_size);
        let schema = Schema::new(vec![
            Column::not_null("id", DataType::Integer),
            Column::new("name", DataType::VARCHAR_MAX),
            Column::new("age", DataType::SmallInt),
            Column::new("score", DataType::Real),
        ])
        .unwrap();
        let rows: Vec<Vec<Value>> = ids
            .map(|id| {
                let age = if id % 4 == 0 {
                    Value::Null
                } else {
                    Value::SmallInt(id as i16)
                };
                vec![
                    Value::Integer(id),
                    Value::Text(format!("user {id}")),
                    age,
                    score(id),
                ]
            })
            .collect();
        db.create_table("accounts", schema, &rows);
        let indexes = [
            ("accounts_id", "id"),
            ("accounts_age", "age"),
            ("accounts_score", "score"),
        ];
        for (name, column) in indexes {
            let ctx = db.ctx();
            ctx.catalog
                .create_index(ctx.bpm, name, "accounts", column)
                .unwrap();
        }
        db
    }

    fn score(id: i32) -> Value {
        Value::Real(Finite32::new(id as f32 / 10.0).unwrap())
    }

    fn double(value: f64) -> Value {
        Value::Double(Finite64::new(value).unwrap())
    }

    fn scan_ids(db: &mut TestDb, scan: &mut IndexScan) -> Vec<i32> {
        execute(scan, &mut db.ctx())
            .unwrap()
            .into_iter()
            .map(|row| match row.values[0] {
                Value::Integer(id) => id,
                ref other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn index_scan_of_a_point_should_find_the_row() {
        let mut db = indexed_accounts(0..50, 16);
        let stored = db.stored_rows("accounts");

        let found = execute(&mut IndexScan::point("accounts_id", int(17)), &mut db.ctx()).unwrap();

        assert_eq!(found, [stored[17].clone()]);
    }

    #[test]
    fn index_scan_of_a_missing_or_null_point_should_find_nothing() {
        let mut db = indexed_accounts((0..50).map(|id| id * 2), 16);

        let missing = scan_ids(&mut db, &mut IndexScan::point("accounts_id", int(17)));
        let null = scan_ids(
            &mut db,
            &mut IndexScan::point("accounts_id", Expr::Literal(Value::Null)),
        );

        assert!(missing.is_empty());
        assert!(null.is_empty());
    }

    #[test]
    fn index_scan_should_report_bad_indexes_and_bounds() {
        let mut db = indexed_accounts(0..5, 16);
        let failing = Expr::binary(BinaryOp::Div, int(1), int(0));
        let cases = [
            (IndexScan::point("missing", int(1)), "42704"),
            (IndexScan::point("accounts_id", Expr::Column(0)), "XX000"),
            (
                IndexScan::new("accounts_id", Included(int(1)), Excluded(failing)),
                "22012",
            ),
            (
                IndexScan::point("accounts_id", Expr::Literal(Value::Text("1".to_string()))),
                "42804",
            ),
        ];

        for (mut scan, sqlstate) in cases {
            let err = scan.next(&mut db.ctx()).unwrap_err();

            assert_eq!(err.sqlstate(), sqlstate, "{err:?}");
        }
    }

    #[test]
    fn index_scan_of_a_row_the_table_no_longer_has_should_fail_and_end() {
        let mut db = indexed_accounts(0..5, 16);
        let rid = db.stored_rows("accounts")[2].rid.unwrap();
        db.with_table("accounts", |heap, bpm| heap.delete(bpm, rid))
            .unwrap();
        let mut scan = IndexScan::new("accounts_id", Unbounded, Unbounded);

        let before = [
            scan.next(&mut db.ctx()).unwrap(),
            scan.next(&mut db.ctx()).unwrap(),
        ];
        let err = scan.next(&mut db.ctx()).unwrap_err();

        assert!(before.iter().all(Option::is_some));
        assert_eq!(err.sqlstate(), "XX000", "{err:?}");
        assert_eq!(scan.next(&mut db.ctx()).unwrap(), None);
    }

    #[test]
    fn index_scan_of_an_empty_table_should_find_nothing() {
        let mut db = indexed_accounts(0..0, 16);

        let open = scan_ids(
            &mut db,
            &mut IndexScan::new("accounts_id", Unbounded, Unbounded),
        );
        let point = scan_ids(&mut db, &mut IndexScan::point("accounts_id", int(1)));

        assert!(open.is_empty());
        assert!(point.is_empty());
    }

    #[test]
    fn index_scan_should_go_on_after_keys_are_removed_between_calls() {
        let mut db = indexed_accounts(0..300, 16);
        let mut scan = IndexScan::new("accounts_id", Unbounded, Unbounded);
        let mut ids: Vec<Value> = (0..10)
            .map(|_| scan.next(&mut db.ctx()).unwrap().unwrap().values[0].clone())
            .collect();

        // Most leaves merge away, the one of the cursor among them.
        let range = IndexScan::new("accounts_id", Included(int(5)), Included(int(250)));
        execute(&mut Delete::new("accounts", Box::new(range)), &mut db.ctx()).unwrap();
        while let Some(row) = scan.next(&mut db.ctx()).unwrap() {
            ids.push(row.values[0].clone());
        }

        let expected: Vec<Value> = (0..10).chain(251..300).map(Value::Integer).collect();
        assert_eq!(ids, expected);
    }

    #[test]
    fn index_scan_should_read_timestamp_keys_with_date_bounds() {
        let mut db = TestDb::new(16);
        let schema = Schema::new(vec![
            Column::not_null("id", DataType::Integer),
            Column::new("at", DataType::Timestamp),
        ])
        .unwrap();
        let rows: Vec<Vec<Value>> = (1..=20)
            .flat_map(|day| [0, 12].map(move |hour| (day, hour)))
            .map(|(day, hour)| {
                let date = Date::from_ymd(2024, 3, day).unwrap();
                let at = Timestamp::new(date, hour, 0, 0, 0).unwrap();
                vec![
                    Value::Integer((day * 100 + hour) as i32),
                    Value::Timestamp(at),
                ]
            })
            .collect();
        db.create_table("events", schema, &rows);
        let ctx = db.ctx();
        ctx.catalog
            .create_index(ctx.bpm, "events_at", "events", "at")
            .unwrap();
        let date = |day| Value::Date(Date::from_ymd(2024, 3, day).unwrap());
        let cases = [
            (Included(date(5)), Excluded(date(8))),
            (Excluded(date(5)), Included(date(8))),
            (Unbounded, Excluded(date(2))),
            (Included(date(20)), Unbounded),
        ];

        for (lower, upper) in cases {
            let description = format!("{lower:?}..{upper:?}");
            let predicate = range_predicate(1, &lower, &upper);
            let mut expected = execute(
                &mut Filter::new(Box::new(SeqScan::new("events")), predicate),
                &mut db.ctx(),
            )
            .unwrap();
            expected.sort_by(|lhs, rhs| lhs.values[1].sort_cmp(&rhs.values[1]));

            let mut scan =
                IndexScan::new("events_at", literal_bound(&lower), literal_bound(&upper));
            let found = execute(&mut scan, &mut db.ctx()).unwrap();

            assert!(!found.is_empty(), "{description}");
            assert_eq!(found, expected, "{description}");
        }
    }

    #[test]
    fn index_scan_should_find_float_keys_a_rounded_bound_admits() {
        // 0.1 as REAL is above 0.1 as DOUBLE PRECISION.
        let mut db = indexed_accounts(0..5, 16);
        let up_to = IndexScan::new(
            "accounts_score",
            Unbounded,
            Included(Expr::Literal(double(0.1))),
        );
        let from = IndexScan::new(
            "accounts_score",
            Excluded(Expr::Literal(double(0.1))),
            Unbounded,
        );
        let point = IndexScan::point("accounts_score", Expr::Literal(double(0.1)));

        assert_eq!(scan_ids(&mut db, &mut { up_to }), [0]);
        assert_eq!(scan_ids(&mut db, &mut { from }), [1, 2, 3, 4]);
        assert!(scan_ids(&mut db, &mut { point }).is_empty());
    }

    #[test]
    fn index_scan_should_end_and_stay_ended() {
        let mut db = indexed_accounts(0..5, 16);
        let mut scan = IndexScan::point("accounts_id", int(3));

        let rows = execute(&mut scan, &mut db.ctx()).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(scan.next(&mut db.ctx()).unwrap(), None);
    }

    #[test]
    fn index_scan_should_honour_inclusive_exclusive_and_open_bounds() {
        let mut db = indexed_accounts(0..100, 16);
        let cases = [
            (
                Included(int(10)),
                Included(int(14)),
                (10..=14).collect::<Vec<_>>(),
            ),
            (Excluded(int(10)), Excluded(int(14)), (11..14).collect()),
            (Unbounded, Excluded(int(3)), (0..3).collect()),
            (Included(int(97)), Unbounded, (97..100).collect()),
            (Unbounded, Unbounded, (0..100).collect()),
            (Included(int(20)), Included(int(10)), Vec::new()),
            (Excluded(int(5)), Excluded(int(6)), Vec::new()),
            (Included(int(-50)), Excluded(int(2)), vec![0, 1]),
        ];

        for (from, to, expected) in cases {
            let description = format!("{from:?}..{to:?}");

            let ids = scan_ids(&mut db, &mut IndexScan::new("accounts_id", from, to));

            assert_eq!(ids, expected, "{description}");
        }
    }

    #[test]
    fn index_scan_should_return_the_stored_rows_in_key_order_across_leaves() {
        // Inserted in scattered order, read back sorted by id, with a 2-frame pool for the scan.
        let ids: Vec<i32> = (0..300).map(|i| (i * 7_919) % 300).collect();
        let mut db = indexed_accounts(ids.into_iter(), 16);
        db.reopen(2);
        let mut stored = db.stored_rows("accounts");
        stored.sort_by(|lhs, rhs| lhs.values[0].sort_cmp(&rhs.values[0]));

        let rows = execute(
            &mut IndexScan::new("accounts_id", Unbounded, Unbounded),
            &mut db.ctx(),
        )
        .unwrap();

        assert_eq!(rows, stored);
    }

    #[test]
    fn index_scan_should_never_return_rows_whose_key_is_null() {
        let mut db = indexed_accounts(0..40, 16);

        let rows = execute(
            &mut IndexScan::new("accounts_age", Unbounded, Unbounded),
            &mut db.ctx(),
        )
        .unwrap();

        assert_eq!(rows.len(), 30);
        assert!(rows.iter().all(|row| !row.values[2].is_null()));
    }

    #[test]
    fn index_scan_should_convert_its_bounds_to_the_key_type() {
        let mut db = indexed_accounts(0..40, 16);
        // SMALLINT bounds on the INTEGER index, and INTEGER bounds past SMALLINT on the age index.
        let small = IndexScan::new(
            "accounts_id",
            Included(Expr::Literal(Value::SmallInt(5))),
            Excluded(Expr::Literal(Value::SmallInt(8))),
        );
        let past_smallint = IndexScan::new(
            "accounts_age",
            Excluded(int(-70_000)),
            Included(int(70_000)),
        );
        let above_every_age = IndexScan::new("accounts_age", Included(int(70_000)), Unbounded);

        assert_eq!(scan_ids(&mut db, &mut { small }), [5, 6, 7]);
        assert_eq!(scan_ids(&mut db, &mut { past_smallint }).len(), 30);
        assert!(scan_ids(&mut db, &mut { above_every_age }).is_empty());
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }

        fn key(&mut self) -> i32 {
            self.below(301) as i32 - 150
        }

        /// Mostly keys of the table's range, in every integer width, but also `NULL` and
        /// integers past SMALLINT and INTEGER.
        fn bound_value(&mut self) -> Value {
            let sign = if self.below(2) == 0 { 1 } else { -1 };
            match self.below(10) {
                0 => Value::Null,
                1 => Value::Integer(sign * 70_000),
                2 => Value::BigInt(if sign > 0 { i64::MAX } else { i64::MIN }),
                3 => Value::SmallInt(self.key() as i16),
                4 => Value::BigInt(i64::from(self.key())),
                _ => Value::Integer(self.key()),
            }
        }

        /// Bounds for the REAL score (`id / 10`): DOUBLE PRECISION tenths, which a REAL
        /// rounds, exact keys and integers, and values past REAL or too close to zero for it.
        fn float_bound_value(&mut self) -> Value {
            let sign = if self.below(2) == 0 { 1.0 } else { -1.0 };
            match self.below(10) {
                0 => Value::Null,
                1 => double(sign * 1e39),
                2 => double(sign * 1e-50),
                3 => Value::Integer(self.key() / 10),
                4 => score(self.key()),
                _ => double(f64::from(self.key()) / 10.0),
            }
        }

        fn bound(&mut self, is_float: bool) -> Bound<Value> {
            let value = |rng: &mut Self| match is_float {
                true => rng.float_bound_value(),
                false => rng.bound_value(),
            };
            match self.below(4) {
                0 => Unbounded,
                1 => Excluded(value(self)),
                _ => Included(value(self)),
            }
        }
    }

    fn literal_bound(bound: &Bound<Value>) -> Bound<Expr> {
        bound.as_ref().map(|value| Expr::Literal(value.clone()))
    }

    /// `low <= column AND column < high`, or the comparisons the bounds ask for; an open side
    /// is `column IS NOT NULL`, since the index has no `NULL` keys.
    fn range_predicate(column: usize, lower: &Bound<Value>, upper: &Bound<Value>) -> Expr {
        let side = |bound: &Bound<Value>, inclusive, exclusive| match bound {
            Unbounded => Expr::IsNull {
                expr: Box::new(Expr::Column(column)),
                negated: true,
            },
            Included(value) => Expr::binary(
                inclusive,
                Expr::Column(column),
                Expr::Literal(value.clone()),
            ),
            Excluded(value) => Expr::binary(
                exclusive,
                Expr::Column(column),
                Expr::Literal(value.clone()),
            ),
        };
        Expr::binary(
            BinaryOp::And,
            side(lower, BinaryOp::Ge, BinaryOp::Gt),
            side(upper, BinaryOp::Le, BinaryOp::Lt),
        )
    }

    /// A statement refused for a repeated key changes nothing; any other error is a bug.
    fn run_write(db: &mut TestDb, write: &mut dyn Executor) {
        if let Err(err) = execute(write, &mut db.ctx()) {
            assert_eq!(err.sqlstate(), "23505", "{err:?}");
        }
    }

    #[test]
    fn random_index_scans_should_match_a_filtered_seq_scan() {
        let mut rng = Rng(0x5eed_1dea);
        let mut db = indexed_accounts((-150..150).step_by(3), 16);
        let indexes = [
            ("accounts_id", 0),
            ("accounts_age", 2),
            ("accounts_score", 3),
        ];

        for step in 0..600 {
            match rng.below(10) {
                0..=2 => {
                    let id = rng.key();
                    let age = match rng.below(4) {
                        0 => Value::Null,
                        _ => Value::SmallInt(rng.key() as i16),
                    };
                    let row = vec![
                        int(id),
                        Expr::Literal(Value::Text(format!("new {id}"))),
                        Expr::Literal(age),
                        Expr::Literal(score(rng.key())),
                    ];
                    let mut insert = Insert::new("accounts", Box::new(Values::new(vec![row])));
                    run_write(&mut db, &mut insert);
                }
                3 => {
                    let (index, _) = indexes[rng.below(2)];
                    let low = rng.key();
                    let high = low + rng.below(10) as i32;
                    let scan = IndexScan::new(index, Included(int(low)), Included(int(high)));
                    run_write(&mut db, &mut Delete::new("accounts", Box::new(scan)));
                }
                4 => {
                    let low = rng.key();
                    let scan =
                        IndexScan::new("accounts_age", Included(int(low)), Excluded(int(low + 20)));
                    let older = Expr::binary(BinaryOp::Add, Expr::Column(2), int(1));
                    run_write(
                        &mut db,
                        &mut Update::new("accounts", Box::new(scan), vec![(2, older)]),
                    );
                }
                _ => {
                    let (index, column) = indexes[rng.below(3)];
                    let is_float = index == "accounts_score";
                    let (lower, upper) = (rng.bound(is_float), rng.bound(is_float));
                    let description = format!("step {step}: {index} {lower:?}..{upper:?}");
                    let predicate = range_predicate(column, &lower, &upper);
                    let mut expected = execute(
                        &mut Filter::new(Box::new(SeqScan::new("accounts")), predicate),
                        &mut db.ctx(),
                    )
                    .unwrap();
                    expected.sort_by(|lhs, rhs| lhs.values[column].sort_cmp(&rhs.values[column]));

                    let mut scan =
                        IndexScan::new(index, literal_bound(&lower), literal_bound(&upper));
                    let found = execute(&mut scan, &mut db.ctx()).unwrap();

                    assert_eq!(found, expected, "{description}");
                }
            }
            if step % 150 == 149 {
                db.reopen(16);
            }
        }
    }

    #[test]
    fn values_should_produce_each_row_once() {
        let mut db = TestDb::new(4);
        let mut values = Values::new(vec![
            vec![int(1), Expr::Literal(Value::Null)],
            vec![
                int(2),
                Expr::binary(crate::execution::BinaryOp::Add, int(1), int(2)),
            ],
        ]);

        let rows = execute(&mut values, &mut db.ctx()).unwrap();

        assert_eq!(
            rows,
            [
                Row {
                    values: vec![Value::Integer(1), Value::Null],
                    rid: None
                },
                Row {
                    values: vec![Value::Integer(2), Value::Integer(3)],
                    rid: None
                },
            ]
        );
        assert_eq!(
            values.next(&mut db.ctx()).unwrap(),
            None,
            "still done after the end"
        );
    }

    #[test]
    fn values_without_rows_should_produce_nothing() {
        let mut db = TestDb::new(4);

        let rows = execute(&mut Values::new(Vec::new()), &mut db.ctx()).unwrap();

        assert!(rows.is_empty());
    }

    #[test]
    fn values_should_report_a_failing_expression() {
        let mut db = TestDb::new(4);
        let mut values = Values::new(vec![
            vec![int(1)],
            vec![Expr::binary(
                crate::execution::BinaryOp::Div,
                int(1),
                int(0),
            )],
        ]);

        let first = values.next(&mut db.ctx()).unwrap();
        let err = values.next(&mut db.ctx()).unwrap_err();
        let reads_a_column = Values::new(vec![vec![Expr::Column(0)]])
            .next(&mut db.ctx())
            .unwrap_err();

        assert_eq!(first.unwrap().values, [Value::Integer(1)]);
        assert_eq!(err.sqlstate(), "22012");
        assert!(matches!(
            reads_a_column,
            ExecutionError::ColumnOutOfRange { column: 0, len: 0 }
        ));
    }
}
