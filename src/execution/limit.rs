//! `Limit` skips `OFFSET` rows and hands out at most `LIMIT` rows, each with its `Rid`.
//!
//! The counts are expressions without columns, evaluated once, on the first call and before
//! the child is pulled, like the bounds of an `IndexScan`. A count is assigned to BIGINT (any
//! integer width; a float or text is 42804), and NULL or a negative count is an invalid row
//! count (2201W for `LIMIT`, 2201X for `OFFSET`), as the standard says for `FETCH FIRST` and
//! `OFFSET`. Once the count is reached the child is never pulled again: `LIMIT 0` reads
//! nothing.

use crate::types::{DataType, Value};

use super::{ExecutionContext, ExecutionError, Executor, Expr, Row, RowCountClause};

pub struct Limit {
    child: Box<dyn Executor>,
    state: LimitState,
}

enum LimitState {
    Start {
        count: Option<Expr>,
        offset: Option<Expr>,
    },
    /// `remaining: None` is no limit.
    Running {
        remaining: Option<u64>,
        to_skip: u64,
    },
    Done,
}

impl Limit {
    /// `count: None` is `LIMIT ALL`; `offset: None` is `OFFSET 0`.
    pub fn new(child: Box<dyn Executor>, count: Option<Expr>, offset: Option<Expr>) -> Self {
        Self {
            child,
            state: LimitState::Start { count, offset },
        }
    }
}

impl Executor for Limit {
    fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError> {
        // `Done` until the next row is in hand, so an error, the end of the child and the end
        // of the count all end the executor.
        let (remaining, mut to_skip) = match std::mem::replace(&mut self.state, LimitState::Done) {
            LimitState::Start { count, offset } => (
                optional_row_count(count.as_ref(), RowCountClause::Limit)?,
                optional_row_count(offset.as_ref(), RowCountClause::Offset)?.unwrap_or(0),
            ),
            LimitState::Running { remaining, to_skip } => (remaining, to_skip),
            LimitState::Done => return Ok(None),
        };
        if remaining == Some(0) {
            return Ok(None);
        }
        while to_skip > 0 {
            if self.child.next(ctx)?.is_none() {
                return Ok(None);
            }
            to_skip -= 1;
        }
        let Some(row) = self.child.next(ctx)? else {
            return Ok(None);
        };
        self.state = LimitState::Running {
            remaining: remaining.map(|count| count - 1),
            to_skip: 0,
        };
        Ok(Some(row))
    }
}

fn optional_row_count(
    expr: Option<&Expr>,
    clause: RowCountClause,
) -> Result<Option<u64>, ExecutionError> {
    expr.map(|expr| row_count(expr, clause)).transpose()
}

/// The value of a `LIMIT` or `OFFSET` count.
pub(super) fn row_count(expr: &Expr, clause: RowCountClause) -> Result<u64, ExecutionError> {
    // Assigned to BIGINT, a value is a BIGINT or NULL.
    match expr.eval(&[])?.assign(DataType::BigInt)? {
        Value::BigInt(count) => u64::try_from(count).map_err(|_| ExecutionError::InvalidRowCount {
            clause,
            count: Some(count),
        }),
        _ => Err(ExecutionError::InvalidRowCount {
            clause,
            count: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::test_support::TestDb;
    use crate::execution::{BinaryOp, SeqScan, Values, execute};
    use crate::table::{Column, Schema};
    use crate::types::{DataType, Finite64, Value};

    fn int(value: i32) -> Expr {
        Expr::Literal(Value::Integer(value))
    }

    fn bigint(value: i64) -> Expr {
        Expr::Literal(Value::BigInt(value))
    }

    /// Rows `[0]` to `[4]`.
    fn five_rows() -> Box<dyn Executor> {
        Box::new(Values::new((0..5).map(|n| vec![int(n)]).collect()))
    }

    /// Rows `[1]`, `[2]`, then one whose expression divides by zero when pulled.
    fn failing_third_row() -> Box<dyn Executor> {
        let one_over_zero = Expr::binary(BinaryOp::Div, int(1), int(0));
        Box::new(Values::new(vec![
            vec![int(1)],
            vec![int(2)],
            vec![one_over_zero],
        ]))
    }

    fn numbers(output: Vec<Row>) -> Vec<i32> {
        output
            .into_iter()
            .map(|row| match row.values[..] {
                [Value::Integer(number)] => number,
                ref other => panic!("{other:?}"),
            })
            .collect()
    }

    fn run(child: Box<dyn Executor>, count: Option<Expr>, offset: Option<Expr>) -> Vec<i32> {
        let mut db = TestDb::new(4);
        let mut limit = Limit::new(child, count, offset);

        let output = execute(&mut limit, &mut db.ctx()).unwrap();

        assert_eq!(limit.next(&mut db.ctx()).unwrap(), None);
        numbers(output)
    }

    fn limit_err(
        child: Box<dyn Executor>,
        count: Option<Expr>,
        offset: Option<Expr>,
    ) -> ExecutionError {
        let mut db = TestDb::new(4);
        let mut limit = Limit::new(child, count, offset);

        let err = limit.next(&mut db.ctx()).unwrap_err();

        assert_eq!(limit.next(&mut db.ctx()).unwrap(), None);
        err
    }

    #[test]
    fn limit_should_skip_the_offset_and_stop_after_the_count() {
        let cases: [(Option<i64>, Option<i64>, &[i32]); 8] = [
            (None, None, &[0, 1, 2, 3, 4]),
            (Some(2), None, &[0, 1]),
            (None, Some(3), &[3, 4]),
            (Some(2), Some(1), &[1, 2]),
            (Some(10), Some(4), &[4]),
            (Some(2), Some(9), &[]),
            (Some(0), None, &[]),
            (Some(5), Some(0), &[0, 1, 2, 3, 4]),
        ];

        for (count, offset, expected) in cases {
            let output = run(five_rows(), count.map(bigint), offset.map(bigint));

            assert_eq!(output, expected, "LIMIT {count:?} OFFSET {offset:?}");
        }
    }

    #[test]
    fn counts_should_accept_every_integer_width() {
        let small = Expr::Literal(Value::SmallInt(1));

        let output = run(five_rows(), Some(int(2)), Some(small));

        assert_eq!(output, [1, 2]);
    }

    #[test]
    fn counts_of_another_type_should_be_a_datatype_mismatch() {
        let others = [
            Value::Double(Finite64::new(1.0).unwrap()),
            Value::Text("1".to_string()),
            Value::Boolean(true),
        ];

        for other in others {
            let as_count = limit_err(five_rows(), Some(Expr::Literal(other.clone())), None);
            let as_offset = limit_err(five_rows(), None, Some(Expr::Literal(other)));

            assert_eq!(as_count.sqlstate(), "42804");
            assert_eq!(as_offset.sqlstate(), "42804");
        }
    }

    #[test]
    fn null_or_negative_counts_should_be_invalid_row_counts() {
        let null = || Some(Expr::Literal(Value::Null));
        let cases = [
            (
                limit_err(five_rows(), null(), None),
                "2201W",
                "LIMIT must not be null",
            ),
            (
                limit_err(five_rows(), None, null()),
                "2201X",
                "OFFSET must not be null",
            ),
            (
                limit_err(five_rows(), Some(bigint(-1)), None),
                "2201W",
                "LIMIT must not be negative, got -1",
            ),
            (
                limit_err(five_rows(), None, Some(int(-5))),
                "2201X",
                "OFFSET must not be negative, got -5",
            ),
        ];

        for (err, sqlstate, message) in cases {
            assert_eq!(err.sqlstate(), sqlstate);
            assert_eq!(err.to_string(), message);
        }
    }

    #[test]
    fn a_failing_count_should_end_the_limit() {
        let one_over_zero = Expr::binary(BinaryOp::Div, int(1), int(0));

        let as_count = limit_err(five_rows(), Some(one_over_zero.clone()), None);
        let as_offset = limit_err(five_rows(), None, Some(one_over_zero));

        assert_eq!(as_count.sqlstate(), "22012");
        assert_eq!(as_offset.sqlstate(), "22012");
    }

    #[test]
    fn a_count_reading_a_column_should_fail() {
        let err = limit_err(five_rows(), Some(Expr::Column(0)), None);

        assert_eq!(err.sqlstate(), "XX000");
    }

    #[test]
    fn limit_should_not_pull_the_child_after_the_count() {
        assert_eq!(run(failing_third_row(), Some(int(2)), None), [1, 2]);
        assert_eq!(run(failing_third_row(), Some(int(1)), Some(int(1))), [2]);
    }

    #[test]
    fn limit_zero_should_not_pull_the_child_at_all() {
        let one_over_zero = Expr::binary(BinaryOp::Div, int(1), int(0));
        let failing_first_row = Box::new(Values::new(vec![vec![one_over_zero]]));

        assert!(run(failing_first_row, Some(int(0)), None).is_empty());
    }

    #[test]
    fn an_invalid_count_should_fail_before_pulling_the_child() {
        let one_over_zero = Expr::binary(BinaryOp::Div, int(1), int(0));
        let failing_first_row = Box::new(Values::new(vec![vec![one_over_zero]]));

        let err = limit_err(failing_first_row, Some(int(-1)), None);

        assert_eq!(err.sqlstate(), "2201W");
    }

    #[test]
    fn limit_should_keep_the_rids() {
        let mut db = TestDb::new(4);
        let schema = Schema::new(vec![Column::new("n", DataType::Integer)]).unwrap();
        let rows: Vec<Vec<Value>> = (0..5).map(|n| vec![Value::Integer(n)]).collect();
        db.create_table("numbers", schema, &rows);
        let mut limit = Limit::new(
            Box::new(SeqScan::new("numbers")),
            Some(int(2)),
            Some(int(1)),
        );

        let output = execute(&mut limit, &mut db.ctx()).unwrap();

        assert_eq!(output, db.stored_rows("numbers")[1..3]);
    }
}
