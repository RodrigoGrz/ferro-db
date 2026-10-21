//! Leaf executors: `Values` produces rows from expressions, `SeqScan` reads a table.

use crate::storage::rid::Rid;

use super::{ExecutionContext, ExecutionError, Executor, Expr, Row};

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
