//! `Projection` computes a new row from expressions over each row of its child.

use super::{ExecutionContext, ExecutionError, Executor, Expr, Row};

pub struct Projection {
    child: Box<dyn Executor>,
    exprs: Vec<Expr>,
}

impl Projection {
    pub fn new(child: Box<dyn Executor>, exprs: Vec<Expr>) -> Self {
        Self { child, exprs }
    }
}

impl Executor for Projection {
    /// Keeps the child's `Rid`: the new row still comes from that table row.
    fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError> {
        let Some(row) = self.child.next(ctx)? else {
            return Ok(None);
        };
        let values = self
            .exprs
            .iter()
            .map(|expr| expr.eval(&row.values))
            .collect::<Result<_, _>>()?;
        Ok(Some(Row {
            values,
            rid: row.rid,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::test_support::TestDb;
    use crate::execution::{BinaryOp, Filter, SeqScan, Values, execute};
    use crate::table::{Column, Schema};
    use crate::types::{DataType, Value};

    fn int(value: i32) -> Expr {
        Expr::Literal(Value::Integer(value))
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_string())
    }

    #[test]
    fn projection_should_compute_each_expression_over_the_row() {
        let mut db = TestDb::new(4);
        let child = Values::new(vec![
            vec![int(2), Expr::Literal(text("a"))],
            vec![int(5), Expr::Literal(text("b"))],
        ]);
        let exprs = vec![
            Expr::Column(1),
            Expr::binary(BinaryOp::Mul, Expr::Column(0), int(10)),
            int(0),
        ];

        let rows = execute(&mut Projection::new(Box::new(child), exprs), &mut db.ctx()).unwrap();

        let values: Vec<Vec<Value>> = rows.into_iter().map(|row| row.values).collect();
        assert_eq!(
            values,
            [
                vec![text("a"), Value::Integer(20), Value::Integer(0)],
                vec![text("b"), Value::Integer(50), Value::Integer(0)],
            ]
        );
    }

    #[test]
    fn projection_should_report_a_failing_expression() {
        let mut db = TestDb::new(4);
        let child = Values::new(vec![vec![int(0)]]);
        let mut projection = Projection::new(
            Box::new(child),
            vec![Expr::binary(BinaryOp::Div, int(1), Expr::Column(0))],
        );

        let err = projection.next(&mut db.ctx()).unwrap_err();

        assert_eq!(err.sqlstate(), "22012");
    }

    #[test]
    fn a_pipeline_should_scan_filter_and_project_a_table() {
        let mut db = TestDb::new(2);
        let schema = Schema::new(vec![
            Column::not_null("id", DataType::Integer),
            Column::new("name", DataType::VARCHAR_MAX),
        ])
        .unwrap();
        let rows: Vec<Vec<Value>> = (0..60)
            .map(|id| {
                vec![
                    Value::Integer(id),
                    if id % 4 == 0 {
                        Value::Null
                    } else {
                        text(&format!("u{id}"))
                    },
                ]
            })
            .collect();
        db.create_table("users", schema, &rows);
        // SELECT name || '!', id FROM users WHERE id >= 50 AND name IS NOT NULL
        let predicate = Expr::binary(
            BinaryOp::And,
            Expr::binary(BinaryOp::Ge, Expr::Column(0), int(50)),
            Expr::IsNull {
                expr: Box::new(Expr::Column(1)),
                negated: true,
            },
        );
        let filter = Filter::new(Box::new(SeqScan::new("users")), predicate);
        let exprs = vec![
            Expr::binary(BinaryOp::Concat, Expr::Column(1), Expr::Literal(text("!"))),
            Expr::Column(0),
        ];
        let mut query = Projection::new(Box::new(filter), exprs);

        let result = execute(&mut query, &mut db.ctx()).unwrap();

        let expected: Vec<Vec<Value>> = (50..60)
            .filter(|id| id % 4 != 0)
            .map(|id| vec![text(&format!("u{id}!")), Value::Integer(id)])
            .collect();
        let values: Vec<Vec<Value>> = result.iter().map(|row| row.values.clone()).collect();
        assert_eq!(values, expected);
        let stored = db.stored_rows("users");
        assert!(
            result
                .iter()
                .all(|row| stored.iter().any(|stored| stored.rid == row.rid)),
            "rids kept"
        );
    }
}
