//! Executors that change a table: `Insert`, `Delete` and `Update`. Each one returns a single
//! row with the number of rows it changed (a `BIGINT`), as BusTub's do.
//!
//! **Two phases.** An executor first reads every row its child produces, then validates the
//! whole statement (types, NOT NULL, sizes, unique keys against the indexes and among the new
//! rows) and only then writes. Reading first solves the Halloween problem (an update that
//! moves a row further down the heap would meet it again in the scan); validating first
//! makes a statement that fails on its data or a constraint leave no trace. Only an I/O
//! failure while writing can leave a statement half done, until there is a WAL.
//!
//! The indexes of the table are kept in step: every write goes to the heap and to each index.

use std::collections::HashSet;

use crate::index::IndexError;
use crate::storage::rid::Rid;
use crate::types::Value;

use super::{ExecutionContext, ExecutionError, Executor, Expr, Row, execute};

/// What the three executors share: the table they change, their child, and whether the
/// statement has run (its count is returned once).
struct Statement {
    table: String,
    child: Box<dyn Executor>,
    has_run: bool,
}

impl Statement {
    fn new(table: &str, child: Box<dyn Executor>) -> Self {
        Self {
            table: table.to_string(),
            child,
            has_run: false,
        }
    }

    /// Every row of the child, the first time (phase one); `None` once the statement has run.
    fn read_child(
        &mut self,
        ctx: &mut ExecutionContext<'_>,
    ) -> Result<Option<Vec<Row>>, ExecutionError> {
        if self.has_run {
            return Ok(None);
        }
        self.has_run = true;
        Ok(Some(execute(self.child.as_mut(), ctx)?))
    }
}

pub struct Insert {
    statement: Statement,
}

impl Insert {
    pub fn new(table: &str, child: Box<dyn Executor>) -> Self {
        Self {
            statement: Statement::new(table, child),
        }
    }
}

impl Executor for Insert {
    fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError> {
        let Some(rows) = self.statement.read_child(ctx)? else {
            return Ok(None);
        };
        let table = self.statement.table.as_str();

        let heap = ctx.catalog.table_heap(ctx.bpm, table)?;
        let new_rows = rows
            .iter()
            .map(|row| heap.prepare(ctx.bpm, &row.values))
            .collect::<Result<Vec<_>, _>>()?;
        let indexes = table_indexes(ctx, table)?;
        check_unique_keys(ctx, &indexes, &new_rows, &HashSet::new())?;

        for row in &new_rows {
            let heap = ctx.catalog.table_heap(ctx.bpm, table)?;
            let rid = heap.insert(ctx.bpm, row)?;
            insert_keys(ctx, &indexes, row, rid)?;
        }
        Ok(Some(count_row(new_rows.len())))
    }
}

/// Deletes the rows its child produces, by `Rid`. The keys taken out of the indexes are those
/// of the stored row, whatever values the child computed.
pub struct Delete {
    statement: Statement,
}

impl Delete {
    pub fn new(table: &str, child: Box<dyn Executor>) -> Self {
        Self {
            statement: Statement::new(table, child),
        }
    }
}

impl Executor for Delete {
    fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError> {
        let Some(rows) = self.statement.read_child(ctx)? else {
            return Ok(None);
        };
        let table = self.statement.table.as_str();

        let doomed = stored_rows(ctx, table, &distinct_rids(&rows)?)?;
        let indexes = table_indexes(ctx, table)?;

        for (rid, row) in &doomed {
            remove_keys(ctx, &indexes, row)?;
            let heap = ctx.catalog.table_heap(ctx.bpm, table)?;
            heap.delete(ctx.bpm, *rid)?;
        }
        Ok(Some(count_row(doomed.len())))
    }
}

/// Sets columns of the rows its child produces: each `(column, expr)` assignment is
/// evaluated over the stored row **before** the update, all at once (`SET a = b, b = a` swaps).
pub struct Update {
    statement: Statement,
    assignments: Vec<(usize, Expr)>,
}

impl Update {
    pub fn new(table: &str, child: Box<dyn Executor>, assignments: Vec<(usize, Expr)>) -> Self {
        Self {
            statement: Statement::new(table, child),
            assignments,
        }
    }
}

impl Executor for Update {
    fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError> {
        let Some(rows) = self.statement.read_child(ctx)? else {
            return Ok(None);
        };
        let table = self.statement.table.as_str();

        let old_rows = stored_rows(ctx, table, &distinct_rids(&rows)?)?;
        let heap = ctx.catalog.table_heap(ctx.bpm, table)?;
        let new_rows = old_rows
            .iter()
            .map(|(_, old)| {
                let assigned = assign(&self.assignments, old)?;
                Ok(heap.prepare(ctx.bpm, &assigned)?)
            })
            .collect::<Result<Vec<_>, ExecutionError>>()?;
        let indexes = table_indexes(ctx, table)?;
        let replaced: HashSet<Rid> = old_rows.iter().map(|(rid, _)| *rid).collect();
        check_unique_keys(ctx, &indexes, &new_rows, &replaced)?;

        let mut changes = Vec::with_capacity(old_rows.len());
        for ((rid, old), new) in old_rows.iter().zip(&new_rows) {
            let heap = ctx.catalog.table_heap(ctx.bpm, table)?;
            let new_rid = heap.update(ctx.bpm, *rid, new)?;
            changes.push(RowChange {
                old,
                old_rid: *rid,
                new,
                new_rid,
            });
        }
        move_keys(ctx, &indexes, &changes)?;
        Ok(Some(count_row(changes.len())))
    }
}

/// `old` with each assigned column replaced by its expression, evaluated over `old`.
fn assign(assignments: &[(usize, Expr)], old: &[Value]) -> Result<Vec<Value>, ExecutionError> {
    let mut new = old.to_vec();
    for (column, expr) in assignments {
        let value = expr.eval(old)?;
        let slot = new
            .get_mut(*column)
            .ok_or(ExecutionError::ColumnOutOfRange {
                column: *column,
                len: old.len(),
            })?;
        *slot = value;
    }
    Ok(new)
}

struct RowChange<'a> {
    old: &'a [Value],
    old_rid: Rid,
    new: &'a [Value],
    new_rid: Rid,
}

impl RowChange<'_> {
    /// Whether `index` needs its entry for this row rewritten: a new key or a new `Rid`.
    fn moves_entry(&self, index: &TableIndex) -> bool {
        let is_same_key = match (key_of(self.old, index), key_of(self.new, index)) {
            (Some(old), Some(new)) => old.sort_cmp(new).is_eq(),
            (None, None) => true,
            _ => false,
        };
        !is_same_key || self.old_rid != self.new_rid
    }
}

/// Rewrites the entries that changed, index by index: every old key out first, then every new
/// key in, so keys swapped between rows of the statement never meet in the tree.
fn move_keys(
    ctx: &mut ExecutionContext<'_>,
    indexes: &[TableIndex],
    changes: &[RowChange<'_>],
) -> Result<(), ExecutionError> {
    for index in indexes {
        let moving: Vec<&RowChange<'_>> = changes
            .iter()
            .filter(|change| change.moves_entry(index))
            .collect();
        let tree = ctx.catalog.index_tree(ctx.bpm, &index.name)?;
        for change in &moving {
            if let Some(key) = key_of(change.old, index) {
                tree.remove(ctx.bpm, key)?;
            }
        }
        for change in &moving {
            if let Some(key) = key_of(change.new, index) {
                tree.insert(ctx.bpm, key, change.new_rid)?;
            }
        }
    }
    Ok(())
}

/// The `Rid` of each row, in order and once each: Delete and Update act on table rows.
fn distinct_rids(rows: &[Row]) -> Result<Vec<Rid>, ExecutionError> {
    let mut seen = HashSet::new();
    let mut rids = Vec::with_capacity(rows.len());
    for row in rows {
        let rid = row.rid.ok_or(ExecutionError::MissingRid)?;
        if seen.insert(rid) {
            rids.push(rid);
        }
    }
    Ok(rids)
}

/// The rows as stored, read before anything changes: a `Rid` that is gone fails here, and the
/// keys taken out of the indexes are those of the stored row.
fn stored_rows(
    ctx: &mut ExecutionContext<'_>,
    table: &str,
    rids: &[Rid],
) -> Result<Vec<(Rid, Vec<Value>)>, ExecutionError> {
    let heap = ctx.catalog.table_heap(ctx.bpm, table)?;
    rids.iter()
        .map(|&rid| Ok((rid, heap.get(ctx.bpm, rid)?)))
        .collect()
}

/// An index of the table being changed: its name and the position of its key column.
struct TableIndex {
    name: String,
    column: usize,
}

fn table_indexes(
    ctx: &ExecutionContext<'_>,
    table: &str,
) -> Result<Vec<TableIndex>, ExecutionError> {
    Ok(ctx
        .catalog
        .table_indexes(table)?
        .into_iter()
        .map(|index| TableIndex {
            name: index.name.clone(),
            column: usize::from(index.column),
        })
        .collect())
}

/// The key of `row` for `index`, or `None` for a `NULL` key, which no index holds.
fn key_of<'a>(row: &'a [Value], index: &TableIndex) -> Option<&'a Value> {
    row.get(index.column).filter(|key| !key.is_null())
}

/// Every index must stay unique once the statement is done: no two new rows share a key,
/// and no new row takes a key held by a row the statement does not `replace`. Checked before
/// any write, with the same key checks as an insert into the tree.
fn check_unique_keys(
    ctx: &mut ExecutionContext<'_>,
    indexes: &[TableIndex],
    new_rows: &[Vec<Value>],
    replaced: &HashSet<Rid>,
) -> Result<(), ExecutionError> {
    for index in indexes {
        let tree = ctx.catalog.index_tree(ctx.bpm, &index.name)?;
        let mut keys: Vec<&Value> = new_rows
            .iter()
            .filter_map(|row| key_of(row, index))
            .collect();
        for key in &keys {
            tree.check_key(key)?;
            if let Some(owner) = tree.get(ctx.bpm, key)?
                && !replaced.contains(&owner)
            {
                return Err(IndexError::duplicate_key(key).into());
            }
        }
        keys.sort_by(|lhs, rhs| lhs.sort_cmp(rhs));
        if let Some(pair) = keys
            .windows(2)
            .find(|pair| pair[0].sort_cmp(pair[1]).is_eq())
        {
            return Err(IndexError::duplicate_key(pair[0]).into());
        }
    }
    Ok(())
}

fn insert_keys(
    ctx: &mut ExecutionContext<'_>,
    indexes: &[TableIndex],
    row: &[Value],
    rid: Rid,
) -> Result<(), ExecutionError> {
    for index in indexes {
        if let Some(key) = key_of(row, index) {
            let tree = ctx.catalog.index_tree(ctx.bpm, &index.name)?;
            tree.insert(ctx.bpm, key, rid)?;
        }
    }
    Ok(())
}

fn remove_keys(
    ctx: &mut ExecutionContext<'_>,
    indexes: &[TableIndex],
    row: &[Value],
) -> Result<(), ExecutionError> {
    for index in indexes {
        if let Some(key) = key_of(row, index) {
            let tree = ctx.catalog.index_tree(ctx.bpm, &index.name)?;
            tree.remove(ctx.bpm, key)?;
        }
    }
    Ok(())
}

fn count_row(count: usize) -> Row {
    Row {
        values: vec![Value::BigInt(i64::try_from(count).unwrap_or(i64::MAX))],
        rid: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::Expr;
    use crate::execution::test_support::TestDb;
    use crate::execution::{IndexScan, SeqScan, Values, execute};
    use crate::table::{Column, Schema};
    use crate::types::{DataType, Value};
    use std::num::NonZeroU16;
    use std::ops::Bound;

    fn varchar(max_chars: u16) -> DataType {
        DataType::Varchar(NonZeroU16::new(max_chars).unwrap())
    }

    /// `accounts (id INTEGER NOT NULL, email VARCHAR(300), name VARCHAR(5))` with unique
    /// indexes on `id` and `email`, and `rows` already in it.
    fn accounts_db(rows: &[(i32, Option<&str>, &str)]) -> TestDb {
        let mut db = TestDb::new(16);
        let schema = Schema::new(vec![
            Column::not_null("id", DataType::Integer),
            Column::new("email", varchar(300)),
            Column::new("name", varchar(5)),
        ])
        .unwrap();
        let rows: Vec<Vec<Value>> = rows
            .iter()
            .map(|&(id, email, name)| account(id, email, name))
            .collect();
        db.create_table("accounts", schema, &rows);
        let ctx = db.ctx();
        ctx.catalog
            .create_index(ctx.bpm, "accounts_id", "accounts", "id")
            .unwrap();
        let ctx = db.ctx();
        ctx.catalog
            .create_index(ctx.bpm, "accounts_email", "accounts", "email")
            .unwrap();
        db
    }

    fn account(id: i32, email: Option<&str>, name: &str) -> Vec<Value> {
        vec![
            Value::Integer(id),
            email.map_or(Value::Null, |email| Value::Text(email.to_string())),
            Value::Text(name.to_string()),
        ]
    }

    fn literals(row: Vec<Value>) -> Vec<Expr> {
        row.into_iter().map(Expr::Literal).collect()
    }

    fn values(rows: Vec<Vec<Value>>) -> Box<dyn Executor> {
        Box::new(Values::new(rows.into_iter().map(literals).collect()))
    }

    fn count(rows: Vec<Row>) -> i64 {
        match rows.as_slice() {
            [Row { values, rid: None }] => match values.as_slice() {
                [Value::BigInt(count)] => *count,
                other => panic!("not a count: {other:?}"),
            },
            other => panic!("not one count row: {other:?}"),
        }
    }

    fn index_entries(db: &mut TestDb, name: &str) -> Vec<(Value, crate::storage::rid::Rid)> {
        let ctx = db.ctx();
        let tree = ctx.catalog.index_tree(ctx.bpm, name).unwrap();
        tree.range(ctx.bpm, Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect()
    }

    /// Every index of `accounts` holds exactly the non-null keys of its column, with the
    /// `Rid` of their row.
    fn assert_indexes_match_table(db: &mut TestDb) {
        let rows = db.stored_rows("accounts");
        for (name, column) in [("accounts_id", 0), ("accounts_email", 1)] {
            let mut expected: Vec<(Value, crate::storage::rid::Rid)> = rows
                .iter()
                .filter(|row| !row.values[column].is_null())
                .map(|row| (row.values[column].clone(), row.rid.unwrap()))
                .collect();
            expected.sort_by(|lhs, rhs| lhs.0.sort_cmp(&rhs.0));
            assert_eq!(index_entries(db, name), expected, "{name}");
        }
    }

    type IndexEntries = Vec<(Value, crate::storage::rid::Rid)>;

    /// The table and its indexes, to compare before and after a refused statement.
    fn snapshot(db: &mut TestDb) -> (Vec<Row>, IndexEntries, IndexEntries) {
        (
            db.stored_rows("accounts"),
            index_entries(db, "accounts_id"),
            index_entries(db, "accounts_email"),
        )
    }

    #[test]
    fn insert_should_add_the_rows_assigned_and_index_them() {
        let mut db = accounts_db(&[(1, Some("a@x"), "ana")]);
        let new_rows = vec![
            vec![
                Value::SmallInt(2),
                Value::Text("b@x".to_string()),
                Value::Text("bia    ".to_string()),
            ],
            account(3, None, "caio"),
        ];

        let inserted = execute(
            &mut Insert::new("accounts", values(new_rows)),
            &mut db.ctx(),
        )
        .unwrap();

        assert_eq!(count(inserted), 2);
        let stored: Vec<Vec<Value>> = db
            .stored_rows("accounts")
            .into_iter()
            .map(|row| row.values)
            .collect();
        assert_eq!(
            stored,
            [
                account(1, Some("a@x"), "ana"),
                account(2, Some("b@x"), "bia  "),
                account(3, None, "caio")
            ]
        );
        assert_indexes_match_table(&mut db);
    }

    #[test]
    fn insert_of_a_table_into_itself_should_copy_each_row_once() {
        let mut db = TestDb::new(16);
        let schema = Schema::new(vec![Column::new("n", DataType::Integer)]).unwrap();
        let rows: Vec<Vec<Value>> = (0..40).map(|n| vec![Value::Integer(n)]).collect();
        db.create_table("log", schema, &rows);

        let inserted = execute(
            &mut Insert::new("log", Box::new(SeqScan::new("log"))),
            &mut db.ctx(),
        )
        .unwrap();

        assert_eq!(count(inserted), 40);
        assert_eq!(db.stored_rows("log").len(), 80);
    }

    #[test]
    fn insert_with_a_repeated_key_should_change_nothing() {
        let mut db = accounts_db(&[(1, Some("a@x"), "ana")]);
        let before = snapshot(&mut db);
        let statements = [
            // Against the index, after a valid row.
            vec![
                account(5, Some("e@x"), "eva"),
                account(1, Some("z@x"), "zed"),
            ],
            // Between rows of the same statement.
            vec![
                account(7, Some("g@x"), "gil"),
                account(8, Some("g@x"), "gus"),
            ],
        ];

        for rows in statements {
            let err =
                execute(&mut Insert::new("accounts", values(rows)), &mut db.ctx()).unwrap_err();

            assert_eq!(err.sqlstate(), "23505", "{err:?}");
            assert_eq!(snapshot(&mut db), before);
        }
    }

    #[test]
    fn insert_with_a_row_the_table_refuses_should_change_nothing() {
        let mut db = accounts_db(&[(1, Some("a@x"), "ana")]);
        let before = snapshot(&mut db);
        let good = account(5, Some("e@x"), "eva");
        let cases = [
            (vec![Value::Null, Value::Null, Value::Null], "23502"),
            (
                vec![Value::Text("9".to_string()), Value::Null, Value::Null],
                "42804",
            ),
            (account(6, None, "toolong"), "22001"),
            (vec![Value::Integer(6)], "42601"),
            (account(6, Some(&"x".repeat(200)), "big"), "54000"),
        ];

        for (bad, sqlstate) in cases {
            let rows = vec![good.clone(), bad];

            let err =
                execute(&mut Insert::new("accounts", values(rows)), &mut db.ctx()).unwrap_err();

            assert_eq!(err.sqlstate(), sqlstate, "{err:?}");
            assert_eq!(snapshot(&mut db), before);
        }
    }

    #[test]
    fn insert_should_leave_null_keys_out_of_the_index() {
        let mut db = accounts_db(&[]);
        let rows = vec![
            account(1, None, "a"),
            account(2, None, "b"),
            account(3, Some("c@x"), "c"),
        ];

        execute(&mut Insert::new("accounts", values(rows)), &mut db.ctx()).unwrap();

        assert_eq!(index_entries(&mut db, "accounts_email").len(), 1);
        assert_indexes_match_table(&mut db);
    }

    #[test]
    fn insert_into_an_unknown_table_should_fail() {
        let mut db = TestDb::new(4);

        let err = execute(
            &mut Insert::new("missing", values(Vec::new())),
            &mut db.ctx(),
        )
        .unwrap_err();

        assert_eq!(err.sqlstate(), "42P01");
    }

    /// A child that hands out exactly these rows (to send `Rid`s a scan never would).
    struct Rows(std::vec::IntoIter<Row>);

    impl Executor for Rows {
        fn next(&mut self, _: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError> {
            Ok(self.0.next())
        }
    }

    fn five_accounts() -> TestDb {
        accounts_db(&[
            (1, Some("a@x"), "ana"),
            (2, None, "bia"),
            (3, Some("c@x"), "caio"),
            (4, Some("d@x"), "davi"),
            (5, None, "eva"),
        ])
    }

    fn ids(db: &mut TestDb) -> Vec<Value> {
        db.stored_rows("accounts")
            .into_iter()
            .map(|row| row.values[0].clone())
            .collect()
    }

    fn id_at_least(id: i32) -> Box<dyn Executor> {
        let predicate = Expr::binary(
            crate::execution::BinaryOp::Ge,
            Expr::Column(0),
            Expr::Literal(Value::Integer(id)),
        );
        Box::new(crate::execution::Filter::new(
            Box::new(SeqScan::new("accounts")),
            predicate,
        ))
    }

    #[test]
    fn delete_should_remove_the_matching_rows_and_their_keys() {
        let mut db = five_accounts();

        let deleted = execute(&mut Delete::new("accounts", id_at_least(3)), &mut db.ctx()).unwrap();

        assert_eq!(count(deleted), 3);
        assert_eq!(ids(&mut db), [Value::Integer(1), Value::Integer(2)]);
        assert_indexes_match_table(&mut db);
    }

    #[test]
    fn delete_of_every_row_should_empty_the_table_and_its_indexes() {
        let mut db = five_accounts();

        let deleted = execute(
            &mut Delete::new("accounts", Box::new(SeqScan::new("accounts"))),
            &mut db.ctx(),
        )
        .unwrap();

        assert_eq!(count(deleted), 5);
        assert!(db.stored_rows("accounts").is_empty());
        assert!(index_entries(&mut db, "accounts_id").is_empty());
        assert!(index_entries(&mut db, "accounts_email").is_empty());
    }

    #[test]
    fn delete_of_nothing_should_change_nothing() {
        let mut db = five_accounts();
        let before = snapshot(&mut db);

        let deleted =
            execute(&mut Delete::new("accounts", id_at_least(10)), &mut db.ctx()).unwrap();

        assert_eq!(count(deleted), 0);
        assert_eq!(snapshot(&mut db), before);
    }

    #[test]
    fn delete_should_take_the_keys_from_the_stored_row() {
        let mut db = five_accounts();
        // The child changes every value but keeps the rows' Rids.
        let child = crate::execution::Projection::new(
            Box::new(SeqScan::new("accounts")),
            vec![
                Expr::Literal(Value::Integer(0)),
                Expr::Literal(Value::Null),
                Expr::Literal(Value::Null),
            ],
        );

        execute(&mut Delete::new("accounts", Box::new(child)), &mut db.ctx()).unwrap();

        assert!(index_entries(&mut db, "accounts_id").is_empty());
        assert!(index_entries(&mut db, "accounts_email").is_empty());
    }

    #[test]
    fn delete_should_count_a_row_given_twice_once() {
        let mut db = five_accounts();
        let first = db.stored_rows("accounts").remove(0);
        let child = Rows(vec![first.clone(), first].into_iter());

        let deleted =
            execute(&mut Delete::new("accounts", Box::new(child)), &mut db.ctx()).unwrap();

        assert_eq!(count(deleted), 1);
        assert_eq!(db.stored_rows("accounts").len(), 4);
        assert_indexes_match_table(&mut db);
    }

    #[test]
    fn delete_of_a_row_without_a_rid_should_change_nothing() {
        let mut db = five_accounts();
        let before = snapshot(&mut db);
        let mut rows = db.stored_rows("accounts");
        rows[1].rid = None;

        let err = execute(
            &mut Delete::new("accounts", Box::new(Rows(rows.into_iter()))),
            &mut db.ctx(),
        )
        .unwrap_err();

        assert!(matches!(err, ExecutionError::MissingRid), "{err:?}");
        assert_eq!(snapshot(&mut db), before);
    }

    #[test]
    fn delete_of_a_row_that_is_gone_should_change_nothing() {
        let mut db = five_accounts();
        let mut rows = db.stored_rows("accounts");
        let gone = rows.remove(4);
        db.with_table("accounts", |heap, bpm| {
            heap.delete(bpm, gone.rid.unwrap()).unwrap()
        });
        let before = db.stored_rows("accounts");
        rows.push(gone);

        let err = execute(
            &mut Delete::new("accounts", Box::new(Rows(rows.into_iter()))),
            &mut db.ctx(),
        )
        .unwrap_err();

        assert_eq!(err.sqlstate(), "XX000", "{err:?}");
        assert_eq!(db.stored_rows("accounts"), before);
    }

    fn set(column: usize, expr: Expr) -> (usize, Expr) {
        (column, expr)
    }

    fn text_expr(value: &str) -> Expr {
        Expr::Literal(Value::Text(value.to_string()))
    }

    fn id_is(id: i32) -> Box<dyn Executor> {
        let predicate = Expr::binary(
            crate::execution::BinaryOp::Eq,
            Expr::Column(0),
            Expr::Literal(Value::Integer(id)),
        );
        Box::new(crate::execution::Filter::new(
            Box::new(SeqScan::new("accounts")),
            predicate,
        ))
    }

    #[test]
    fn update_should_set_the_columns_of_the_matching_rows() {
        let mut db = five_accounts();

        let updated = execute(
            &mut Update::new("accounts", id_at_least(4), vec![set(2, text_expr("zz"))]),
            &mut db.ctx(),
        )
        .unwrap();

        assert_eq!(count(updated), 2);
        let names: Vec<Value> = db
            .stored_rows("accounts")
            .into_iter()
            .map(|row| row.values[2].clone())
            .collect();
        let text = |value: &str| Value::Text(value.to_string());
        assert_eq!(
            names,
            [
                text("ana"),
                text("bia"),
                text("caio"),
                text("zz"),
                text("zz")
            ]
        );
        assert_indexes_match_table(&mut db);
    }

    #[test]
    fn update_expressions_should_see_the_row_before_the_update() {
        let mut db = TestDb::new(16);
        let schema = Schema::new(vec![
            Column::new("a", DataType::Integer),
            Column::new("b", DataType::Integer),
        ])
        .unwrap();
        db.create_table(
            "pairs",
            schema,
            &[vec![Value::Integer(1), Value::Integer(2)]],
        );
        let swap = vec![set(0, Expr::Column(1)), set(1, Expr::Column(0))];

        execute(
            &mut Update::new("pairs", Box::new(SeqScan::new("pairs")), swap),
            &mut db.ctx(),
        )
        .unwrap();

        assert_eq!(
            db.stored_rows("pairs")[0].values,
            [Value::Integer(2), Value::Integer(1)]
        );
    }

    #[test]
    fn update_of_every_row_should_change_each_row_once_even_when_rows_move() {
        let emails: Vec<String> = (1..=12).map(|id| format!("u{id}@x")).collect();
        let rows: Vec<(i32, Option<&str>, &str)> = emails
            .iter()
            .enumerate()
            .map(|(index, email)| (index as i32 + 1, Some(email.as_str()), "n"))
            .collect();
        let mut db = accounts_db(&rows);
        let before = db.stored_rows("accounts");
        // id + 100, and an email 100 bytes longer: the rows outgrow their page and move.
        let assignments = vec![
            set(
                0,
                Expr::binary(
                    crate::execution::BinaryOp::Add,
                    Expr::Column(0),
                    Expr::Literal(Value::Integer(100)),
                ),
            ),
            set(
                1,
                Expr::binary(
                    crate::execution::BinaryOp::Concat,
                    Expr::Column(1),
                    text_expr(&"m".repeat(100)),
                ),
            ),
        ];

        let updated = execute(
            &mut Update::new("accounts", Box::new(SeqScan::new("accounts")), assignments),
            &mut db.ctx(),
        )
        .unwrap();

        assert_eq!(count(updated), 12);
        let mut after_ids: Vec<i32> = db
            .stored_rows("accounts")
            .into_iter()
            .map(|row| match row.values[0] {
                Value::Integer(id) => id,
                ref other => panic!("{other:?}"),
            })
            .collect();
        after_ids.sort();
        assert_eq!(after_ids, (101..=112).collect::<Vec<_>>());
        let after = db.stored_rows("accounts");
        assert!(
            before
                .iter()
                .any(|row| after.iter().all(|moved| moved.rid != row.rid)),
            "some row moved"
        );
        assert_indexes_match_table(&mut db);
    }

    #[test]
    fn update_to_a_key_another_row_holds_should_change_nothing() {
        let mut db = five_accounts();
        let before = snapshot(&mut db);
        let statements = [
            // c@x takes the email of row 1, which the statement does not change.
            Update::new("accounts", id_is(3), vec![set(1, text_expr("a@x"))]),
            // Two updated rows end up with the same email.
            Update::new(
                "accounts",
                id_at_least(3),
                vec![set(1, text_expr("same@x"))],
            ),
        ];

        for mut statement in statements {
            let err = execute(&mut statement, &mut db.ctx()).unwrap_err();

            assert_eq!(err.sqlstate(), "23505", "{err:?}");
            assert_eq!(snapshot(&mut db), before);
        }
    }

    #[test]
    fn update_swapping_two_keys_should_be_allowed() {
        let mut db = five_accounts();
        // id = 3 - id for ids 1 and 2: each takes the other's key.
        let predicate = Expr::binary(
            crate::execution::BinaryOp::Le,
            Expr::Column(0),
            Expr::Literal(Value::Integer(2)),
        );
        let child = crate::execution::Filter::new(Box::new(SeqScan::new("accounts")), predicate);
        let assignment = set(
            0,
            Expr::binary(
                crate::execution::BinaryOp::Sub,
                Expr::Literal(Value::Integer(3)),
                Expr::Column(0),
            ),
        );

        execute(
            &mut Update::new("accounts", Box::new(child), vec![assignment]),
            &mut db.ctx(),
        )
        .unwrap();

        let rows = db.stored_rows("accounts");
        assert_eq!(
            rows[0].values[..2],
            [Value::Integer(2), Value::Text("a@x".to_string())]
        );
        assert_eq!(rows[1].values[..2], [Value::Integer(1), Value::Null]);
        assert_indexes_match_table(&mut db);
    }

    #[test]
    fn update_with_a_value_the_table_refuses_should_change_nothing() {
        let mut db = five_accounts();
        let before = snapshot(&mut db);
        let cases = [
            (set(0, Expr::Literal(Value::Null)), "23502"),
            (set(2, text_expr("toolong")), "22001"),
            (set(0, text_expr("1")), "42804"),
            (set(9, Expr::Literal(Value::Integer(1))), "XX000"),
            (set(1, text_expr(&"x".repeat(200))), "54000"),
        ];

        for (assignment, sqlstate) in cases {
            let err = execute(
                &mut Update::new("accounts", id_at_least(4), vec![assignment]),
                &mut db.ctx(),
            )
            .unwrap_err();

            assert_eq!(err.sqlstate(), sqlstate, "{err:?}");
            assert_eq!(snapshot(&mut db), before);
        }
    }

    #[test]
    fn update_that_moves_rows_should_repoint_an_index_whose_key_did_not_change() {
        let mut db = TestDb::new(16);
        let schema = Schema::new(vec![
            Column::not_null("id", DataType::Integer),
            Column::new("body", varchar(300)),
        ])
        .unwrap();
        let rows: Vec<Vec<Value>> = (0..10)
            .map(|id| vec![Value::Integer(id), Value::Text("b".to_string())])
            .collect();
        db.create_table("notes", schema, &rows);
        let ctx = db.ctx();
        ctx.catalog
            .create_index(ctx.bpm, "notes_id", "notes", "id")
            .unwrap();
        let before = db.stored_rows("notes");
        // Only the body grows: the key stays, but the rows move to other pages.
        let grow = vec![set(1, text_expr(&"g".repeat(200)))];

        execute(
            &mut Update::new("notes", Box::new(SeqScan::new("notes")), grow),
            &mut db.ctx(),
        )
        .unwrap();

        let after = db.stored_rows("notes");
        assert!(
            before
                .iter()
                .any(|row| after.iter().all(|moved| moved.rid != row.rid)),
            "some row moved"
        );
        let mut expected: Vec<(Value, crate::storage::rid::Rid)> = after
            .iter()
            .map(|row| (row.values[0].clone(), row.rid.unwrap()))
            .collect();
        expected.sort_by(|lhs, rhs| lhs.0.sort_cmp(&rhs.0));
        assert_eq!(index_entries(&mut db, "notes_id"), expected);
    }

    #[test]
    fn update_should_leave_the_index_of_an_unchanged_column_alone() {
        let mut db = five_accounts();
        let ids_before = index_entries(&mut db, "accounts_id");

        execute(
            &mut Update::new("accounts", id_at_least(1), vec![set(2, text_expr("z"))]),
            &mut db.ctx(),
        )
        .unwrap();

        assert_eq!(index_entries(&mut db, "accounts_id"), ids_before);
        assert_indexes_match_table(&mut db);
    }

    #[test]
    fn update_of_a_row_without_a_rid_should_fail() {
        let mut db = five_accounts();
        let child = Values::new(vec![literals(account(1, None, "x"))]);

        let err = execute(
            &mut Update::new("accounts", Box::new(child), Vec::new()),
            &mut db.ctx(),
        )
        .unwrap_err();

        assert!(matches!(err, ExecutionError::MissingRid), "{err:?}");
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

        /// A small pool of emails, so the unique index on `email` is often hit.
        fn email(&mut self) -> Option<String> {
            match self.below(4) {
                0 => None,
                _ => Some(format!("e{}@x", self.below(25))),
            }
        }

        /// Text added to every email of a range: emails stay unique, rows outgrow their page.
        fn padding(&mut self) -> String {
            "q".repeat(20 + self.below(41))
        }
    }

    type ModelRow = (i32, Option<String>, String);

    /// What the unique indexes require: no repeated id, no repeated non-null email.
    fn is_unique(rows: &[ModelRow]) -> bool {
        let mut ids: Vec<i32> = rows.iter().map(|row| row.0).collect();
        let mut emails: Vec<&String> = rows.iter().filter_map(|row| row.1.as_ref()).collect();
        ids.sort();
        emails.sort();
        ids.windows(2).all(|pair| pair[0] != pair[1])
            && emails.windows(2).all(|pair| pair[0] != pair[1])
    }

    fn model_values(row: &ModelRow) -> Vec<Value> {
        account(row.0, row.1.as_deref(), &row.2)
    }

    fn assert_table_matches(db: &mut TestDb, model: &[ModelRow], step: usize) {
        let mut stored: Vec<Vec<Value>> = db
            .stored_rows("accounts")
            .into_iter()
            .map(|row| row.values)
            .collect();
        let mut expected: Vec<Vec<Value>> = model.iter().map(model_values).collect();
        stored.sort_by(|lhs, rhs| lhs[0].sort_cmp(&rhs[0]));
        expected.sort_by(|lhs, rhs| lhs[0].sort_cmp(&rhs[0]));
        assert_eq!(stored, expected, "step {step}");
        assert_indexes_match_table(db);
    }

    /// `low <= id < high`.
    fn id_between(low: i32, high: i32) -> Box<dyn Executor> {
        use crate::execution::BinaryOp::{And, Ge, Lt};
        let int = |value| Expr::Literal(Value::Integer(value));
        let predicate = Expr::binary(
            And,
            Expr::binary(Ge, Expr::Column(0), int(low)),
            Expr::binary(Lt, Expr::Column(0), int(high)),
        );
        Box::new(crate::execution::Filter::new(
            Box::new(SeqScan::new("accounts")),
            predicate,
        ))
    }

    /// The error a statement leading to `rows` must fail with, if any: a repeated key, or an
    /// email longer than the 111-byte keys of 512-byte pages.
    fn expected_error(rows: &[ModelRow]) -> Option<&'static str> {
        if !is_unique(rows) {
            return Some("23505");
        }
        let has_long_email = rows
            .iter()
            .any(|row| row.1.as_ref().is_some_and(|email| email.len() > 111));
        has_long_email.then_some("54000")
    }

    /// Applies a statement whose expected result the model computed: on success the table
    /// must match `new_model`; on failure it must fail as expected and nothing may change.
    fn run_statement(
        db: &mut TestDb,
        model: &mut Vec<ModelRow>,
        mut statement: Box<dyn Executor>,
        new_model: Vec<ModelRow>,
        step: usize,
    ) {
        let result = execute(statement.as_mut(), &mut db.ctx());
        match expected_error(&new_model) {
            None => {
                result.unwrap();
                *model = new_model;
            }
            Some(sqlstate) => assert_eq!(result.unwrap_err().sqlstate(), sqlstate, "step {step}"),
        }
        assert_table_matches(db, model, step);
    }

    #[test]
    fn random_statements_should_keep_the_table_and_its_indexes_like_a_model() {
        let mut rng = Rng(0x5851_F42D_4C95_7F2D);
        let mut db = accounts_db(&[]);
        let mut model: Vec<ModelRow> = Vec::new();

        for step in 1..=300 {
            let low = rng.below(60) as i32;
            let high = low + 1 + rng.below(10) as i32;
            let in_range = |row: &ModelRow| row.0 >= low && row.0 < high;
            match rng.below(4) {
                0 => {
                    let batch: Vec<ModelRow> = (0..1 + rng.below(3))
                        .map(|_| {
                            (
                                rng.below(70) as i32,
                                rng.email(),
                                format!("n{}", rng.below(9)),
                            )
                        })
                        .collect();
                    let mut new_model = model.clone();
                    new_model.extend(batch.iter().cloned());
                    let insert =
                        Insert::new("accounts", values(batch.iter().map(model_values).collect()));
                    run_statement(&mut db, &mut model, Box::new(insert), new_model, step);
                }
                1 => {
                    let new_model: Vec<ModelRow> =
                        model.iter().filter(|row| !in_range(row)).cloned().collect();
                    let delete = Delete::new("accounts", id_between(low, high));
                    run_statement(&mut db, &mut model, Box::new(delete), new_model, step);
                }
                2 => {
                    let padding = rng.padding();
                    let new_model: Vec<ModelRow> = model
                        .iter()
                        .map(|row| match in_range(row) {
                            false => row.clone(),
                            true => (
                                row.0,
                                row.1.as_ref().map(|email| format!("{email}{padding}")),
                                row.2.clone(),
                            ),
                        })
                        .collect();
                    let grow = set(
                        1,
                        Expr::binary(
                            crate::execution::BinaryOp::Concat,
                            Expr::Column(1),
                            text_expr(&padding),
                        ),
                    );
                    let update = Update::new("accounts", id_between(low, high), vec![grow]);
                    run_statement(&mut db, &mut model, Box::new(update), new_model, step);
                }
                _ => {
                    let shift = rng.below(7) as i32 - 3;
                    let email = rng.email();
                    let changes_email = rng.below(2) == 0;
                    let new_model: Vec<ModelRow> = model
                        .iter()
                        .map(|row| match in_range(row) {
                            false => row.clone(),
                            true if changes_email => (row.0, email.clone(), row.2.clone()),
                            true => (row.0 + shift, row.1.clone(), row.2.clone()),
                        })
                        .collect();
                    let assignment = if changes_email {
                        set(1, Expr::Literal(email.map_or(Value::Null, Value::Text)))
                    } else {
                        set(
                            0,
                            Expr::binary(
                                crate::execution::BinaryOp::Add,
                                Expr::Column(0),
                                Expr::Literal(Value::Integer(shift)),
                            ),
                        )
                    };
                    let update = Update::new("accounts", id_between(low, high), vec![assignment]);
                    run_statement(&mut db, &mut model, Box::new(update), new_model, step);
                }
            }
            if step % 50 == 0 {
                db.reopen(16);
                assert_table_matches(&mut db, &model, step);
            }
        }
        assert!(model.len() > 10, "the table should keep some rows");
    }

    #[test]
    fn delete_and_update_of_rows_from_another_table_should_change_nothing() {
        let mut db = five_accounts();
        let schema = Schema::new(vec![Column::new("n", DataType::Integer)]).unwrap();
        db.create_table("other", schema, &[vec![Value::Integer(1)]]);
        let before = snapshot(&mut db);
        let from_other = || Box::new(SeqScan::new("other"));

        let delete =
            execute(&mut Delete::new("accounts", from_other()), &mut db.ctx()).unwrap_err();
        let update = execute(
            &mut Update::new("accounts", from_other(), vec![set(2, text_expr("x"))]),
            &mut db.ctx(),
        )
        .unwrap_err();

        for err in [delete, update] {
            assert_eq!(err.sqlstate(), "XX000", "{err:?}");
        }
        assert_eq!(snapshot(&mut db), before);
    }

    fn int(value: i32) -> Expr {
        Expr::Literal(Value::Integer(value))
    }

    fn integer_ids(db: &mut TestDb) -> Vec<i32> {
        let mut ids: Vec<i32> = ids(db)
            .into_iter()
            .map(|id| match id {
                Value::Integer(id) => id,
                other => panic!("{other:?}"),
            })
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn delete_through_an_index_scan_should_remove_the_rows_in_the_range() {
        let mut db = five_accounts();
        let scan = IndexScan::new(
            "accounts_id",
            Bound::Included(int(2)),
            Bound::Excluded(int(4)),
        );

        let deleted = execute(&mut Delete::new("accounts", Box::new(scan)), &mut db.ctx()).unwrap();

        assert_eq!(count(deleted), 2);
        assert_eq!(integer_ids(&mut db), [1, 4, 5]);
        assert_indexes_match_table(&mut db);
    }

    #[test]
    fn update_through_an_index_scan_should_set_the_row_of_the_key() {
        let mut db = five_accounts();
        let scan = IndexScan::point("accounts_email", text_expr("c@x"));

        let updated = execute(
            &mut Update::new("accounts", Box::new(scan), vec![set(1, text_expr("new@x"))]),
            &mut db.ctx(),
        )
        .unwrap();

        assert_eq!(count(updated), 1);
        let email_of_3 = db
            .stored_rows("accounts")
            .into_iter()
            .find(|row| row.values[0] == Value::Integer(3))
            .map(|row| row.values[1].clone());
        assert_eq!(email_of_3, Some(Value::Text("new@x".to_string())));
        assert_indexes_match_table(&mut db);
    }

    #[test]
    fn update_of_the_scanned_key_should_change_each_row_once() {
        // `SET id = id + 10 WHERE id >= 5` read through the index of `id`: a scan interleaved
        // with the writes would meet each moved key again further on.
        let rows: Vec<(i32, Option<&str>, &str)> = (1..=30).map(|id| (id, None, "n")).collect();
        let mut db = accounts_db(&rows);
        let scan = IndexScan::new("accounts_id", Bound::Included(int(5)), Bound::Unbounded);
        let plus_ten = Expr::binary(crate::execution::BinaryOp::Add, Expr::Column(0), int(10));

        let updated = execute(
            &mut Update::new("accounts", Box::new(scan), vec![set(0, plus_ten)]),
            &mut db.ctx(),
        )
        .unwrap();

        assert_eq!(count(updated), 26);
        let expected: Vec<i32> = (1..=4).chain(15..=40).collect();
        assert_eq!(integer_ids(&mut db), expected);
        assert_indexes_match_table(&mut db);
    }

    #[test]
    fn update_should_count_a_row_given_twice_once() {
        let mut db = five_accounts();
        let first = db.stored_rows("accounts").remove(0);
        let child = Rows(vec![first.clone(), first].into_iter());

        let updated = execute(
            &mut Update::new(
                "accounts",
                Box::new(child),
                vec![set(2, text_expr("twice"))],
            ),
            &mut db.ctx(),
        )
        .unwrap();

        assert_eq!(count(updated), 1);
        assert_eq!(
            db.stored_rows("accounts")[0].values[2],
            Value::Text("twice".to_string())
        );
        assert_indexes_match_table(&mut db);
    }

    #[test]
    fn delete_and_update_of_an_unknown_table_should_fail() {
        let mut db = TestDb::new(4);

        let delete = execute(
            &mut Delete::new("missing", values(Vec::new())),
            &mut db.ctx(),
        )
        .unwrap_err();
        let update = execute(
            &mut Update::new("missing", values(Vec::new()), Vec::new()),
            &mut db.ctx(),
        )
        .unwrap_err();

        assert_eq!(delete.sqlstate(), "42P01");
        assert_eq!(update.sqlstate(), "42P01");
    }

    #[test]
    fn insert_should_count_once_and_then_end() {
        let mut db = accounts_db(&[]);
        let mut insert = Insert::new("accounts", values(Vec::new()));

        let first = insert.next(&mut db.ctx()).unwrap();
        let second = insert.next(&mut db.ctx()).unwrap();

        assert_eq!(count(vec![first.unwrap()]), 0);
        assert_eq!(second, None);
    }
}
