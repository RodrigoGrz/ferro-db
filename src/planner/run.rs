//! Running statements: DDL straight against the catalog, DML through the executors with a
//! fixed plan (`Insert(Values)`, `Update` and `Delete` over `Filter(SeqScan)`). The whole text
//! is parsed before any statement runs; each statement is atomic, but a failure keeps the
//! statements before it (there are no transactions). A `SELECT` runs through its logical and
//! physical plans and gives back all its rows.

use crate::execution::{
    Delete, ExecutionContext, Executor, Expr, Filter, Insert, SeqScan, Update, Values, execute,
};
use crate::sql::{Assignment, ColumnDef, Statement, ast, parse};
use crate::table::{Column, Schema};
use crate::types::Value;

use super::PlannerError;
use super::binder::bind;
use super::physical_plan::plan_physical;
use super::scope::Scope;
use super::select::plan_select;

/// What each statement did: its command tag, which a client prints as it is, or the rows of a
/// query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementOutcome {
    CreateTable,
    CreateIndex,
    Insert(u64),
    Update(u64),
    Delete(u64),
    /// The text of the plan, one node per line.
    Explain(String),
    /// The names of the columns of a `SELECT`, and its rows in the order the plan gives them.
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
}

/// Parses every statement of `sql` (a syntax error runs none), then runs them in order; the
/// first failure stops the rest, and the statements before it stay done.
pub fn run_sql(
    sql: &str,
    ctx: &mut ExecutionContext<'_>,
) -> Result<Vec<StatementOutcome>, PlannerError> {
    let statements = parse(sql)?;
    statements
        .into_iter()
        .map(|statement| run_statement(statement, ctx))
        .collect()
}

/// Runs one parsed statement; `run_sql` is this for each statement of a text, in order.
pub fn run_statement(
    statement: Statement,
    ctx: &mut ExecutionContext<'_>,
) -> Result<StatementOutcome, PlannerError> {
    match statement {
        Statement::Select(select) => query(&select, ctx),
        Statement::Explain { select, is_logical } => {
            let logical = plan_select(&select, ctx.catalog)?;
            let text = match is_logical {
                true => logical.to_string(),
                false => plan_physical(logical, ctx.catalog)?.to_string(),
            };
            Ok(StatementOutcome::Explain(text))
        }
        Statement::CreateTable { name, columns } => create_table(ctx, &name, columns),
        Statement::CreateIndex {
            is_unique: false, ..
        } => Err(PlannerError::Unsupported {
            feature: "CREATE INDEX without UNIQUE",
        }),
        Statement::CreateIndex {
            name,
            table,
            column,
            is_unique: true,
        } => {
            ctx.catalog.create_index(ctx.bpm, &name, &table, &column)?;
            Ok(StatementOutcome::CreateIndex)
        }
        Statement::Insert {
            table,
            columns,
            rows,
        } => insert(ctx, &table, &columns, &rows),
        Statement::Update {
            table,
            assignments,
            selection,
        } => update(ctx, &table, &assignments, selection.as_ref()),
        Statement::Delete { table, selection } => delete(ctx, &table, selection.as_ref()),
    }
}

/// `Insert(Values)`: each row in table order, the columns outside the list `NULL` (the
/// default of a column without `DEFAULT`); the values are assigned to the column types by the
/// table, as any insert.
fn insert(
    ctx: &mut ExecutionContext<'_>,
    table: &str,
    columns: &[String],
    rows: &[Vec<ast::Expr>],
) -> Result<StatementOutcome, PlannerError> {
    let schema = ctx.catalog.table(table)?.schema.clone();
    let targets = target_columns(&schema, table, columns)?;
    let rows = rows
        .iter()
        .map(|row| row_in_table_order(row, &targets, schema.columns().len()))
        .collect::<Result<Vec<_>, _>>()?;
    let count = execute_count(Insert::new(table, Box::new(Values::new(rows))), ctx)?;
    Ok(StatementOutcome::Insert(count))
}

/// The positions the values of each row go to: the listed columns, or every column.
fn target_columns(
    schema: &Schema,
    table: &str,
    columns: &[String],
) -> Result<Vec<usize>, PlannerError> {
    if columns.is_empty() {
        return Ok((0..schema.columns().len()).collect());
    }
    let scope = Scope::table(table, schema);
    let mut targets = Vec::with_capacity(columns.len());
    for column in columns {
        let position = scope.column(None, column)?;
        if targets.contains(&position) {
            return Err(PlannerError::DuplicateColumn {
                column: column.clone(),
            });
        }
        targets.push(position);
    }
    Ok(targets)
}

fn row_in_table_order(
    row: &[ast::Expr],
    targets: &[usize],
    width: usize,
) -> Result<Vec<Expr>, PlannerError> {
    if row.len() != targets.len() {
        return Err(PlannerError::ColumnCountMismatch {
            expected: targets.len(),
            found: row.len(),
        });
    }
    let assigned: Vec<(usize, &ast::Expr)> = targets.iter().copied().zip(row).collect();
    (0..width)
        .map(
            |position| match assigned.iter().find(|(target, _)| *target == position) {
                Some((_, value)) => bind(value, &Scope::empty(), "VALUES"),
                None => Ok(Expr::Literal(Value::Null)),
            },
        )
        .collect()
}

/// `Update(Filter(SeqScan))`; every assignment reads the row as it was before the update.
fn update(
    ctx: &mut ExecutionContext<'_>,
    table: &str,
    assignments: &[Assignment],
    selection: Option<&ast::Expr>,
) -> Result<StatementOutcome, PlannerError> {
    let schema = ctx.catalog.table(table)?.schema.clone();
    let scope = Scope::table(table, &schema);
    let mut bound: Vec<(usize, Expr)> = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        let position = scope.column(None, &assignment.column)?;
        if bound.iter().any(|(target, _)| *target == position) {
            return Err(PlannerError::DuplicateAssignment {
                column: assignment.column.clone(),
            });
        }
        bound.push((position, bind(&assignment.value, &scope, "SET")?));
    }
    let rows = matching_rows(table, selection, &scope)?;
    let count = execute_count(Update::new(table, rows, bound), ctx)?;
    Ok(StatementOutcome::Update(count))
}

/// `Delete(Filter(SeqScan))`.
fn delete(
    ctx: &mut ExecutionContext<'_>,
    table: &str,
    selection: Option<&ast::Expr>,
) -> Result<StatementOutcome, PlannerError> {
    let schema = ctx.catalog.table(table)?.schema.clone();
    let rows = matching_rows(table, selection, &Scope::table(table, &schema))?;
    let count = execute_count(Delete::new(table, rows), ctx)?;
    Ok(StatementOutcome::Delete(count))
}

/// The rows of `table` the `WHERE` keeps, or all of them.
fn matching_rows(
    table: &str,
    selection: Option<&ast::Expr>,
    scope: &Scope<'_>,
) -> Result<Box<dyn Executor>, PlannerError> {
    let scan = Box::new(SeqScan::new(table));
    Ok(match selection {
        Some(selection) => Box::new(Filter::new(scan, bind(selection, scope, "WHERE")?)),
        None => scan,
    })
}

/// The rows of a `SELECT`, through its logical and physical plans, with its column names.
fn query(
    select: &ast::Select,
    ctx: &mut ExecutionContext<'_>,
) -> Result<StatementOutcome, PlannerError> {
    let plan = plan_physical(plan_select(select, ctx.catalog)?, ctx.catalog)?;
    let columns = plan
        .columns()
        .into_iter()
        .map(|column| column.name)
        .collect();
    let mut root = plan.into_executor();
    let rows = execute(root.as_mut(), ctx)?
        .into_iter()
        .map(|row| row.values)
        .collect();
    Ok(StatementOutcome::Rows { columns, rows })
}

/// The number of rows a write executor reports in its single BIGINT row.
fn execute_count(
    mut write: impl Executor,
    ctx: &mut ExecutionContext<'_>,
) -> Result<u64, PlannerError> {
    let rows = execute(&mut write, ctx)?;
    match rows.first().map(|row| row.values.as_slice()) {
        Some([Value::BigInt(count)]) => {
            u64::try_from(*count).map_err(|_| PlannerError::MissingCount)
        }
        _ => Err(PlannerError::MissingCount),
    }
}

fn create_table(
    ctx: &mut ExecutionContext<'_>,
    name: &str,
    columns: Vec<ColumnDef>,
) -> Result<StatementOutcome, PlannerError> {
    let columns = columns
        .into_iter()
        .map(|column| match column.is_not_null {
            true => Column::not_null(column.name, column.data_type),
            false => Column::new(column.name, column.data_type),
        })
        .collect();
    ctx.catalog
        .create_table(ctx.bpm, name, Schema::new(columns)?)?;
    Ok(StatementOutcome::CreateTable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::test_support::TestDb;

    use crate::types::DataType;
    use std::collections::BTreeMap;
    use std::num::NonZeroU16;
    use std::ops::Bound::Unbounded;

    fn run(db: &mut TestDb, sql: &str) -> Vec<StatementOutcome> {
        run_sql(sql, &mut db.ctx()).unwrap_or_else(|err| panic!("{err}: {sql}"))
    }

    fn run_err(db: &mut TestDb, sql: &str) -> PlannerError {
        run_sql(sql, &mut db.ctx()).unwrap_err()
    }

    fn index_keys(db: &mut TestDb, index: &str) -> Vec<Value> {
        let ctx = db.ctx();
        let tree = ctx.catalog.index_tree(ctx.bpm, index).unwrap();
        tree.range(ctx.bpm, Unbounded, Unbounded)
            .unwrap()
            .map(|entry| entry.unwrap().0)
            .collect()
    }

    #[test]
    fn create_table_should_add_the_table_to_the_catalog() {
        let mut db = TestDb::new(8);

        let outcome = run(
            &mut db,
            "CREATE TABLE Users (id INTEGER NOT NULL, name VARCHAR(20))",
        );

        assert_eq!(outcome, [StatementOutcome::CreateTable]);
        let expected = Schema::new(vec![
            Column::not_null("id", DataType::Integer),
            Column::new("name", DataType::Varchar(NonZeroU16::new(20).unwrap())),
        ])
        .unwrap();
        assert_eq!(db.catalog.table("users").unwrap().schema, expected);
    }

    #[test]
    fn a_table_that_cannot_be_created_should_fail() {
        let mut db = TestDb::new(8);
        run(&mut db, "CREATE TABLE t (a INT)");

        let existing = run_err(&mut db, "CREATE TABLE t (b INT)");
        let repeated_column = run_err(&mut db, "CREATE TABLE u (a INT, a BIGINT)");

        assert_eq!(existing.sqlstate(), "42P07");
        assert_eq!(repeated_column.sqlstate(), "42701");
        assert!(db.catalog.table("u").is_err());
    }

    #[test]
    fn create_unique_index_should_index_the_rows_already_there() {
        let mut db = TestDb::new(8);
        run(&mut db, "CREATE TABLE t (id INT, v INT)");
        db.with_table("t", |heap, bpm| {
            for id in [3, 1, 2] {
                heap.insert(bpm, &[Value::Integer(id), Value::Null])
                    .unwrap();
            }
        });

        let outcome = run(&mut db, "CREATE UNIQUE INDEX t_id ON t (id)");

        assert_eq!(outcome, [StatementOutcome::CreateIndex]);
        assert_eq!(
            index_keys(&mut db, "t_id"),
            [Value::Integer(1), Value::Integer(2), Value::Integer(3)]
        );
    }

    #[test]
    fn an_index_that_cannot_be_created_should_fail() {
        let mut db = TestDb::new(8);
        run(
            &mut db,
            "CREATE TABLE t (id INT, v INT); CREATE UNIQUE INDEX t_id ON t (id)",
        );
        db.with_table("t", |heap, bpm| {
            for _ in 0..2 {
                heap.insert(bpm, &[Value::Integer(9), Value::Integer(1)])
                    .unwrap();
            }
        });

        let cases = [
            ("CREATE UNIQUE INDEX t_v ON t (v)", "23505"),
            ("CREATE UNIQUE INDEX u_id ON u (id)", "42P01"),
            ("CREATE UNIQUE INDEX t_z ON t (z)", "42703"),
            ("CREATE UNIQUE INDEX t_id ON t (v)", "42P07"),
            ("CREATE INDEX t_v ON t (v)", "0A000"),
        ];

        for (sql, sqlstate) in cases {
            assert_eq!(run_err(&mut db, sql).sqlstate(), sqlstate, "{sql}");
        }
        assert!(db.catalog.index("t_v").is_err());
    }

    #[test]
    fn a_syntax_error_anywhere_should_run_no_statement() {
        let mut db = TestDb::new(8);

        let err = run_err(&mut db, "CREATE TABLE a (x INT); CREATE TABLE");

        assert_eq!(err.sqlstate(), "42601");
        assert!(db.catalog.table("a").is_err());
    }

    #[test]
    fn select_should_give_its_columns_and_rows() {
        let mut db = TestDb::new(8);

        let outcomes = run(
            &mut db,
            "SELECT 1 AS one, 'a'; CREATE TABLE e (x INTEGER); SELECT count(*), max(x) FROM e",
        );

        assert_eq!(
            outcomes,
            [
                StatementOutcome::Rows {
                    columns: vec!["one".to_string(), "?column?".to_string()],
                    rows: vec![vec![int(1), text("a")]],
                },
                StatementOutcome::CreateTable,
                StatementOutcome::Rows {
                    columns: vec!["count".to_string(), "max".to_string()],
                    rows: vec![vec![Value::BigInt(0), Value::Null]],
                },
            ]
        );
    }

    #[test]
    fn a_select_failing_while_it_runs_should_report_the_executor_error() {
        let mut db = filled_db();

        let err = run_err(&mut db, "SELECT v FROM t WHERE s > 1");

        assert!(matches!(err, PlannerError::Execution(_)), "{err:?}");
        assert_eq!(err.sqlstate(), "42883");
    }

    /// `t (id INTEGER NOT NULL, v INTEGER, s VARCHAR(5))` with the unique index `t_id`.
    fn table_db() -> TestDb {
        let mut db = TestDb::new(8);
        run(
            &mut db,
            "CREATE TABLE t (id INTEGER NOT NULL, v INTEGER, s VARCHAR(5)); \
             CREATE UNIQUE INDEX t_id ON t (id)",
        );
        db
    }

    fn int(value: i32) -> Value {
        Value::Integer(value)
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_string())
    }

    /// The rows of `t`, by id.
    fn stored(db: &mut TestDb) -> Vec<Vec<Value>> {
        let mut rows: Vec<Vec<Value>> = db
            .stored_rows("t")
            .into_iter()
            .map(|row| row.values)
            .collect();
        rows.sort_by(|lhs, rhs| lhs[0].sort_cmp(&rhs[0]));
        rows
    }

    /// Three rows: `(1, 10, 'a')`, `(2, NULL, 'b')`, `(3, 30, NULL)`.
    fn filled_db() -> TestDb {
        let mut db = table_db();
        run(
            &mut db,
            "INSERT INTO t VALUES (1, 10, 'a'), (2, NULL, 'b'), (3, 30, NULL)",
        );
        db
    }

    #[test]
    fn insert_should_add_rows_with_or_without_a_column_list() {
        let mut db = table_db();

        let all_columns = run(&mut db, "INSERT INTO t VALUES (1, 10, 'a'), (2, NULL, 'b')");
        let some_columns = run(&mut db, "INSERT INTO t (s, id) VALUES ('c', 3)");

        assert_eq!(all_columns, [StatementOutcome::Insert(2)]);
        assert_eq!(some_columns, [StatementOutcome::Insert(1)]);
        assert_eq!(
            stored(&mut db),
            [
                vec![int(1), int(10), text("a")],
                vec![int(2), Value::Null, text("b")],
                vec![int(3), Value::Null, text("c")],
            ]
        );
        assert_eq!(index_keys(&mut db, "t_id"), [int(1), int(2), int(3)]);
    }

    #[test]
    fn an_insert_that_fails_should_change_nothing() {
        let mut db = filled_db();
        let before = stored(&mut db);
        let cases = [
            ("INSERT INTO t (v) VALUES (1)", "23502"),
            ("INSERT INTO t VALUES ('x', 1, 'a')", "42804"),
            ("INSERT INTO t VALUES (5, 1, 'toolong')", "22001"),
            ("INSERT INTO t VALUES (4, 0, 'z'), (1, 0, 'z')", "23505"),
            ("INSERT INTO t VALUES (4, 2)", "42601"),
            ("INSERT INTO t (id) VALUES (4, 2)", "42601"),
            ("INSERT INTO t (id, id) VALUES (4, 5)", "42701"),
            ("INSERT INTO t (zz) VALUES (4)", "42703"),
            ("INSERT INTO u VALUES (4)", "42P01"),
            ("INSERT INTO t VALUES (id, 1, 'a')", "42703"),
            ("INSERT INTO t VALUES (count(*), 1, 'a')", "42803"),
            ("INSERT INTO t VALUES (4 / 0, 1, 'a')", "22012"),
        ];

        for (sql, sqlstate) in cases {
            assert_eq!(run_err(&mut db, sql).sqlstate(), sqlstate, "{sql}");
            assert_eq!(stored(&mut db), before, "{sql}");
        }
        assert_eq!(index_keys(&mut db, "t_id"), [int(1), int(2), int(3)]);
    }

    #[test]
    fn update_should_change_the_matching_rows_and_keep_the_index() {
        let mut db = filled_db();

        let some = run(&mut db, "UPDATE t SET v = v + 1 WHERE id >= 2");
        let all = run(&mut db, "UPDATE t SET s = 'x'");
        let key = run(&mut db, "UPDATE t SET id = id + 10 WHERE id = 1");

        assert_eq!(some, [StatementOutcome::Update(2)]);
        assert_eq!(all, [StatementOutcome::Update(3)]);
        assert_eq!(key, [StatementOutcome::Update(1)]);
        assert_eq!(
            stored(&mut db),
            [
                vec![int(2), Value::Null, text("x")],
                vec![int(3), int(31), text("x")],
                vec![int(11), int(10), text("x")],
            ]
        );
        assert_eq!(index_keys(&mut db, "t_id"), [int(2), int(3), int(11)]);
    }

    #[test]
    fn an_update_that_fails_should_change_nothing() {
        let mut db = filled_db();
        let before = stored(&mut db);
        let cases = [
            ("UPDATE t SET v = 1, v = 2", "42601"),
            ("UPDATE t SET zz = 1", "42703"),
            ("UPDATE t SET id = NULL WHERE id = 3", "23502"),
            ("UPDATE t SET id = 2 WHERE id = 1", "23505"),
            ("UPDATE t SET v = u.v", "42P01"),
            ("UPDATE t SET v = 1 WHERE v", "42804"),
            ("UPDATE u SET v = 1", "42P01"),
        ];

        for (sql, sqlstate) in cases {
            assert_eq!(run_err(&mut db, sql).sqlstate(), sqlstate, "{sql}");
            assert_eq!(stored(&mut db), before, "{sql}");
        }
    }

    #[test]
    fn delete_should_remove_the_matching_rows_or_every_row() {
        let mut db = filled_db();

        let some = run(&mut db, "DELETE FROM t WHERE v IS NULL OR id IN (3)");
        let rest = run(&mut db, "DELETE FROM t");

        assert_eq!(some, [StatementOutcome::Delete(2)]);
        assert_eq!(rest, [StatementOutcome::Delete(1)]);
        assert!(stored(&mut db).is_empty());
        assert!(index_keys(&mut db, "t_id").is_empty());
    }

    #[test]
    fn explain_should_give_the_physical_plan_and_explain_logical_the_logical() {
        let mut db = filled_db();

        let outcomes = run(
            &mut db,
            "EXPLAIN SELECT v FROM t WHERE id = 2; EXPLAIN LOGICAL SELECT v FROM t WHERE id = 2; \
             INSERT INTO t VALUES (9, 9, 'z'); SELECT v FROM t WHERE id = 9",
        );
        let unknown = run_err(&mut db, "EXPLAIN SELECT zz FROM t");

        assert_eq!(
            outcomes,
            [
                StatementOutcome::Explain(
                    "Projection: t.v AS v\n  Filter: t.id = 2\n    IndexScan: t (index t_id = 2)"
                        .to_string()
                ),
                StatementOutcome::Explain(
                    "Project: t.v AS v\n  Filter: t.id = 2\n    Scan: t".to_string()
                ),
                StatementOutcome::Insert(1),
                StatementOutcome::Rows {
                    columns: vec!["v".to_string()],
                    rows: vec![vec![int(9)]],
                },
            ]
        );
        assert_eq!(unknown.sqlstate(), "42703");
    }

    #[test]
    fn aggregates_should_be_refused_naming_their_clause() {
        let mut db = filled_db();
        let statements = [
            ("INSERT INTO t VALUES (count(*), 1, 'a')", "VALUES"),
            ("UPDATE t SET v = max(v)", "SET"),
            ("UPDATE t SET v = 1 WHERE sum(v) > 1", "WHERE"),
            ("DELETE FROM t WHERE avg(v) > 1", "WHERE"),
        ];

        let messages = statements.map(|(sql, _)| run_err(&mut db, sql).to_string());

        for ((sql, clause), message) in statements.iter().zip(messages) {
            assert_eq!(
                message,
                format!("aggregate functions are not allowed in {clause}"),
                "{sql}"
            );
        }
    }

    #[test]
    fn a_long_in_list_should_run_on_a_small_stack() {
        let list = (4..100_000).map(|i| i.to_string()).collect::<Vec<_>>();
        let sql = format!("DELETE FROM t WHERE id IN (2, {})", list.join(", "));

        let (outcome, rows) = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let mut db = filled_db();
                let outcome = run(&mut db, &sql);
                (outcome, stored(&mut db))
            })
            .unwrap()
            .join()
            .unwrap();

        assert_eq!(outcome, [StatementOutcome::Delete(1)]);
        let ids: Vec<Value> = rows.into_iter().map(|row| row[0].clone()).collect();
        assert_eq!(ids, [int(1), int(3)]);
    }

    #[test]
    fn every_column_type_should_take_its_literals() {
        let mut db = TestDb::new(8);
        run(
            &mut db,
            "CREATE TABLE t (a BIGINT, b SMALLINT, c DOUBLE PRECISION, d BOOLEAN, e DATE, \
             f TIMESTAMP, g VARCHAR(3))",
        );

        let inserted = run(
            &mut db,
            "INSERT INTO t VALUES \
             (1, 2, 3, TRUE, DATE '2024-01-31', TIMESTAMP '2024-01-31 10:00:00', 'abc'), \
             (9223372036854775807, -32768, 1.5e10, FALSE, NULL, NULL, NULL)",
        );
        let updated = run(&mut db, "UPDATE t SET b = b + 1, c = c * 2 WHERE d");
        let failures = [
            ("INSERT INTO t (b) VALUES (70000)", "22003"),
            ("INSERT INTO t (a) VALUES (1.5)", "42804"),
            ("INSERT INTO t (e) VALUES ('2024-01-31')", "42804"),
            ("INSERT INTO t (g) VALUES ('abcd')", "22001"),
            ("UPDATE t SET b = b - 1 WHERE NOT d", "22003"),
        ]
        .map(|(sql, sqlstate)| (run_err(&mut db, sql).sqlstate(), sqlstate));
        let before_delete = stored(&mut db);
        let deleted = run(&mut db, "DELETE FROM t WHERE e < DATE '2025-01-01'");

        assert_eq!(inserted, [StatementOutcome::Insert(2)]);
        assert_eq!(updated, [StatementOutcome::Update(1)]);
        for (actual, expected) in failures {
            assert_eq!(actual, expected);
        }
        assert_eq!(
            before_delete,
            [
                vec![
                    Value::BigInt(1),
                    Value::SmallInt(3),
                    Value::parse(DataType::Double, "6").unwrap(),
                    Value::Boolean(true),
                    Value::parse(DataType::Date, "2024-01-31").unwrap(),
                    Value::parse(DataType::Timestamp, "2024-01-31 10:00:00").unwrap(),
                    text("abc"),
                ],
                vec![
                    Value::BigInt(i64::MAX),
                    Value::SmallInt(i16::MIN),
                    Value::parse(DataType::Double, "1.5e10").unwrap(),
                    Value::Boolean(false),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ],
            ]
        );
        assert_eq!(deleted, [StatementOutcome::Delete(1)]);
    }

    #[test]
    fn a_write_without_its_count_should_fail() {
        let mut db = TestDb::new(8);
        let no_rows = Values::new(Vec::new());
        let negative = Values::new(vec![vec![Expr::Literal(Value::BigInt(-1))]]);
        let not_a_count = Values::new(vec![vec![Expr::Literal(Value::Integer(1))]]);

        let errors = [no_rows, negative, not_a_count]
            .map(|write| execute_count(write, &mut db.ctx()).unwrap_err());

        for err in errors {
            assert!(matches!(err, PlannerError::MissingCount), "{err:?}");
            assert_eq!(err.sqlstate(), "XX000");
        }
    }

    #[test]
    fn statements_should_run_in_order_until_one_fails() {
        let mut db = table_db();

        let outcomes = run(
            &mut db,
            "INSERT INTO t VALUES (1, 1, 'a'), (2, 2, 'b'); UPDATE t SET v = 0 WHERE id = 2; \
             DELETE FROM t WHERE id = 1",
        );
        let err = run_err(
            &mut db,
            "INSERT INTO t VALUES (5, 5, 'e'); INSERT INTO t VALUES (2, 0, 'z'); \
             INSERT INTO t VALUES (6, 6, 'f')",
        );

        assert_eq!(
            outcomes,
            [
                StatementOutcome::Insert(2),
                StatementOutcome::Update(1),
                StatementOutcome::Delete(1),
            ]
        );
        assert_eq!(err.sqlstate(), "23505");
        assert_eq!(
            stored(&mut db),
            [
                vec![int(2), int(0), text("b")],
                vec![int(5), int(5), text("e")]
            ]
        );
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

        fn int(&mut self, low: i32, high: i32) -> i32 {
            low + self.below((high - low + 1) as usize) as i32
        }

        fn maybe<T>(&mut self, value: impl FnOnce(&mut Rng) -> T) -> Option<T> {
            (self.below(4) != 0).then(|| value(self))
        }

        fn label(&mut self) -> Option<String> {
            const LABELS: [&str; 4] = ["a", "it's", "ação", "ten chars."];
            self.maybe(|rng| LABELS[rng.below(LABELS.len())].to_string())
        }
    }

    /// `(v, s)` of a row.
    type ModelRow = (Option<i32>, Option<String>);

    /// `id → (v, s)`: the table as plain Rust values.
    type Model = BTreeMap<i32, ModelRow>;

    fn sql_int(value: Option<i32>) -> String {
        value.map_or("NULL".to_string(), |value| value.to_string())
    }

    fn sql_text(value: &Option<String>) -> String {
        value.as_ref().map_or("NULL".to_string(), |text| {
            format!("'{}'", text.replace('\'', "''"))
        })
    }

    fn model_rows(model: &Model) -> Vec<Vec<Value>> {
        model
            .iter()
            .map(|(id, (v, s))| {
                vec![
                    int(*id),
                    v.map_or(Value::Null, int),
                    s.as_deref().map_or(Value::Null, text),
                ]
            })
            .collect()
    }

    /// A random statement, the model after it, and the outcome or SQLSTATE it must give.
    fn random_statement(
        rng: &mut Rng,
        model: &Model,
    ) -> (String, Model, Result<StatementOutcome, &'static str>) {
        let mut next = model.clone();
        match rng.below(4) {
            0 => {
                let rows: Vec<(Option<i32>, Option<i32>, Option<String>)> = (0..1 + rng.below(3))
                    .map(|_| {
                        let id = (rng.below(10) != 0).then(|| rng.int(0, 30));
                        (id, rng.maybe(|rng| rng.int(-50, 50)), rng.label())
                    })
                    .collect();
                let values: Vec<String> = rows
                    .iter()
                    .map(|(id, v, s)| {
                        format!("({}, {}, {})", sql_int(*id), sql_int(*v), sql_text(s))
                    })
                    .collect();
                let sql = format!("INSERT INTO t VALUES {}", values.join(", "));
                if rows.iter().any(|(id, _, _)| id.is_none()) {
                    return (sql, model.clone(), Err("23502"));
                }
                for (id, v, s) in &rows {
                    let Some(id) = id else { unreachable!() };
                    if next.insert(*id, (*v, s.clone())).is_some() {
                        return (sql, model.clone(), Err("23505"));
                    }
                }
                (sql, next, Ok(StatementOutcome::Insert(rows.len() as u64)))
            }
            1 => {
                let (low, high, shift) = (rng.int(0, 30), rng.int(0, 30), rng.int(-5, 5));
                let sql = format!("UPDATE t SET v = v + {shift} WHERE id BETWEEN {low} AND {high}");
                let mut count = 0;
                if low <= high {
                    for (_, (v, _)) in next.range_mut(low..=high) {
                        *v = v.map(|v| v + shift);
                        count += 1;
                    }
                }
                (sql, next, Ok(StatementOutcome::Update(count)))
            }
            2 => {
                let (low, high, shift) = (rng.int(0, 30), rng.int(0, 30), rng.int(-3, 3));
                let sql =
                    format!("UPDATE t SET id = id + {shift} WHERE id >= {low} AND id <= {high}");
                let moved: Vec<(i32, ModelRow)> = model
                    .iter()
                    .filter(|(id, _)| (low..=high).contains(*id))
                    .map(|(id, row)| (*id, row.clone()))
                    .collect();
                for (id, _) in &moved {
                    next.remove(id);
                }
                for (id, row) in &moved {
                    if next.insert(id + shift, row.clone()).is_some() {
                        return (sql, model.clone(), Err("23505"));
                    }
                }
                (sql, next, Ok(StatementOutcome::Update(moved.len() as u64)))
            }
            _ => {
                let doomed: Vec<i32> = (0..1 + rng.below(4)).map(|_| rng.int(0, 30)).collect();
                let by_null_v = rng.below(3) == 0;
                let sql = match by_null_v {
                    true => "DELETE FROM t WHERE v IS NULL".to_string(),
                    false => {
                        let list: Vec<String> = doomed.iter().map(i32::to_string).collect();
                        format!("DELETE FROM t WHERE id IN ({})", list.join(", "))
                    }
                };
                let before = next.len();
                next.retain(|id, (v, _)| match by_null_v {
                    true => v.is_some(),
                    false => !doomed.contains(id),
                });
                let count = (before - next.len()) as u64;
                (sql, next, Ok(StatementOutcome::Delete(count)))
            }
        }
    }

    #[test]
    fn random_statements_should_keep_the_table_like_a_model() {
        let mut rng = Rng(0x5851_F42D_4C95_7F2D);
        let mut db = TestDb::new(8);
        run(
            &mut db,
            "CREATE TABLE t (id INTEGER NOT NULL, v INTEGER, s VARCHAR(10)); \
             CREATE UNIQUE INDEX t_id ON t (id)",
        );
        let mut model = Model::new();

        for step in 1..=400 {
            let (sql, next, expected) = random_statement(&mut rng, &model);

            let result = run_sql(&sql, &mut db.ctx());

            match expected {
                Ok(outcome) => {
                    assert_eq!(
                        result.unwrap_or_else(|err| panic!("step {step}: {err}: {sql}")),
                        [outcome],
                        "step {step}: {sql}"
                    );
                    model = next;
                }
                Err(sqlstate) => assert_eq!(
                    result.unwrap_err().sqlstate(),
                    sqlstate,
                    "step {step}: {sql}"
                ),
            }
            assert_eq!(stored(&mut db), model_rows(&model), "step {step}: {sql}");
            let keys: Vec<Value> = model.keys().map(|id| int(*id)).collect();
            assert_eq!(index_keys(&mut db, "t_id"), keys, "step {step}: {sql}");
        }
        assert!(model.len() > 5, "the table should keep some rows");
    }
}
