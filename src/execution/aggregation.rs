//! `Aggregation` groups its child's rows by the `GROUP BY` expressions and hands out one row
//! per group. It is a pipeline breaker: the first call reads the whole child into an
//! in-memory hash table keyed by the group's values, updating one accumulator per aggregate,
//! and later calls hand out the groups in the order each first appeared.
//!
//! Two rows share a group when their keys are not distinct, so all NULL keys form one group;
//! the derived `Eq`/`Hash` of `Value` are exactly that within a type. A key column is assumed
//! to hold one type, as every expression over a schema does: `Integer(1)` and `BigInt(1)`, which
//! only a `Values` built by hand can mix, would form two groups. Every aggregate but
//! `COUNT(*)` skips NULL, and over no values `COUNT` is 0 and the others NULL. `SUM` of
//! integers is BIGINT, of floats the input's type; `AVG` is always DOUBLE PRECISION; `MIN` and
//! `MAX` keep the input's type. The type of an accumulator is fixed by its first non-NULL
//! value, so `SUM`/`AVG` of a non-numeric type fails there (42883).

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::types::{DataType, Finite64, TypeError, Value};

use super::materialized::Materialized;
use super::{ExecutionContext, ExecutionError, Executor, Expr, Row};

#[derive(Debug, Clone, PartialEq)]
pub enum Aggregate {
    CountStar,
    Count(Expr),
    Sum(Expr),
    Avg(Expr),
    Min(Expr),
    Max(Expr),
}

impl Aggregate {
    /// The value this aggregate takes from a row. `COUNT(*)` counts rows, so it takes a value
    /// that is never NULL: it is `COUNT` of a constant.
    fn input(&self, row: &[Value]) -> Result<Value, ExecutionError> {
        match self {
            Aggregate::CountStar => Ok(Value::Boolean(true)),
            Aggregate::Count(expr)
            | Aggregate::Sum(expr)
            | Aggregate::Avg(expr)
            | Aggregate::Min(expr)
            | Aggregate::Max(expr) => expr.eval(row),
        }
    }
}

/// One row per group: the values of `group_by`, then one value per aggregate. Without
/// `group_by`, the whole input is one group, so an empty input still gives one row.
pub struct Aggregation {
    child: Box<dyn Executor>,
    group_by: Vec<Expr>,
    aggregates: Vec<Aggregate>,
    state: Materialized<()>,
}

impl Aggregation {
    pub fn new(child: Box<dyn Executor>, group_by: Vec<Expr>, aggregates: Vec<Aggregate>) -> Self {
        Self {
            child,
            group_by,
            aggregates,
            state: Materialized::Start(()),
        }
    }
}

impl Executor for Aggregation {
    fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError> {
        self.state
            .next(|()| group_rows(self.child.as_mut(), &self.group_by, &self.aggregates, ctx))
    }
}

/// Reads the whole child into the groups, in the order each group first appeared.
fn group_rows(
    child: &mut dyn Executor,
    group_by: &[Expr],
    aggregates: &[Aggregate],
    ctx: &mut ExecutionContext<'_>,
) -> Result<Vec<Row>, ExecutionError> {
    let mut groups = HashMap::new();
    if group_by.is_empty() {
        groups.insert(Vec::new(), Group::new(0, aggregates));
    }
    while let Some(row) = child.next(ctx)? {
        let key = group_by
            .iter()
            .map(|expr| expr.eval(&row.values))
            .collect::<Result<Vec<_>, _>>()?;
        let first_seen = groups.len();
        groups
            .entry(key)
            .or_insert_with(|| Group::new(first_seen, aggregates))
            .add(aggregates, &row.values)?;
    }
    // Sorting the groups, not the rows: cheap next to reading the child.
    let mut groups: Vec<(Vec<Value>, Group)> = groups.into_iter().collect();
    groups.sort_unstable_by_key(|(_, group)| group.first_seen);
    groups
        .into_iter()
        .map(|(key, group)| group.into_row(key))
        .collect()
}

struct Group {
    /// How many groups existed before this one: its place in the output.
    first_seen: usize,
    accumulators: Vec<Accumulator>,
}

impl Group {
    fn new(first_seen: usize, aggregates: &[Aggregate]) -> Self {
        Self {
            first_seen,
            accumulators: aggregates.iter().map(Accumulator::new).collect(),
        }
    }

    fn add(&mut self, aggregates: &[Aggregate], row: &[Value]) -> Result<(), ExecutionError> {
        for (aggregate, accumulator) in aggregates.iter().zip(&mut self.accumulators) {
            accumulator.add(aggregate.input(row)?)?;
        }
        Ok(())
    }

    /// The key, then each aggregate's result. Not a table row, so no `Rid`.
    fn into_row(self, key: Vec<Value>) -> Result<Row, ExecutionError> {
        let mut values = key;
        for accumulator in self.accumulators {
            values.push(accumulator.finish()?);
        }
        Ok(Row { values, rid: None })
    }
}

/// The running state of one aggregate over one group.
enum Accumulator {
    Count(i64),
    Sum(RunningSum),
    Avg { sum: RunningSum, count: i64 },
    Min(Option<Value>),
    Max(Option<Value>),
}

impl Accumulator {
    fn new(aggregate: &Aggregate) -> Self {
        match aggregate {
            Aggregate::CountStar | Aggregate::Count(_) => Accumulator::Count(0),
            Aggregate::Sum(_) => Accumulator::Sum(RunningSum::Empty),
            Aggregate::Avg(_) => Accumulator::Avg {
                sum: RunningSum::Empty,
                count: 0,
            },
            Aggregate::Min(_) => Accumulator::Min(None),
            Aggregate::Max(_) => Accumulator::Max(None),
        }
    }

    fn add(&mut self, value: Value) -> Result<(), ExecutionError> {
        // Set functions eliminate NULL; the input of COUNT(*) never is.
        let Some(ty) = value.data_type() else {
            return Ok(());
        };
        match self {
            Accumulator::Count(count) => increment(count),
            Accumulator::Sum(sum) => sum.add(value, ty, "sum"),
            Accumulator::Avg { sum, count } => {
                // A REAL average sums in DOUBLE PRECISION, the type of its result.
                let value = if ty.is_float() {
                    value.cast(DataType::Double)?
                } else {
                    value
                };
                sum.add(value, ty, "avg")?;
                increment(count)
            }
            Accumulator::Min(least) => keep_if(least, value, Ordering::Less),
            Accumulator::Max(greatest) => keep_if(greatest, value, Ordering::Greater),
        }
    }

    fn finish(self) -> Result<Value, ExecutionError> {
        Ok(match self {
            Accumulator::Count(count) => Value::BigInt(count),
            Accumulator::Sum(sum) => sum.finish()?,
            Accumulator::Avg { sum, count } => sum.average(count)?,
            Accumulator::Min(kept) | Accumulator::Max(kept) => kept.unwrap_or(Value::Null),
        })
    }
}

/// The sum behind `SUM` and `AVG`. Integers add up exactly in an `i128`, checked against
/// BIGINT only at the end, so the order of the rows never decides whether it overflows;
/// floats add up as `Value`s, with their overflow rules. The first value fixes which.
enum RunningSum {
    Empty,
    Integer(i128),
    Float(Value),
}

impl RunningSum {
    fn add(
        &mut self,
        value: Value,
        ty: DataType,
        function: &'static str,
    ) -> Result<(), ExecutionError> {
        if !ty.is_numeric() {
            return Err(ExecutionError::UndefinedFunction { function, arg: ty });
        }
        match (&mut *self, integer_of(&value)) {
            (RunningSum::Empty, Some(number)) => *self = RunningSum::Integer(number.into()),
            (RunningSum::Empty, None) => *self = RunningSum::Float(value),
            (RunningSum::Integer(total), Some(number)) => {
                *total = total
                    .checked_add(number.into())
                    .ok_or(bigint_out_of_range())?;
            }
            // Only a plan built wrong mixes integers and floats: add them as `+` would.
            (RunningSum::Integer(total), None) => {
                *self = RunningSum::Float(bigint(*total)?.add(&value)?);
            }
            (RunningSum::Float(total), _) => *total = total.add(&value)?,
        }
        Ok(())
    }

    fn finish(self) -> Result<Value, ExecutionError> {
        Ok(match self {
            RunningSum::Empty => Value::Null,
            RunningSum::Integer(total) => bigint(total)?,
            RunningSum::Float(total) => total,
        })
    }

    /// The sum over `count` values as DOUBLE PRECISION. An integer sum is rounded to `f64`
    /// before dividing, so an average of huge BIGINTs does not overflow.
    fn average(self, count: i64) -> Result<Value, ExecutionError> {
        Ok(match self {
            RunningSum::Empty => Value::Null,
            RunningSum::Integer(total) => Finite64::new(total as f64 / count as f64)
                .map(Value::Double)
                .ok_or(TypeError::OutOfRange {
                    ty: DataType::Double,
                })?,
            RunningSum::Float(total) => total.div(&Value::BigInt(count))?,
        })
    }
}

fn integer_of(value: &Value) -> Option<i64> {
    match *value {
        Value::SmallInt(number) => Some(number.into()),
        Value::Integer(number) => Some(number.into()),
        Value::BigInt(number) => Some(number),
        _ => None,
    }
}

fn bigint(total: i128) -> Result<Value, TypeError> {
    i64::try_from(total)
        .map(Value::BigInt)
        .map_err(|_| bigint_out_of_range())
}

fn bigint_out_of_range() -> TypeError {
    TypeError::OutOfRange {
        ty: DataType::BigInt,
    }
}

/// Replaces `kept` when `value` compares to it as `wanted`. Values of another family are the
/// comparison's error (42883): only a plan built wrong mixes them in one aggregate.
fn keep_if(kept: &mut Option<Value>, value: Value, wanted: Ordering) -> Result<(), ExecutionError> {
    let is_wanted = match kept {
        None => true,
        Some(current) => value.compare(current)? == Some(wanted),
    };
    if is_wanted {
        *kept = Some(value);
    }
    Ok(())
}

fn increment(count: &mut i64) -> Result<(), ExecutionError> {
    *count = count.checked_add(1).ok_or(bigint_out_of_range())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::test_support::TestDb;
    use crate::execution::{BinaryOp, Delete, Filter, Insert, SeqScan, Values, execute};
    use crate::table::{Column, Schema};
    use crate::types::Timestamp;

    fn int(value: i32) -> Value {
        Value::Integer(value)
    }

    fn bigint(value: i64) -> Value {
        Value::BigInt(value)
    }

    fn rows(rows: Vec<Vec<Value>>) -> Box<dyn Executor> {
        let rows = rows
            .into_iter()
            .map(|row| row.into_iter().map(Expr::Literal).collect())
            .collect();
        Box::new(Values::new(rows))
    }

    fn aggregate(
        input: Vec<Vec<Value>>,
        group_by: Vec<Expr>,
        aggregates: Vec<Aggregate>,
    ) -> Vec<Vec<Value>> {
        let mut db = TestDb::new(4);
        let mut aggregation = Aggregation::new(rows(input), group_by, aggregates);

        let output = execute(&mut aggregation, &mut db.ctx()).unwrap();

        output.into_iter().map(|row| row.values).collect()
    }

    fn col(column: usize) -> Expr {
        Expr::Column(column)
    }

    #[test]
    fn count_star_should_count_every_row_and_count_should_skip_nulls() {
        let input = vec![
            vec![int(1)],
            vec![Value::Null],
            vec![int(3)],
            vec![Value::Null],
        ];

        let output = aggregate(
            input,
            vec![],
            vec![Aggregate::CountStar, Aggregate::Count(col(0))],
        );

        assert_eq!(output, [[bigint(4), bigint(2)]]);
    }

    #[test]
    fn counts_of_an_empty_input_should_be_one_row_of_zeros() {
        let output = aggregate(
            vec![],
            vec![],
            vec![Aggregate::CountStar, Aggregate::Count(col(0))],
        );

        assert_eq!(output, [[bigint(0), bigint(0)]]);
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_string())
    }

    fn date(year: i32, month: u32, day: u32) -> Value {
        Value::Date(crate::types::Date::from_ymd(year, month, day).unwrap())
    }

    fn min_and_max(input: Vec<Value>) -> Vec<Vec<Value>> {
        let input = input.into_iter().map(|value| vec![value]).collect();
        aggregate(
            input,
            vec![],
            vec![Aggregate::Min(col(0)), Aggregate::Max(col(0))],
        )
    }

    #[test]
    fn min_and_max_should_skip_nulls_for_every_comparable_type() {
        let cases = [
            (
                vec![int(5), Value::Null, int(-2), int(9)],
                [int(-2), int(9)],
            ),
            (
                vec![text("b"), text("a "), Value::Null, text("a")],
                [text("a"), text("b")],
            ),
            (
                vec![date(2026, 10, 22), date(1999, 1, 1), Value::Null],
                [date(1999, 1, 1), date(2026, 10, 22)],
            ),
            (
                vec![Value::Boolean(true), Value::Null, Value::Boolean(false)],
                [Value::Boolean(false), Value::Boolean(true)],
            ),
            (
                vec![real(0.5), Value::Null, real(-1.25)],
                [real(-1.25), real(0.5)],
            ),
            (
                vec![double(2.5), double(-0.0), Value::Null],
                [double(0.0), double(2.5)],
            ),
            (
                vec![
                    Value::Timestamp(Timestamp::MAX),
                    Value::Null,
                    Value::Timestamp(Timestamp::MIN),
                ],
                [
                    Value::Timestamp(Timestamp::MIN),
                    Value::Timestamp(Timestamp::MAX),
                ],
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(min_and_max(input), [expected]);
        }
    }

    #[test]
    fn min_and_max_without_values_should_be_null() {
        assert_eq!(min_and_max(vec![]), [[Value::Null, Value::Null]]);
        assert_eq!(min_and_max(vec![Value::Null]), [[Value::Null, Value::Null]]);
    }

    #[test]
    fn min_and_max_of_values_of_another_family_should_fail() {
        for aggregate in [Aggregate::Min(col(0)), Aggregate::Max(col(0))] {
            let err = aggregate_err(one_column(vec![int(1), text("a")]), vec![aggregate]);

            assert_eq!(err.sqlstate(), "42883");
        }
    }

    fn real(value: f32) -> Value {
        Value::Real(crate::types::Finite32::new(value).unwrap())
    }

    fn double(value: f64) -> Value {
        Value::Double(crate::types::Finite64::new(value).unwrap())
    }

    fn one_column(values: Vec<Value>) -> Vec<Vec<Value>> {
        values.into_iter().map(|value| vec![value]).collect()
    }

    fn sum_and_avg(values: Vec<Value>) -> Vec<Vec<Value>> {
        aggregate(
            one_column(values),
            vec![],
            vec![Aggregate::Sum(col(0)), Aggregate::Avg(col(0))],
        )
    }

    fn aggregate_err(input: Vec<Vec<Value>>, aggregates: Vec<Aggregate>) -> ExecutionError {
        let mut db = TestDb::new(4);
        let mut aggregation = Aggregation::new(rows(input), vec![], aggregates);

        aggregation.next(&mut db.ctx()).unwrap_err()
    }

    #[test]
    fn sum_of_every_integer_width_should_be_bigint() {
        let cases = [
            vec![Value::SmallInt(i16::MAX), Value::SmallInt(i16::MAX)],
            vec![int(i32::MAX), int(i32::MAX)],
            vec![bigint(i64::MAX / 2), bigint(i64::MAX / 2)],
        ];
        let expected = [
            2 * i64::from(i16::MAX),
            2 * i64::from(i32::MAX),
            i64::MAX / 2 * 2,
        ];

        for (input, expected) in cases.into_iter().zip(expected) {
            let output = aggregate(one_column(input), vec![], vec![Aggregate::Sum(col(0))]);

            assert_eq!(output, [[bigint(expected)]]);
        }
    }

    #[test]
    fn sum_of_floats_should_keep_the_column_type() {
        let reals = aggregate(
            one_column(vec![real(1.5), real(2.25)]),
            vec![],
            vec![Aggregate::Sum(col(0))],
        );
        let doubles = aggregate(
            one_column(vec![double(1.5), Value::Null, double(2.25)]),
            vec![],
            vec![Aggregate::Sum(col(0))],
        );

        assert_eq!(reals, [[real(3.75)]]);
        assert_eq!(doubles, [[double(3.75)]]);
    }

    #[test]
    fn sum_should_only_fail_when_the_total_does_not_fit() {
        let back_in_range = vec![bigint(i64::MAX), bigint(1), bigint(-1)];
        let out_of_range = vec![bigint(i64::MAX), bigint(1)];

        let output = aggregate(
            one_column(back_in_range),
            vec![],
            vec![Aggregate::Sum(col(0))],
        );
        let err = aggregate_err(one_column(out_of_range), vec![Aggregate::Sum(col(0))]);

        assert_eq!(output, [[bigint(i64::MAX)]]);
        assert_eq!(err.sqlstate(), "22003");
    }

    #[test]
    fn sum_of_floats_that_overflows_should_fail() {
        let err = aggregate_err(
            one_column(vec![double(f64::MAX), double(f64::MAX)]),
            vec![Aggregate::Sum(col(0))],
        );

        assert_eq!(err.sqlstate(), "22003");
    }

    #[test]
    fn avg_of_floats_that_overflows_should_fail() {
        let err = aggregate_err(
            one_column(vec![double(f64::MAX), double(f64::MAX)]),
            vec![Aggregate::Avg(col(0))],
        );

        assert_eq!(err.sqlstate(), "22003");
    }

    /// Only a plan built wrong mixes them; they add up as `+` would.
    #[test]
    fn sum_mixing_integers_and_floats_should_add_like_plus() {
        let integer_first = vec![int(1), double(0.5)];
        let float_first = vec![double(0.5), int(2)];
        let integer_sum_too_large = vec![bigint(i64::MAX), bigint(i64::MAX), double(1.0)];

        let integer_then_float = aggregate(
            one_column(integer_first),
            vec![],
            vec![Aggregate::Sum(col(0))],
        );
        let float_then_integer = aggregate(
            one_column(float_first),
            vec![],
            vec![Aggregate::Sum(col(0))],
        );
        let err = aggregate_err(
            one_column(integer_sum_too_large),
            vec![Aggregate::Sum(col(0))],
        );

        assert_eq!(integer_then_float, [[double(1.5)]]);
        assert_eq!(float_then_integer, [[double(2.5)]]);
        assert_eq!(err.sqlstate(), "22003");
    }

    #[test]
    fn avg_should_be_double_precision() {
        let integers = aggregate(
            one_column(vec![int(1), Value::Null, int(2)]),
            vec![],
            vec![Aggregate::Avg(col(0))],
        );
        let reals = aggregate(
            one_column(vec![real(0.5), real(0.25)]),
            vec![],
            vec![Aggregate::Avg(col(0))],
        );
        let huge = aggregate(
            one_column(vec![bigint(i64::MAX), bigint(i64::MAX)]),
            vec![],
            vec![Aggregate::Avg(col(0))],
        );

        assert_eq!(integers, [[double(1.5)]]);
        assert_eq!(reals, [[double(0.375)]]);
        assert_eq!(huge, [[double(i64::MAX as f64)]]);
    }

    #[test]
    fn sum_and_avg_of_only_nulls_should_be_null() {
        assert_eq!(sum_and_avg(vec![]), [[Value::Null, Value::Null]]);
        assert_eq!(
            sum_and_avg(vec![Value::Null, Value::Null]),
            [[Value::Null, Value::Null]]
        );
    }

    #[test]
    fn sum_and_avg_of_a_type_that_is_not_numeric_should_fail() {
        let cases = [
            (
                Aggregate::Sum(col(0)),
                text("a"),
                "function sum(varchar) does not exist",
            ),
            (
                Aggregate::Avg(col(0)),
                Value::Boolean(true),
                "function avg(boolean) does not exist",
            ),
            (
                Aggregate::Sum(col(0)),
                date(2026, 1, 1),
                "function sum(date) does not exist",
            ),
            (
                Aggregate::Avg(col(0)),
                Value::Timestamp(Timestamp::MIN),
                "function avg(timestamp) does not exist",
            ),
        ];

        for (aggregate, value, message) in cases {
            let err = aggregate_err(one_column(vec![Value::Null, value]), vec![aggregate]);

            assert_eq!(err.sqlstate(), "42883");
            assert_eq!(err.to_string(), message);
        }
    }

    /// `(dept, age, salary)` rows.
    fn staff() -> Vec<Vec<Value>> {
        vec![
            vec![text("eng"), int(30), int(100)],
            vec![text("ops"), int(41), int(70)],
            vec![text("eng"), int(30), Value::Null],
            vec![Value::Null, int(25), int(50)],
            vec![text("eng"), int(52), int(120)],
            vec![Value::Null, int(33), int(60)],
        ]
    }

    #[test]
    fn group_by_should_give_one_row_per_key_in_order_of_first_appearance() {
        let output = aggregate(
            staff(),
            vec![col(0)],
            vec![
                Aggregate::CountStar,
                Aggregate::Sum(col(2)),
                Aggregate::Max(col(1)),
            ],
        );

        assert_eq!(
            output,
            [
                [text("eng"), bigint(3), bigint(220), int(52)],
                [text("ops"), bigint(1), bigint(70), int(41)],
                [Value::Null, bigint(2), bigint(110), int(33)],
            ]
        );
    }

    #[test]
    fn group_by_several_keys_should_group_by_all_of_them() {
        let output = aggregate(
            staff(),
            vec![col(0), col(1)],
            vec![Aggregate::Count(col(2))],
        );

        assert_eq!(
            output,
            [
                [text("eng"), int(30), bigint(1)],
                [text("ops"), int(41), bigint(1)],
                [Value::Null, int(25), bigint(1)],
                [text("eng"), int(52), bigint(1)],
                [Value::Null, int(33), bigint(1)],
            ]
        );
    }

    #[test]
    fn group_by_should_put_every_null_key_in_one_group() {
        let input = one_column(vec![Value::Null, int(1), Value::Null, Value::Null]);

        let output = aggregate(input, vec![col(0)], vec![Aggregate::CountStar]);

        assert_eq!(output, [[Value::Null, bigint(3)], [int(1), bigint(1)]]);
    }

    #[test]
    fn group_by_should_keep_keys_apart_like_is_distinct_from() {
        let cases = [
            (
                vec![text("a"), text("a "), text("a")],
                vec![[text("a"), bigint(2)], [text("a "), bigint(1)]],
            ),
            (
                vec![double(-0.0), double(0.5), double(0.0)],
                vec![[double(0.0), bigint(2)], [double(0.5), bigint(1)]],
            ),
            (
                vec![date(2026, 1, 1), date(2026, 1, 2), date(2026, 1, 1)],
                vec![[date(2026, 1, 1), bigint(2)], [date(2026, 1, 2), bigint(1)]],
            ),
        ];

        for (keys, expected) in cases {
            let output = aggregate(one_column(keys), vec![col(0)], vec![Aggregate::CountStar]);

            assert_eq!(output, expected);
        }
    }

    #[test]
    fn group_by_without_aggregates_should_give_each_key_once() {
        let output = aggregate(staff(), vec![col(0)], vec![]);

        assert_eq!(output, [[text("eng")], [text("ops")], [Value::Null]]);
    }

    #[test]
    fn a_failing_child_should_end_the_aggregation() {
        let mut db = TestDb::new(4);
        let mut aggregation = Aggregation::new(
            Box::new(SeqScan::new("missing")),
            vec![],
            vec![Aggregate::CountStar],
        );

        let err = aggregation.next(&mut db.ctx()).unwrap_err();

        assert_eq!(err.sqlstate(), "42P01");
        assert_eq!(aggregation.next(&mut db.ctx()).unwrap(), None);
    }

    #[test]
    fn group_by_should_accept_an_expression_as_the_key() {
        let input = one_column((0..7).map(int).collect());
        let id_mod_3 = Expr::binary(BinaryOp::Rem, col(0), Expr::Literal(int(3)));

        let output = aggregate(input, vec![id_mod_3], vec![Aggregate::Sum(col(0))]);

        assert_eq!(
            output,
            [
                [int(0), bigint(9)],
                [int(1), bigint(5)],
                [int(2), bigint(7)],
            ]
        );
    }

    #[test]
    fn group_by_of_an_empty_input_should_give_no_rows() {
        let output = aggregate(vec![], vec![col(0)], vec![Aggregate::CountStar]);

        assert!(output.is_empty());
    }

    #[test]
    fn a_failing_expression_should_end_the_aggregation() {
        let one_over_zero = Expr::binary(BinaryOp::Div, Expr::Literal(int(1)), col(0));
        let cases = [
            (vec![one_over_zero.clone()], Aggregate::CountStar),
            (vec![], Aggregate::Sum(one_over_zero)),
        ];

        for (group_by, aggregate) in cases {
            let mut db = TestDb::new(4);
            // Reading on after the error would find the row `[1]`, which does not fail.
            let input = one_column(vec![int(0), int(1)]);
            let mut aggregation = Aggregation::new(rows(input), group_by, vec![aggregate]);

            let err = aggregation.next(&mut db.ctx()).unwrap_err();

            assert_eq!(err.sqlstate(), "22012");
            assert_eq!(aggregation.next(&mut db.ctx()).unwrap(), None);
        }
    }

    #[test]
    fn groups_should_have_no_rid() {
        let mut db = TestDb::new(4);
        let schema = Schema::new(vec![Column::new("n", DataType::Integer)]).unwrap();
        db.create_table("numbers", schema, &one_column(vec![int(1), int(2), int(1)]));
        let mut aggregation = Aggregation::new(
            Box::new(SeqScan::new("numbers")),
            vec![col(0)],
            vec![Aggregate::CountStar],
        );

        let output = execute(&mut aggregation, &mut db.ctx()).unwrap();

        assert_eq!(output.len(), 2);
        assert!(output.iter().all(|row| row.rid.is_none()));
    }

    #[test]
    fn having_should_be_a_filter_over_the_groups() {
        let mut db = TestDb::new(4);
        // SELECT dept, COUNT(*) FROM staff GROUP BY dept HAVING COUNT(*) > 1
        let groups = Aggregation::new(rows(staff()), vec![col(0)], vec![Aggregate::CountStar]);
        let more_than_one = Expr::binary(BinaryOp::Gt, col(1), Expr::Literal(bigint(1)));
        let mut having = Filter::new(Box::new(groups), more_than_one);

        let output = execute(&mut having, &mut db.ctx()).unwrap();

        let values: Vec<Vec<Value>> = output.into_iter().map(|row| row.values).collect();
        assert_eq!(values, [[text("eng"), bigint(3)], [Value::Null, bigint(2)]]);
    }

    #[test]
    fn aggregation_without_group_by_should_end_after_its_one_row() {
        let mut db = TestDb::new(4);
        let mut aggregation =
            Aggregation::new(rows(vec![vec![int(1)]]), vec![], vec![Aggregate::CountStar]);

        let first = aggregation.next(&mut db.ctx()).unwrap();

        assert_eq!(
            first,
            Some(Row {
                values: vec![bigint(1)],
                rid: None
            })
        );
        assert_eq!(aggregation.next(&mut db.ctx()).unwrap(), None);
        assert_eq!(aggregation.next(&mut db.ctx()).unwrap(), None);
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

        /// `None` once in `one_in` draws.
        fn maybe<T>(&mut self, one_in: usize, value: impl FnOnce(&mut Rng) -> T) -> Option<T> {
            (self.below(one_in) != 0).then(|| value(self))
        }

        /// `(dept SMALLINT, region VARCHAR, amount INTEGER, score DOUBLE PRECISION)`, any of
        /// them NULL, from small pools so that groups repeat.
        fn fact(&mut self) -> Vec<Value> {
            const REGIONS: [&str; 4] = ["n", "s", "e", "w"];
            vec![
                self.maybe(6, |rng| rng.below(5) as i16)
                    .map_or(Value::Null, Value::SmallInt),
                self.maybe(5, |rng| text(REGIONS[rng.below(REGIONS.len())]))
                    .unwrap_or(Value::Null),
                self.maybe(4, |rng| rng.below(2001) as i32 - 1000)
                    .map_or(Value::Null, Value::Integer),
                self.maybe(4, |rng| (rng.below(2001) as f64 - 1000.0) / 8.0)
                    .map_or(Value::Null, double),
            ]
        }
    }

    const FACT_AGGREGATES: usize = 10;

    fn fact_aggregates() -> Vec<Aggregate> {
        vec![
            Aggregate::CountStar,
            Aggregate::Count(col(2)),
            Aggregate::Sum(col(2)),
            Aggregate::Avg(col(2)),
            Aggregate::Min(col(2)),
            Aggregate::Max(col(2)),
            Aggregate::Sum(col(3)),
            Aggregate::Avg(col(3)),
            Aggregate::Min(col(1)),
            Aggregate::Max(col(1)),
        ]
    }

    /// The aggregates of one group computed in plain Rust: integers in `i128`, floats in
    /// `f64` in the order of the rows, text by `String`'s byte order.
    #[derive(Default)]
    struct ModelGroup {
        rows: i64,
        amounts: Vec<i32>,
        score_sum: f64,
        score_count: i64,
        regions: Vec<String>,
    }

    impl ModelGroup {
        fn add(&mut self, fact: &[Value]) {
            self.rows += 1;
            if let Value::Integer(amount) = fact[2] {
                self.amounts.push(amount);
            }
            if let Value::Double(score) = fact[3] {
                self.score_sum += score.get();
                self.score_count += 1;
            }
            if let Value::Text(region) = &fact[1] {
                self.regions.push(region.clone());
            }
        }

        fn results(&self) -> Vec<Value> {
            let amount_count = self.amounts.len() as i64;
            let amount_sum: i128 = self.amounts.iter().map(|&amount| i128::from(amount)).sum();
            let has_amounts = !self.amounts.is_empty();
            let has_scores = self.score_count > 0;
            let or_null = |is_defined: bool, value: &dyn Fn() -> Value| match is_defined {
                true => value(),
                false => Value::Null,
            };
            vec![
                bigint(self.rows),
                bigint(amount_count),
                or_null(has_amounts, &|| bigint(amount_sum as i64)),
                or_null(has_amounts, &|| {
                    double(amount_sum as f64 / amount_count as f64)
                }),
                self.amounts
                    .iter()
                    .min()
                    .map_or(Value::Null, |&min| int(min)),
                self.amounts
                    .iter()
                    .max()
                    .map_or(Value::Null, |&max| int(max)),
                or_null(has_scores, &|| double(self.score_sum)),
                or_null(has_scores, &|| {
                    double(self.score_sum / self.score_count as f64)
                }),
                self.regions
                    .iter()
                    .min()
                    .map_or(Value::Null, |min| text(min)),
                self.regions
                    .iter()
                    .max()
                    .map_or(Value::Null, |max| text(max)),
            ]
        }
    }

    fn model_groups(facts: &[Row], key_columns: usize) -> Vec<Vec<Value>> {
        let mut groups: Vec<(Vec<Value>, ModelGroup)> = Vec::new();
        for fact in facts {
            let key = fact.values[..key_columns].to_vec();
            match groups.iter_mut().find(|(group_key, _)| *group_key == key) {
                Some((_, group)) => group.add(&fact.values),
                None => {
                    let mut group = ModelGroup::default();
                    group.add(&fact.values);
                    groups.push((key, group));
                }
            }
        }
        groups
            .into_iter()
            .map(|(mut key, group)| {
                key.extend(group.results());
                key
            })
            .collect()
    }

    fn sorted_by_key(mut groups: Vec<Vec<Value>>, key_columns: usize) -> Vec<Vec<Value>> {
        groups.sort_by(|lhs, rhs| {
            (0..key_columns)
                .map(|column| lhs[column].sort_cmp(&rhs[column]))
                .find(|order| order.is_ne())
                .unwrap_or(Ordering::Equal)
        });
        groups
    }

    fn insert_facts(db: &mut TestDb, facts: Vec<Vec<Value>>) {
        let facts = facts
            .into_iter()
            .map(|fact| fact.into_iter().map(Expr::Literal).collect())
            .collect();
        let mut insert = Insert::new("facts", Box::new(Values::new(facts)));
        execute(&mut insert, &mut db.ctx()).unwrap();
    }

    /// `amount BETWEEN low AND low + 200`.
    fn delete_amounts_from(db: &mut TestDb, low: i32) {
        let predicate = Expr::binary(
            BinaryOp::And,
            Expr::binary(BinaryOp::Ge, col(2), Expr::Literal(int(low))),
            Expr::binary(BinaryOp::Le, col(2), Expr::Literal(int(low + 200))),
        );
        let matching = Filter::new(Box::new(SeqScan::new("facts")), predicate);
        execute(&mut Delete::new("facts", Box::new(matching)), &mut db.ctx()).unwrap();
    }

    #[test]
    fn random_aggregations_should_match_a_model() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut db = TestDb::new(8);
        let schema = Schema::new(vec![
            Column::new("dept", DataType::SmallInt),
            Column::new("region", DataType::VARCHAR_MAX),
            Column::new("amount", DataType::Integer),
            Column::new("score", DataType::Double),
        ])
        .unwrap();
        db.create_table("facts", schema, &[]);

        for round in 1..=60 {
            if rng.below(4) == 0 {
                delete_amounts_from(&mut db, rng.below(2001) as i32 - 1000);
            } else {
                let facts = (0..rng.below(40)).map(|_| rng.fact()).collect();
                insert_facts(&mut db, facts);
            }
            let key_columns = 1 + rng.below(2);
            let group_by = (0..key_columns).map(col).collect();
            let mut aggregation =
                Aggregation::new(Box::new(SeqScan::new("facts")), group_by, fact_aggregates());

            let output = execute(&mut aggregation, &mut db.ctx()).unwrap();

            let expected = model_groups(&db.stored_rows("facts"), key_columns);
            let actual: Vec<Vec<Value>> = output.into_iter().map(|row| row.values).collect();
            assert!(
                actual
                    .iter()
                    .all(|row| row.len() == key_columns + FACT_AGGREGATES)
            );
            assert_eq!(
                sorted_by_key(actual, key_columns),
                sorted_by_key(expected, key_columns),
                "round {round}"
            );
        }
        assert!(db.stored_rows("facts").len() > 100, "the table should grow");
    }
}
