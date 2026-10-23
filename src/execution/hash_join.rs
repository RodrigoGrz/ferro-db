//! `HashJoin` joins two inputs by equal keys through an in-memory hash table (CMU 15-445
//! lecture 12): the first call reads the whole right input into buckets by key (build), then
//! each left row looks its bucket up (probe).
//!
//! Keys are normalized before hashing (`HashKey`) so that two keys are equal exactly when
//! SQL's `=` is TRUE: `1 = 1.0`, a DATE equals its midnight TIMESTAMP, text compares by bytes.
//! A NULL in any key never matches. Every key value is compared with the first right key of
//! its column, so keys of different families fail with `=`'s error (42883). Each right row
//! carries its arrival number and whether it matched: `RIGHT` and `FULL` hand out the
//! unmatched ones at the end, in input order, padded with the left input's declared width of
//! NULLs. The output follows the left input; one left row's matches follow the right input.

use std::collections::HashMap;

use crate::types::{Timestamp, Value};

use super::join::{joined, passes};
use super::{ExecutionContext, ExecutionError, Executor, Expr, JoinType, Row};

/// One side of a join: its rows and how many values each has, which an outer join pads
/// with NULLs even when that side has no rows.
pub struct JoinInput {
    pub child: Box<dyn Executor>,
    pub width: usize,
}

/// Builds a hash table of the right input on its first call, then probes it with each left
/// row. A joined row is the left values followed by the right values, without `Rid`.
pub struct HashJoin {
    left: JoinInput,
    right: JoinInput,
    keys: Vec<(Expr, Expr)>,
    join_type: JoinType,
    /// The rest of the `ON`, over the joined row: it decides what matches, including which
    /// right rows count as matched for `RIGHT` and `FULL`.
    predicate: Option<Expr>,
    state: HashJoinState,
}

enum HashJoinState {
    Start,
    Probing {
        table: BuildTable,
        pending: std::vec::IntoIter<Row>,
    },
    /// The right rows that never matched, for `RIGHT` and `FULL`.
    Finishing(std::vec::IntoIter<Row>),
    Done,
}

/// A right row, with what `RIGHT` and `FULL` need: whether it matched, and its place in the
/// input to hand out the unmatched ones in order.
struct BuildRow {
    values: Vec<Value>,
    arrival: usize,
    is_matched: bool,
}

/// The right input, by key.
struct BuildTable {
    buckets: HashMap<Vec<HashKey>, Vec<BuildRow>>,
    /// Rows with a NULL key: they never match, but `RIGHT` and `FULL` hand them out.
    unmatchable: Vec<BuildRow>,
    /// The first non-NULL value of each right key: every later key value is compared with it,
    /// so that keys of different families fail as `=` does.
    samples: Vec<Option<Value>>,
}

impl BuildTable {
    /// Compares each non-NULL key with its column's sample; with `is_build`, a column
    /// without a sample takes this value as its sample.
    fn check_families(
        &mut self,
        key_values: &[Value],
        is_build: bool,
    ) -> Result<(), ExecutionError> {
        for (value, sample) in key_values.iter().zip(&mut self.samples) {
            if value.is_null() {
                continue;
            }
            match sample {
                Some(sample) => {
                    value.compare(sample)?;
                }
                None if is_build => *sample = Some(value.clone()),
                None => {}
            }
        }
        Ok(())
    }

    /// The rows that never matched, in input order.
    fn into_unmatched(self) -> Vec<BuildRow> {
        let mut unmatched: Vec<BuildRow> = self
            .buckets
            .into_values()
            .flatten()
            .chain(self.unmatchable)
            .filter(|row| !row.is_matched)
            .collect();
        unmatched.sort_unstable_by_key(|row| row.arrival);
        unmatched
    }
}

impl HashJoin {
    /// `keys` pairs an expression over the left row with one over the right row; two rows
    /// match when every pair is equal (and `predicate`, over the joined row, is TRUE).
    pub fn new(
        left: JoinInput,
        right: JoinInput,
        keys: Vec<(Expr, Expr)>,
        join_type: JoinType,
        predicate: Option<Expr>,
    ) -> Self {
        Self {
            left,
            right,
            keys,
            join_type,
            predicate,
            state: HashJoinState::Start,
        }
    }

    fn build(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<BuildTable, ExecutionError> {
        let mut table = BuildTable {
            buckets: HashMap::new(),
            unmatchable: Vec::new(),
            samples: vec![None; self.keys.len()],
        };
        let mut arrival = 0;
        while let Some(row) = self.right.child.next(ctx)? {
            check_width(&row.values, self.right.width)?;
            let key_values = key_values(self.keys.iter().map(|(_, right)| right), &row.values)?;
            table.check_families(&key_values, true)?;
            let build_row = BuildRow {
                values: row.values,
                arrival,
                is_matched: false,
            };
            arrival += 1;
            match hash_keys(&key_values) {
                Some(key) => table.buckets.entry(key).or_default().push(build_row),
                None => table.unmatchable.push(build_row),
            }
        }
        Ok(table)
    }

    /// The joined rows of one left row: its matches, which mark their right rows, or for
    /// `LEFT` and `FULL` without any, the row padded with NULLs.
    fn probe(&self, table: &mut BuildTable, left_row: Row) -> Result<Vec<Row>, ExecutionError> {
        check_width(&left_row.values, self.left.width)?;
        let key_values = key_values(self.keys.iter().map(|(left, _)| left), &left_row.values)?;
        table.check_families(&key_values, false)?;
        let mut matches = Vec::new();
        if let Some(bucket) = hash_keys(&key_values).and_then(|key| table.buckets.get_mut(&key)) {
            for right_row in bucket {
                let row = joined(left_row.values.clone(), right_row.values.iter().cloned());
                if passes(self.predicate.as_ref(), &row.values)? {
                    right_row.is_matched = true;
                    matches.push(row);
                }
            }
        }
        if matches.is_empty() && matches!(self.join_type, JoinType::Left | JoinType::Full) {
            let nulls = std::iter::repeat_n(Value::Null, self.right.width);
            matches.push(joined(left_row.values, nulls));
        }
        Ok(matches)
    }

    /// For `RIGHT` and `FULL`, the right rows that never matched, padded with NULLs.
    fn unmatched_right_rows(&self, table: BuildTable) -> Vec<Row> {
        if !matches!(self.join_type, JoinType::Right | JoinType::Full) {
            return Vec::new();
        }
        table
            .into_unmatched()
            .into_iter()
            .map(|right_row| joined(vec![Value::Null; self.left.width], right_row.values))
            .collect()
    }
}

impl Executor for HashJoin {
    fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError> {
        // `Done` until the next row is in hand, so an error also ends the join.
        let mut state = std::mem::replace(&mut self.state, HashJoinState::Done);
        loop {
            state = match state {
                HashJoinState::Start => HashJoinState::Probing {
                    table: self.build(ctx)?,
                    pending: Vec::new().into_iter(),
                },
                HashJoinState::Probing {
                    mut table,
                    mut pending,
                } => {
                    if let Some(row) = pending.next() {
                        self.state = HashJoinState::Probing { table, pending };
                        return Ok(Some(row));
                    }
                    match self.left.child.next(ctx)? {
                        Some(left_row) => {
                            let pending = self.probe(&mut table, left_row)?.into_iter();
                            HashJoinState::Probing { table, pending }
                        }
                        None => {
                            HashJoinState::Finishing(self.unmatched_right_rows(table).into_iter())
                        }
                    }
                }
                HashJoinState::Finishing(mut rows) => {
                    let row = rows.next();
                    if row.is_some() {
                        self.state = HashJoinState::Finishing(rows);
                    }
                    return Ok(row);
                }
                HashJoinState::Done => return Ok(None),
            };
        }
    }
}

fn check_width(values: &[Value], width: usize) -> Result<(), ExecutionError> {
    if values.len() == width {
        Ok(())
    } else {
        Err(ExecutionError::RowWidthMismatch {
            expected: width,
            actual: values.len(),
        })
    }
}

fn key_values<'a>(
    exprs: impl Iterator<Item = &'a Expr>,
    row: &[Value],
) -> Result<Vec<Value>, ExecutionError> {
    exprs.map(|expr| expr.eval(row)).collect()
}

/// `None` when a key is NULL: the row matches nothing.
fn hash_keys(key_values: &[Value]) -> Option<Vec<HashKey>> {
    key_values.iter().map(hash_key).collect()
}

/// A join key value in the form the hash table compares: two keys are equal exactly when
/// SQL's `=` is TRUE for the values they come from. `Value`'s derived `Eq` is not that
/// across types (`Integer(1)` against `BigInt(1)` or `Double(1.0)`), and the two sides of a
/// join may have different types.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum HashKey {
    /// Every integer, and every float whose value is an integer `i64` holds.
    Integer(i64),
    /// The bits of any other float, widened to `f64` exactly. With no NaN and no `-0.0`,
    /// equal bits are equal floats.
    Float(u64),
    Text(String),
    Boolean(bool),
    /// A DATE as its midnight, a TIMESTAMP as itself, in microseconds.
    Instant(i64),
}

/// `None` for NULL, which never matches.
fn hash_key(value: &Value) -> Option<HashKey> {
    Some(match value {
        Value::Null => return None,
        Value::SmallInt(number) => HashKey::Integer(i64::from(*number)),
        Value::Integer(number) => HashKey::Integer(i64::from(*number)),
        Value::BigInt(number) => HashKey::Integer(*number),
        Value::Real(number) => float_key(f64::from(number.get())),
        Value::Double(number) => float_key(number.get()),
        Value::Text(text) => HashKey::Text(text.clone()),
        Value::Boolean(value) => HashKey::Boolean(*value),
        Value::Date(date) => HashKey::Instant(Timestamp::at_midnight(*date).micros_since_epoch()),
        Value::Timestamp(timestamp) => HashKey::Instant(timestamp.micros_since_epoch()),
    })
}

fn float_key(value: f64) -> HashKey {
    // -2^63 is exact in f64; 2^63 is the first float past `i64::MAX`.
    const I64_RANGE: std::ops::Range<f64> =
        -9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0;
    if value.fract() == 0.0 && I64_RANGE.contains(&value) {
        HashKey::Integer(value as i64)
    } else {
        HashKey::Float(value.to_bits())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::test_support::TestDb;
    use crate::execution::{BinaryOp, SeqScan, Values, execute};
    use crate::table::{Column, Schema};
    use crate::types::{DataType, Date, Finite32, Finite64, Timestamp};

    fn real(value: f32) -> Value {
        Value::Real(Finite32::new(value).unwrap())
    }

    fn double(value: f64) -> Value {
        Value::Double(Finite64::new(value).unwrap())
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_string())
    }

    #[test]
    fn hash_keys_should_be_equal_when_sql_equality_is_true() {
        let day = Date::from_ymd(2026, 10, 22).unwrap();
        let equal = [
            (Value::SmallInt(7), Value::BigInt(7)),
            (Value::Integer(1), double(1.0)),
            (Value::BigInt(-3), real(-3.0)),
            (real(0.5), double(0.5)),
            (text("ana"), text("ana")),
            (Value::Boolean(true), Value::Boolean(true)),
            (
                Value::Date(day),
                Value::Timestamp(Timestamp::at_midnight(day)),
            ),
        ];

        for (left, right) in equal {
            assert_eq!(hash_key(&left), hash_key(&right), "{left:?} = {right:?}");
        }
    }

    #[test]
    fn hash_keys_should_differ_when_sql_equality_is_false() {
        let day = Date::from_ymd(2026, 10, 22).unwrap();
        let next_day = Date::from_ymd(2026, 10, 23).unwrap();
        let different = [
            (Value::Integer(1), Value::Integer(2)),
            (double(0.5), Value::Integer(1)),
            (double(0.5), Value::Integer(0)),
            (real(0.1), double(0.1)),
            (double(1e20), Value::BigInt(i64::MAX)),
            (double(-9.3e18), Value::BigInt(i64::MIN)),
            (text("a"), text("a ")),
            (Value::Boolean(true), Value::Boolean(false)),
            (
                Value::Date(next_day),
                Value::Timestamp(Timestamp::at_midnight(day)),
            ),
            (text("1"), Value::Integer(1)),
        ];

        for (left, right) in different {
            assert_ne!(hash_key(&left), hash_key(&right), "{left:?} <> {right:?}");
        }
    }

    #[test]
    fn a_null_value_should_have_no_hash_key() {
        assert_eq!(hash_key(&Value::Null), None);
    }

    fn int(value: i32) -> Value {
        Value::Integer(value)
    }

    fn col(column: usize) -> Expr {
        Expr::Column(column)
    }

    fn input(rows: Vec<Vec<Value>>, width: usize) -> JoinInput {
        let rows = rows
            .into_iter()
            .map(|row| row.into_iter().map(Expr::Literal).collect())
            .collect();
        JoinInput {
            child: Box::new(Values::new(rows)),
            width,
        }
    }

    /// `(id, key)` rows: l1 and l3 share key 1, l4 has a key no right row has.
    fn left_rows() -> Vec<Vec<Value>> {
        vec![
            vec![text("l1"), int(1)],
            vec![text("l2"), int(2)],
            vec![text("l3"), int(1)],
            vec![text("l4"), int(9)],
        ]
    }

    /// `(key, id)` rows: r1 and r3 share key 1, r2 has a key no left row has.
    fn right_rows() -> Vec<Vec<Value>> {
        vec![
            vec![int(1), text("r1")],
            vec![int(3), text("r2")],
            vec![int(1), text("r3")],
        ]
    }

    fn on_keys() -> Vec<(Expr, Expr)> {
        vec![(col(1), col(0))]
    }

    fn run(join: &mut HashJoin) -> Vec<Vec<Value>> {
        let mut db = TestDb::new(4);

        let output = execute(join, &mut db.ctx()).unwrap();

        assert_eq!(join.next(&mut db.ctx()).unwrap(), None);
        output.into_iter().map(|row| row.values).collect()
    }

    fn join_err(join: &mut HashJoin) -> ExecutionError {
        let mut db = TestDb::new(4);

        let err = execute(join, &mut db.ctx()).unwrap_err();

        assert_eq!(join.next(&mut db.ctx()).unwrap(), None);
        err
    }

    fn inner(left: JoinInput, right: JoinInput, keys: Vec<(Expr, Expr)>) -> HashJoin {
        HashJoin::new(left, right, keys, JoinType::Inner, None)
    }

    #[test]
    fn inner_join_should_pair_equal_keys_in_left_then_right_order() {
        let mut join = inner(input(left_rows(), 2), input(right_rows(), 2), on_keys());

        let output = run(&mut join);

        assert_eq!(
            output,
            [
                [text("l1"), int(1), int(1), text("r1")],
                [text("l1"), int(1), int(1), text("r3")],
                [text("l3"), int(1), int(1), text("r1")],
                [text("l3"), int(1), int(1), text("r3")],
            ]
        );
    }

    #[test]
    fn every_key_pair_should_be_equal_for_a_match() {
        let left = input(vec![vec![int(1), text("a")], vec![int(1), text("b")]], 2);
        let right = input(vec![vec![int(1), text("b")], vec![int(2), text("a")]], 2);
        let keys = vec![(col(0), col(0)), (col(1), col(1))];

        let output = run(&mut inner(left, right, keys));

        assert_eq!(output, [[int(1), text("b"), int(1), text("b")]]);
    }

    #[test]
    fn keys_of_different_numeric_types_should_match_by_value() {
        let left = input(
            vec![vec![Value::SmallInt(7)], vec![int(1)], vec![double(0.5)]],
            1,
        );
        let right = input(
            vec![vec![Value::BigInt(7)], vec![double(1.0)], vec![real(0.5)]],
            1,
        );

        let output = run(&mut inner(left, right, vec![(col(0), col(0))]));

        assert_eq!(
            output,
            [
                [Value::SmallInt(7), Value::BigInt(7)],
                [int(1), double(1.0)],
                [double(0.5), real(0.5)],
            ]
        );
    }

    #[test]
    fn a_null_in_any_key_should_match_nothing() {
        let left = input(
            vec![vec![Value::Null, int(1)], vec![int(1), Value::Null]],
            2,
        );
        let right = input(
            vec![vec![Value::Null, int(1)], vec![int(1), Value::Null]],
            2,
        );
        let keys = vec![(col(0), col(0)), (col(1), col(1))];

        let output = run(&mut inner(left, right, keys));

        assert!(output.is_empty());
    }

    #[test]
    fn keys_of_different_families_should_fail_like_equality() {
        let left_text = inner(
            input(vec![vec![text("1")]], 1),
            input(vec![vec![int(1)]], 1),
            vec![(col(0), col(0))],
        );
        let mixed_right = inner(
            input(vec![], 1),
            input(vec![vec![int(1)], vec![Value::Null], vec![text("1")]], 1),
            vec![(col(0), col(0))],
        );

        for mut join in [left_text, mixed_right] {
            let err = join_err(&mut join);

            assert_eq!(err.sqlstate(), "42883", "{err:?}");
        }
    }

    #[test]
    fn inner_join_with_an_empty_side_should_give_no_rows() {
        let no_left = inner(input(vec![], 2), input(right_rows(), 2), on_keys());
        let no_right = inner(input(left_rows(), 2), input(vec![], 2), on_keys());

        for mut join in [no_left, no_right] {
            assert!(run(&mut join).is_empty());
        }
    }

    #[test]
    fn a_join_without_keys_should_pair_every_row() {
        let left = input(vec![vec![int(1)], vec![int(2)]], 1);
        let right = input(vec![vec![text("a")], vec![text("b")]], 1);

        let output = run(&mut inner(left, right, vec![]));

        assert_eq!(
            output,
            [
                [int(1), text("a")],
                [int(1), text("b")],
                [int(2), text("a")],
                [int(2), text("b")],
            ]
        );
    }

    #[test]
    fn a_row_of_another_width_than_declared_should_fail() {
        let wrong_left = inner(input(left_rows(), 3), input(right_rows(), 2), on_keys());
        let wrong_right = inner(input(left_rows(), 2), input(right_rows(), 1), on_keys());

        for mut join in [wrong_left, wrong_right] {
            let err = join_err(&mut join);

            assert!(
                matches!(err, ExecutionError::RowWidthMismatch { .. }),
                "{err:?}"
            );
            assert_eq!(err.sqlstate(), "XX000");
        }
    }

    #[test]
    fn a_failing_key_or_child_should_end_the_join() {
        let one_over_zero =
            Expr::binary(BinaryOp::Div, Expr::Literal(int(1)), Expr::Literal(int(0)));
        let failing_left_key = inner(
            input(left_rows(), 2),
            input(right_rows(), 2),
            vec![(one_over_zero.clone(), col(0))],
        );
        let failing_right_key = inner(
            input(left_rows(), 2),
            input(right_rows(), 2),
            vec![(col(1), one_over_zero)],
        );
        let missing_table = inner(
            input(left_rows(), 2),
            JoinInput {
                child: Box::new(SeqScan::new("missing")),
                width: 2,
            },
            on_keys(),
        );

        let cases = [
            (failing_left_key, "22012"),
            (failing_right_key, "22012"),
            (missing_table, "42P01"),
        ];
        for (mut join, sqlstate) in cases {
            assert_eq!(join_err(&mut join).sqlstate(), sqlstate);
        }
    }

    fn outer(join_type: JoinType, left: JoinInput, right: JoinInput) -> HashJoin {
        HashJoin::new(left, right, on_keys(), join_type, None)
    }

    fn l1_to_l3_pairs() -> [[Value; 4]; 4] {
        [
            [text("l1"), int(1), int(1), text("r1")],
            [text("l1"), int(1), int(1), text("r3")],
            [text("l3"), int(1), int(1), text("r1")],
            [text("l3"), int(1), int(1), text("r3")],
        ]
    }

    #[test]
    fn left_join_should_pad_unmatched_left_rows_where_they_are() {
        let mut join = outer(
            JoinType::Left,
            input(left_rows(), 2),
            input(right_rows(), 2),
        );

        let output = run(&mut join);

        let [l1r1, l1r3, l3r1, l3r3] = l1_to_l3_pairs();
        let l2 = [text("l2"), int(2), Value::Null, Value::Null];
        let l4 = [text("l4"), int(9), Value::Null, Value::Null];
        assert_eq!(output, [l1r1, l1r3, l2, l3r1, l3r3, l4]);
    }

    #[test]
    fn right_join_should_add_unmatched_right_rows_at_the_end() {
        let mut join = outer(
            JoinType::Right,
            input(left_rows(), 2),
            input(right_rows(), 2),
        );

        let output = run(&mut join);

        let [l1r1, l1r3, l3r1, l3r3] = l1_to_l3_pairs();
        let r2 = [Value::Null, Value::Null, int(3), text("r2")];
        assert_eq!(output, [l1r1, l1r3, l3r1, l3r3, r2]);
    }

    #[test]
    fn full_join_should_pad_both_sides() {
        let mut join = outer(
            JoinType::Full,
            input(left_rows(), 2),
            input(right_rows(), 2),
        );

        let output = run(&mut join);

        let [l1r1, l1r3, l3r1, l3r3] = l1_to_l3_pairs();
        let l2 = [text("l2"), int(2), Value::Null, Value::Null];
        let l4 = [text("l4"), int(9), Value::Null, Value::Null];
        let r2 = [Value::Null, Value::Null, int(3), text("r2")];
        assert_eq!(output, [l1r1, l1r3, l2, l3r1, l3r3, l4, r2]);
    }

    #[test]
    fn outer_joins_should_pad_with_the_declared_width_of_an_empty_side() {
        let mut left_join = outer(
            JoinType::Left,
            input(vec![vec![text("l"), int(1)]], 2),
            input(vec![], 3),
        );
        let mut right_join = outer(
            JoinType::Right,
            input(vec![], 4),
            input(vec![vec![int(1), text("r")]], 2),
        );

        let from_left = run(&mut left_join);
        let from_right = run(&mut right_join);

        assert_eq!(
            from_left,
            [vec![
                text("l"),
                int(1),
                Value::Null,
                Value::Null,
                Value::Null
            ]]
        );
        assert_eq!(
            from_right,
            [vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                int(1),
                text("r")
            ]]
        );
    }

    #[test]
    fn rows_with_null_keys_should_be_padded_on_both_sides_in_arrival_order() {
        let left = input(vec![vec![text("l"), Value::Null]], 2);
        let right = input(
            vec![
                vec![Value::Null, text("r0")],
                vec![int(5), text("r1")],
                vec![Value::Null, text("r2")],
            ],
            2,
        );
        let mut join = outer(JoinType::Full, left, right);

        let output = run(&mut join);

        assert_eq!(
            output,
            [
                [text("l"), Value::Null, Value::Null, Value::Null],
                [Value::Null, Value::Null, Value::Null, text("r0")],
                [Value::Null, Value::Null, int(5), text("r1")],
                [Value::Null, Value::Null, Value::Null, text("r2")],
            ]
        );
    }

    #[test]
    fn inner_join_should_drop_pairs_whose_predicate_is_not_true() {
        // the right id is r1
        let is_r1 = Expr::binary(BinaryOp::Eq, col(3), Expr::Literal(text("r1")));
        let mut join = HashJoin::new(
            input(left_rows(), 2),
            input(right_rows(), 2),
            on_keys(),
            JoinType::Inner,
            Some(is_r1),
        );

        let output = run(&mut join);

        let [l1r1, _, l3r1, _] = l1_to_l3_pairs();
        assert_eq!(output, [l1r1, l3r1]);
    }

    #[test]
    fn a_right_row_whose_only_pair_fails_the_predicate_should_be_padded() {
        let never = || Some(Expr::Literal(Value::Boolean(false)));
        let join = |join_type| {
            HashJoin::new(
                input(vec![vec![text("l"), int(1)]], 2),
                input(vec![vec![int(1), text("r")]], 2),
                on_keys(),
                join_type,
                never(),
            )
        };

        let from_right = run(&mut join(JoinType::Right));
        let from_full = run(&mut join(JoinType::Full));

        let padded_right = [Value::Null, Value::Null, int(1), text("r")];
        let padded_left = [text("l"), int(1), Value::Null, Value::Null];
        assert_eq!(from_right, std::slice::from_ref(&padded_right));
        assert_eq!(from_full, [padded_left, padded_right]);
    }

    #[test]
    fn a_predicate_that_is_not_boolean_or_fails_should_end_the_join() {
        let one_over_zero =
            Expr::binary(BinaryOp::Div, Expr::Literal(int(1)), Expr::Literal(int(0)));
        let cases = [(col(0), "42804"), (one_over_zero, "22012")];

        for (predicate, sqlstate) in cases {
            let mut join = HashJoin::new(
                input(left_rows(), 2),
                input(right_rows(), 2),
                on_keys(),
                JoinType::Full,
                Some(predicate),
            );

            assert_eq!(join_err(&mut join).sqlstate(), sqlstate);
        }
    }

    #[test]
    fn a_key_of_another_family_against_an_empty_right_input_should_not_fail() {
        let join = |join_type| {
            HashJoin::new(
                input(vec![vec![text("1")]], 1),
                input(vec![], 1),
                vec![(col(0), col(0))],
                join_type,
                None,
            )
        };

        let from_inner = run(&mut join(JoinType::Inner));
        let from_left = run(&mut join(JoinType::Left));

        assert!(from_inner.is_empty());
        assert_eq!(from_left, [[text("1"), Value::Null]]);
    }

    #[test]
    fn dates_and_timestamps_should_join_by_instant() {
        let day = Date::from_ymd(2026, 10, 22).unwrap();
        let next_day = Date::from_ymd(2026, 10, 23).unwrap();
        let midnight = Value::Timestamp(Timestamp::at_midnight(day));
        let ten_o_clock = Value::Timestamp(Timestamp::new(day, 10, 0, 0, 0).unwrap());
        let left = input(vec![vec![Value::Date(day)], vec![Value::Date(next_day)]], 1);
        let right = input(vec![vec![ten_o_clock], vec![midnight.clone()]], 1);

        let output = run(&mut inner(left, right, vec![(col(0), col(0))]));

        assert_eq!(output, [[Value::Date(day), midnight]]);
    }

    #[test]
    fn outer_joins_of_two_empty_inputs_should_give_no_rows() {
        for join_type in [JoinType::Left, JoinType::Right, JoinType::Full] {
            let mut join = outer(join_type, input(vec![], 2), input(vec![], 2));

            assert!(run(&mut join).is_empty(), "{join_type:?}");
        }
    }

    #[test]
    fn a_failing_left_child_should_end_the_join_after_the_build() {
        let missing_left = JoinInput {
            child: Box::new(SeqScan::new("missing")),
            width: 2,
        };
        let mut join = outer(JoinType::Full, missing_left, input(right_rows(), 2));

        let err = join_err(&mut join);

        assert_eq!(err.sqlstate(), "42P01");
    }

    #[test]
    fn joined_rows_should_have_no_rid() {
        let mut db = TestDb::new(4);
        let schema = Schema::new(vec![Column::new("n", DataType::Integer)]).unwrap();
        db.create_table("numbers", schema, &[vec![int(1)], vec![int(2)]]);
        let scan = |table: &str| JoinInput {
            child: Box::new(SeqScan::new(table)),
            width: 1,
        };
        let mut join = inner(scan("numbers"), scan("numbers"), vec![(col(0), col(0))]);

        let output = execute(&mut join, &mut db.ctx()).unwrap();

        assert_eq!(output.len(), 2);
        assert!(output.iter().all(|row| row.rid.is_none()));
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

        /// A key from a small pool, so that keys repeat, as a value of `ty`; sometimes NULL.
        /// Floats are sometimes half-way between integers, which no integer key equals.
        fn number(&mut self, ty: DataType) -> Value {
            if self.below(6) == 0 {
                return Value::Null;
            }
            let whole = self.below(6) as i16;
            let half = if self.below(4) == 0 { 0.5 } else { 0.0 };
            match ty {
                DataType::SmallInt => Value::SmallInt(whole),
                DataType::Integer => Value::Integer(whole.into()),
                DataType::BigInt => Value::BigInt(whole.into()),
                DataType::Real => real(f32::from(whole) + half as f32),
                _ => double(f64::from(whole) + half),
            }
        }

        fn label(&mut self) -> Value {
            match self.below(5) {
                0 => Value::Null,
                n => text(["a", "b", "a ", "c"][n - 1]),
            }
        }

        /// `(id, number, label)` rows for the left, `(number, label, id)` for the right.
        fn side(&mut self, number_type: DataType, is_left: bool) -> Vec<Vec<Value>> {
            (0..self.below(12))
                .map(|id| {
                    let (number, label) = (self.number(number_type), self.label());
                    match is_left {
                        true => vec![int(id as i32), number, label],
                        false => vec![number, label, int(id as i32)],
                    }
                })
                .collect()
        }
    }

    /// The value of a numeric key for the model: small integers and halves are exact `f64`s.
    fn model_number(value: &Value) -> Option<f64> {
        match *value {
            Value::SmallInt(number) => Some(number.into()),
            Value::Integer(number) => Some(number.into()),
            Value::BigInt(number) => Some(number as f64),
            Value::Real(number) => Some(number.get().into()),
            Value::Double(number) => Some(number.get()),
            _ => None,
        }
    }

    fn model_label(value: &Value) -> Option<&str> {
        match value {
            Value::Text(label) => Some(label),
            _ => None,
        }
    }

    /// A nested loop over the two sides: no normalized key, no hash table.
    fn model_join(
        left: &[Vec<Value>],
        right: &[Vec<Value>],
        has_two_keys: bool,
        join_type: JoinType,
        has_predicate: bool,
    ) -> Vec<Vec<Value>> {
        let keys_equal = |left_row: &[Value], right_row: &[Value]| {
            let numbers = model_number(&left_row[1]).zip(model_number(&right_row[0]));
            let labels = model_label(&left_row[2]).zip(model_label(&right_row[1]));
            numbers.is_some_and(|(l, r)| l == r)
                && (!has_two_keys || labels.is_some_and(|(l, r)| l == r))
        };
        // left id <= right id
        let passes = |left_row: &[Value], right_row: &[Value]| {
            !has_predicate || left_row[0].sort_cmp(&right_row[2]).is_le()
        };
        let mut is_right_matched = vec![false; right.len()];
        let mut output = Vec::new();
        for left_row in left {
            let mut has_match = false;
            for (right_row, is_matched) in right.iter().zip(&mut is_right_matched) {
                if keys_equal(left_row, right_row) && passes(left_row, right_row) {
                    has_match = true;
                    *is_matched = true;
                    output.push([left_row.clone(), right_row.clone()].concat());
                }
            }
            if !has_match && matches!(join_type, JoinType::Left | JoinType::Full) {
                output.push([left_row.clone(), vec![Value::Null; 3]].concat());
            }
        }
        if matches!(join_type, JoinType::Right | JoinType::Full) {
            for (right_row, is_matched) in right.iter().zip(is_right_matched) {
                if !is_matched {
                    output.push([vec![Value::Null; 3], right_row.clone()].concat());
                }
            }
        }
        output
    }

    #[test]
    fn random_hash_joins_should_match_a_nested_loop() {
        let mut rng = Rng(0xBF58_476D_1CE4_E5B9);
        let left_types = [DataType::SmallInt, DataType::Integer, DataType::Double];
        let right_types = [DataType::Integer, DataType::BigInt, DataType::Real];
        let join_types = [
            JoinType::Inner,
            JoinType::Left,
            JoinType::Right,
            JoinType::Full,
        ];

        for round in 1..=300 {
            let (left_type, right_type) = (left_types[rng.below(3)], right_types[rng.below(3)]);
            let left = rng.side(left_type, true);
            let right = rng.side(right_type, false);
            let has_two_keys = rng.below(2) == 0;
            let join_type = join_types[rng.below(4)];
            let has_predicate = rng.below(3) == 0;
            let mut keys = vec![(col(1), col(0))];
            if has_two_keys {
                keys.push((col(2), col(1)));
            }
            let predicate = has_predicate.then(|| Expr::binary(BinaryOp::Le, col(0), col(5)));
            let mut join = HashJoin::new(
                input(left.clone(), 3),
                input(right.clone(), 3),
                keys,
                join_type,
                predicate,
            );

            let output = run(&mut join);

            let expected = model_join(&left, &right, has_two_keys, join_type, has_predicate);
            assert_eq!(
                output, expected,
                "round {round}: {join_type:?}, two keys {has_two_keys}, predicate {has_predicate}"
            );
        }
    }
}
