//! `Filter` keeps the rows whose predicate is TRUE: a `WHERE` drops FALSE and UNKNOWN alike.

use super::{ExecutionContext, ExecutionError, Executor, Expr, Row};

pub struct Filter {
    child: Box<dyn Executor>,
    predicate: Expr,
}

impl Filter {
    pub fn new(child: Box<dyn Executor>, predicate: Expr) -> Self {
        Self { child, predicate }
    }
}

impl Executor for Filter {
    fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError> {
        while let Some(row) = self.child.next(ctx)? {
            if self.predicate.eval(&row.values)?.is_true()? {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::test_support::TestDb;
    use crate::execution::{BinaryOp, Values, execute};
    use crate::types::Value;

    fn int(value: i32) -> Expr {
        Expr::Literal(Value::Integer(value))
    }

    /// Rows `[0]`, `[1]`, `[NULL]`, `[3]`.
    fn numbers() -> Box<dyn Executor> {
        Box::new(Values::new(vec![
            vec![int(0)],
            vec![int(1)],
            vec![Expr::Literal(Value::Null)],
            vec![int(3)],
        ]))
    }

    #[test]
    fn filter_should_keep_only_the_rows_where_the_predicate_is_true() {
        let mut db = TestDb::new(4);
        // c0 > 0: FALSE for 0, UNKNOWN for NULL.
        let predicate = Expr::binary(BinaryOp::Gt, Expr::Column(0), int(0));
        let mut filter = Filter::new(numbers(), predicate);

        let rows = execute(&mut filter, &mut db.ctx()).unwrap();

        let values: Vec<Vec<Value>> = rows.into_iter().map(|row| row.values).collect();
        assert_eq!(values, [[Value::Integer(1)], [Value::Integer(3)]]);
        assert_eq!(filter.next(&mut db.ctx()).unwrap(), None);
    }

    #[test]
    fn filter_with_a_predicate_that_is_not_boolean_should_fail() {
        let mut db = TestDb::new(4);
        let mut filter = Filter::new(numbers(), Expr::Column(0));

        let err = filter.next(&mut db.ctx()).unwrap_err();

        assert_eq!(err.sqlstate(), "42804");
    }

    #[test]
    fn filter_should_pass_the_rid_through() {
        let mut db = TestDb::new(4);
        let schema = crate::table::Schema::new(vec![crate::table::Column::new(
            "n",
            crate::types::DataType::Integer,
        )])
        .unwrap();
        db.create_table(
            "numbers",
            schema,
            &[vec![Value::Integer(5)], vec![Value::Integer(6)]],
        );
        let predicate = Expr::binary(BinaryOp::Eq, Expr::Column(0), int(6));
        let mut filter = Filter::new(
            Box::new(crate::execution::SeqScan::new("numbers")),
            predicate,
        );

        let rows = execute(&mut filter, &mut db.ctx()).unwrap();

        assert_eq!(rows, db.stored_rows("numbers")[1..]);
    }
}
