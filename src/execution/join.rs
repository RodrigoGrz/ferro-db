//! `IndexNestedLoopJoin` joins each outer row with the inner rows an index finds for it
//! (CMU 15-445 lecture 12, BusTub's `NestedIndexJoin`): one descent of the B+Tree per outer
//! row, instead of a scan of the inner table, and no executor ever has to start over.
//!
//! The outer key is converted to the index's key type as an `IndexScan::point` bound is
//! (`key_bounds::point_key`): a NULL key or one the key type rounds or cannot hold matches
//! nothing, another family is 42804. The residual predicate is part of the `ON`: it decides
//! what matches, so in a `LEFT` join an outer row whose pairs it rejects comes out padded with
//! NULLs. The matches of one outer row are read into a `Vec` within the call, so no leaf stays
//! pinned between calls; with today's unique indexes there is at most one.

use std::ops::Bound;

use crate::storage::rid::Rid;
use crate::types::{DataType, Value};

use super::key_bounds::point_key;
use super::{ExecutionContext, ExecutionError, Executor, Expr, Row};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    Inner,
    /// Every left (outer) row comes out at least once, with NULLs for the right columns when
    /// nothing matched.
    Left,
    /// Every right row comes out at least once, with NULLs for the left columns.
    Right,
    /// `LEFT` and `RIGHT` together.
    Full,
}

impl JoinType {
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            JoinType::Inner => "INNER JOIN",
            JoinType::Left => "LEFT JOIN",
            JoinType::Right => "RIGHT JOIN",
            JoinType::Full => "FULL JOIN",
        }
    }
}

/// For each outer row, the inner rows whose `index` key equals `outer_key` (evaluated over
/// the outer row). A joined row is the outer values followed by the inner table's values,
/// without `Rid`.
pub struct IndexNestedLoopJoin {
    outer: Box<dyn Executor>,
    outer_key: Expr,
    index: String,
    join_type: JoinType,
    /// The rest of the `ON`, over the joined row: it decides what matches, so in a `LEFT`
    /// join a pair it rejects leaves the outer row padded with NULLs.
    predicate: Option<Expr>,
    state: JoinState,
}

enum JoinState {
    Start,
    Running {
        inner: InnerTable,
        pending: std::vec::IntoIter<Row>,
    },
    Done,
}

/// What the first call learns about the inner side.
struct InnerTable {
    name: String,
    key_type: DataType,
    /// The number of columns, which a `LEFT` join pads with NULLs.
    width: usize,
}

impl IndexNestedLoopJoin {
    pub fn new(
        outer: Box<dyn Executor>,
        outer_key: Expr,
        index: &str,
        join_type: JoinType,
        predicate: Option<Expr>,
    ) -> Self {
        Self {
            outer,
            outer_key,
            index: index.to_string(),
            join_type,
            predicate,
            state: JoinState::Start,
        }
    }

    fn inner_table(&self, ctx: &mut ExecutionContext<'_>) -> Result<InnerTable, ExecutionError> {
        // The index only finds inner rows for outer ones: it cannot tell which inner rows never
        // matched.
        if matches!(self.join_type, JoinType::Right | JoinType::Full) {
            return Err(ExecutionError::UnsupportedJoinType {
                join_type: self.join_type,
            });
        }
        let table = ctx.catalog.index_table(&self.index)?;
        let (name, width) = (table.name.clone(), table.schema.columns().len());
        let key_type = ctx.catalog.index_tree(ctx.bpm, &self.index)?.key_type();
        Ok(InnerTable {
            name,
            key_type,
            width,
        })
    }

    /// The joined rows of one outer row: its matches, or for a `LEFT` join without any, the
    /// row padded with NULLs.
    fn joined_rows(
        &self,
        ctx: &mut ExecutionContext<'_>,
        inner: &InnerTable,
        outer_row: Row,
    ) -> Result<Vec<Row>, ExecutionError> {
        let matches = self.matches(ctx, inner, &outer_row.values)?;
        if matches.is_empty() && self.join_type == JoinType::Left {
            let nulls = std::iter::repeat_n(Value::Null, inner.width);
            return Ok(vec![joined(outer_row.values, nulls)]);
        }
        Ok(matches)
    }

    fn matches(
        &self,
        ctx: &mut ExecutionContext<'_>,
        inner: &InnerTable,
        outer_values: &[Value],
    ) -> Result<Vec<Row>, ExecutionError> {
        let key = self.outer_key.eval(outer_values)?;
        let Some(key) = point_key(key, inner.key_type)? else {
            return Ok(Vec::new());
        };
        let mut matches = Vec::new();
        for inner_values in inner_rows(ctx, &self.index, &inner.name, &key)? {
            let row = joined(outer_values.to_vec(), inner_values);
            if passes(self.predicate.as_ref(), &row.values)? {
                matches.push(row);
            }
        }
        Ok(matches)
    }
}

impl Executor for IndexNestedLoopJoin {
    fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError> {
        // `Done` until the next row is in hand, so an error also ends the join.
        let (inner, mut pending) = match std::mem::replace(&mut self.state, JoinState::Done) {
            JoinState::Start => (self.inner_table(ctx)?, Vec::new().into_iter()),
            JoinState::Running { inner, pending } => (inner, pending),
            JoinState::Done => return Ok(None),
        };
        loop {
            if let Some(row) = pending.next() {
                self.state = JoinState::Running { inner, pending };
                return Ok(Some(row));
            }
            let Some(outer_row) = self.outer.next(ctx)? else {
                return Ok(None);
            };
            pending = self.joined_rows(ctx, &inner, outer_row)?.into_iter();
        }
    }
}

/// The left (outer) values followed by the right (inner) ones. Not a row of one table, so no
/// `Rid`.
pub(super) fn joined(mut values: Vec<Value>, right_values: impl IntoIterator<Item = Value>) -> Row {
    values.extend(right_values);
    Row { values, rid: None }
}

/// Whether a joined row matches the rest of the `ON`: like a `WHERE`, only TRUE does. In an
/// outer join this decides what matches, so a pair it rejects leaves its row padded.
pub(super) fn passes(
    predicate: Option<&Expr>,
    joined_values: &[Value],
) -> Result<bool, ExecutionError> {
    match predicate {
        Some(predicate) => Ok(predicate.eval(joined_values)?.is_true()?),
        None => Ok(true),
    }
}

/// The inner table's rows whose key is `key`, in key order. The range is read and dropped
/// here, so no leaf stays pinned between calls.
fn inner_rows(
    ctx: &mut ExecutionContext<'_>,
    index: &str,
    table: &str,
    key: &Value,
) -> Result<Vec<Vec<Value>>, ExecutionError> {
    let tree = ctx.catalog.index_tree(ctx.bpm, index)?;
    let rids = tree
        .range(ctx.bpm, Bound::Included(key), Bound::Included(key))?
        .map(|entry| entry.map(|(_, rid)| rid))
        .collect::<Result<Vec<Rid>, _>>()?;
    let heap = ctx.catalog.table_heap(ctx.bpm, table)?;
    rids.into_iter()
        .map(|rid| Ok(heap.get(ctx.bpm, rid)?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::test_support::TestDb;
    use crate::execution::{BinaryOp, Delete, Filter, Insert, SeqScan, Values, execute};
    use crate::table::{Column, Schema};
    use crate::types::{DataType, Date, Finite32, Finite64, Timestamp, Value};

    fn int(value: i32) -> Value {
        Value::Integer(value)
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_string())
    }

    fn col(column: usize) -> Expr {
        Expr::Column(column)
    }

    fn rows(rows: Vec<Vec<Value>>) -> Box<dyn Executor> {
        let rows = rows
            .into_iter()
            .map(|row| row.into_iter().map(Expr::Literal).collect())
            .collect();
        Box::new(Values::new(rows))
    }

    /// `customers (id INTEGER NOT NULL, name VARCHAR)` with the unique index `customers_id`.
    fn customers_db(customers: &[(i32, &str)]) -> TestDb {
        let mut db = TestDb::new(8);
        let schema = Schema::new(vec![
            Column::not_null("id", DataType::Integer),
            Column::new("name", DataType::VARCHAR_MAX),
        ])
        .unwrap();
        let customers: Vec<Vec<Value>> = customers
            .iter()
            .map(|&(id, name)| vec![int(id), text(name)])
            .collect();
        db.create_table("customers", schema, &customers);
        let ctx = db.ctx();
        ctx.catalog
            .create_index(ctx.bpm, "customers_id", "customers", "id")
            .unwrap();
        db
    }

    fn ana_and_bia() -> TestDb {
        customers_db(&[(10, "ana"), (20, "bia")])
    }

    /// `(order id, customer id)` rows.
    fn orders(orders: &[(i32, Value)]) -> Box<dyn Executor> {
        rows(
            orders
                .iter()
                .map(|(id, customer)| vec![int(*id), customer.clone()])
                .collect(),
        )
    }

    fn join(outer: Box<dyn Executor>, join_type: JoinType) -> IndexNestedLoopJoin {
        IndexNestedLoopJoin::new(outer, col(1), "customers_id", join_type, None)
    }

    fn values_of(output: Vec<Row>) -> Vec<Vec<Value>> {
        output.into_iter().map(|row| row.values).collect()
    }

    fn join_err(db: &mut TestDb, mut join: IndexNestedLoopJoin) -> ExecutionError {
        let err = join.next(&mut db.ctx()).unwrap_err();

        assert_eq!(join.next(&mut db.ctx()).unwrap(), None);
        err
    }

    #[test]
    fn inner_join_should_pair_each_outer_row_with_its_inner_row_in_outer_order() {
        let mut db = ana_and_bia();
        let outer = orders(&[(1, int(10)), (2, int(99)), (3, int(20)), (4, int(10))]);
        let mut join = join(outer, JoinType::Inner);

        let output = execute(&mut join, &mut db.ctx()).unwrap();

        assert_eq!(
            values_of(output),
            [
                [int(1), int(10), int(10), text("ana")],
                [int(3), int(20), int(20), text("bia")],
                [int(4), int(10), int(10), text("ana")],
            ]
        );
        assert_eq!(join.next(&mut db.ctx()).unwrap(), None);
    }

    #[test]
    fn a_null_outer_key_should_match_nothing() {
        let mut db = ana_and_bia();
        let mut join = join(orders(&[(1, Value::Null)]), JoinType::Inner);

        let output = execute(&mut join, &mut db.ctx()).unwrap();

        assert!(output.is_empty());
    }

    #[test]
    fn an_outer_key_of_another_width_should_be_converted_to_the_key_type() {
        let mut db = ana_and_bia();
        let mut join = join(orders(&[(1, Value::SmallInt(20))]), JoinType::Inner);

        let output = execute(&mut join, &mut db.ctx()).unwrap();

        assert_eq!(
            values_of(output),
            [[int(1), Value::SmallInt(20), int(20), text("bia")]]
        );
    }

    #[test]
    fn an_outer_key_of_another_family_should_fail() {
        let mut db = ana_and_bia();
        let join = join(orders(&[(1, text("10"))]), JoinType::Inner);

        let err = join_err(&mut db, join);

        assert_eq!(err.sqlstate(), "42804");
    }

    #[test]
    fn inner_join_with_an_empty_side_should_give_no_rows() {
        let mut db = ana_and_bia();
        let mut no_outer = join(orders(&[]), JoinType::Inner);
        let mut no_customers = customers_db(&[]);
        let mut no_inner = join(orders(&[(1, int(10))]), JoinType::Inner);

        let from_no_outer = execute(&mut no_outer, &mut db.ctx()).unwrap();
        let from_no_inner = execute(&mut no_inner, &mut no_customers.ctx()).unwrap();

        assert!(from_no_outer.is_empty());
        assert!(from_no_inner.is_empty());
    }

    #[test]
    fn right_and_full_joins_should_be_unsupported() {
        for join_type in [JoinType::Right, JoinType::Full] {
            let mut db = ana_and_bia();

            let err = join_err(&mut db, join(orders(&[(1, int(10))]), join_type));

            assert_eq!(err.sqlstate(), "0A000");
            assert!(
                matches!(err, ExecutionError::UnsupportedJoinType { join_type: found } if found == join_type),
                "{err:?}"
            );
        }
    }

    #[test]
    fn an_unknown_index_should_fail() {
        let mut db = ana_and_bia();
        let join = IndexNestedLoopJoin::new(
            orders(&[(1, int(10))]),
            col(1),
            "missing",
            JoinType::Inner,
            None,
        );

        let err = join_err(&mut db, join);

        assert_eq!(err.sqlstate(), "42704");
    }

    #[test]
    fn a_failing_outer_key_should_end_the_join() {
        let mut db = ana_and_bia();
        let ten_over_id = Expr::binary(BinaryOp::Div, Expr::Literal(int(10)), col(0));
        let outer = orders(&[(1, int(10)), (0, int(20)), (2, int(10))]);
        let mut join =
            IndexNestedLoopJoin::new(outer, ten_over_id, "customers_id", JoinType::Inner, None);

        let first = join.next(&mut db.ctx()).unwrap();
        let err = join_err(&mut db, join);

        assert_eq!(
            first.map(|row| row.values),
            Some(vec![int(1), int(10), int(10), text("ana")])
        );
        assert_eq!(err.sqlstate(), "22012");
    }

    #[test]
    fn joined_rows_should_have_no_rid() {
        let mut db = ana_and_bia();
        let schema = Schema::new(vec![
            Column::not_null("id", DataType::Integer),
            Column::new("customer_id", DataType::Integer),
        ])
        .unwrap();
        db.create_table(
            "orders",
            schema,
            &[vec![int(1), int(10)], vec![int(2), int(20)]],
        );
        let mut join = join(Box::new(SeqScan::new("orders")), JoinType::Inner);

        let output = execute(&mut join, &mut db.ctx()).unwrap();

        assert_eq!(output.len(), 2);
        assert!(output.iter().all(|row| row.rid.is_none()));
    }

    #[test]
    fn left_join_should_pad_an_unmatched_outer_row_with_nulls() {
        let mut db = ana_and_bia();
        let outer = orders(&[(1, int(10)), (2, int(99)), (3, int(20))]);
        let mut join = join(outer, JoinType::Left);

        let output = execute(&mut join, &mut db.ctx()).unwrap();

        assert_eq!(
            values_of(output),
            [
                [int(1), int(10), int(10), text("ana")],
                [int(2), int(99), Value::Null, Value::Null],
                [int(3), int(20), int(20), text("bia")],
            ]
        );
    }

    /// `customers (id, name, active BOOLEAN)`: ana is active, bia is not, caio is unknown.
    fn active_customers_db() -> TestDb {
        let mut db = TestDb::new(8);
        let schema = Schema::new(vec![
            Column::not_null("id", DataType::Integer),
            Column::new("name", DataType::VARCHAR_MAX),
            Column::new("active", DataType::Boolean),
        ])
        .unwrap();
        let customers = [
            vec![int(10), text("ana"), Value::Boolean(true)],
            vec![int(20), text("bia"), Value::Boolean(false)],
            vec![int(30), text("caio"), Value::Null],
        ];
        db.create_table("customers", schema, &customers);
        let ctx = db.ctx();
        ctx.catalog
            .create_index(ctx.bpm, "customers_id", "customers", "id")
            .unwrap();
        db
    }

    fn one_order_per_customer() -> Box<dyn Executor> {
        orders(&[(1, int(10)), (2, int(20)), (3, int(30))])
    }

    /// `c.active`, over the joined row `(order id, customer id, id, name, active)`.
    fn is_active() -> Option<Expr> {
        Some(col(4))
    }

    #[test]
    fn inner_join_should_drop_pairs_whose_predicate_is_not_true() {
        let mut db = active_customers_db();
        let mut join = IndexNestedLoopJoin::new(
            one_order_per_customer(),
            col(1),
            "customers_id",
            JoinType::Inner,
            is_active(),
        );

        let output = execute(&mut join, &mut db.ctx()).unwrap();

        assert_eq!(
            values_of(output),
            [[int(1), int(10), int(10), text("ana"), Value::Boolean(true)]]
        );
    }

    #[test]
    fn left_join_should_pad_pairs_whose_predicate_is_not_true_unlike_a_filter_above() {
        let mut db = active_customers_db();
        let mut predicate_in_on = IndexNestedLoopJoin::new(
            one_order_per_customer(),
            col(1),
            "customers_id",
            JoinType::Left,
            is_active(),
        );
        let unfiltered = IndexNestedLoopJoin::new(
            one_order_per_customer(),
            col(1),
            "customers_id",
            JoinType::Left,
            None,
        );
        let mut predicate_in_where = Filter::new(Box::new(unfiltered), col(4));

        let from_on = execute(&mut predicate_in_on, &mut db.ctx()).unwrap();
        let from_where = execute(&mut predicate_in_where, &mut db.ctx()).unwrap();

        let ana = vec![int(1), int(10), int(10), text("ana"), Value::Boolean(true)];
        let padded = |id, customer| {
            vec![
                int(id),
                int(customer),
                Value::Null,
                Value::Null,
                Value::Null,
            ]
        };
        assert_eq!(
            values_of(from_on),
            [ana.clone(), padded(2, 20), padded(3, 30)]
        );
        assert_eq!(values_of(from_where), [ana]);
    }

    #[test]
    fn a_predicate_should_read_both_sides_of_the_joined_row() {
        let mut db = ana_and_bia();
        // order id * 10 = customer id
        let predicate = Expr::binary(
            BinaryOp::Eq,
            Expr::binary(BinaryOp::Mul, col(0), Expr::Literal(int(10))),
            col(2),
        );
        let outer = orders(&[(1, int(10)), (5, int(20))]);
        let mut join = IndexNestedLoopJoin::new(
            outer,
            col(1),
            "customers_id",
            JoinType::Inner,
            Some(predicate),
        );

        let output = execute(&mut join, &mut db.ctx()).unwrap();

        assert_eq!(values_of(output), [[int(1), int(10), int(10), text("ana")]]);
    }

    #[test]
    fn a_predicate_that_is_not_boolean_or_fails_should_end_the_join() {
        let one_over_zero =
            Expr::binary(BinaryOp::Div, Expr::Literal(int(1)), Expr::Literal(int(0)));
        let cases = [(col(3), "42804"), (one_over_zero, "22012")];

        for (predicate, sqlstate) in cases {
            let mut db = ana_and_bia();
            let join = IndexNestedLoopJoin::new(
                orders(&[(1, int(10))]),
                col(1),
                "customers_id",
                JoinType::Left,
                Some(predicate),
            );

            let err = join_err(&mut db, join);

            assert_eq!(err.sqlstate(), sqlstate);
        }
    }

    #[test]
    fn an_index_out_of_step_with_its_table_should_fail_and_end_the_join() {
        let mut db = ana_and_bia();
        let bia = db.stored_rows("customers")[1].rid.unwrap();
        db.with_table("customers", |heap, bpm| heap.delete(bpm, bia))
            .unwrap();
        let mut join = join(orders(&[(1, int(10)), (2, int(20))]), JoinType::Inner);

        let first = join.next(&mut db.ctx()).unwrap();
        let err = join.next(&mut db.ctx()).unwrap_err();

        assert!(first.is_some());
        assert_eq!(err.sqlstate(), "XX000", "{err:?}");
        assert_eq!(join.next(&mut db.ctx()).unwrap(), None);
    }

    #[test]
    fn left_join_should_pad_null_keys_and_rows_of_an_empty_inner_table() {
        let mut db = customers_db(&[]);
        let mut join = join(orders(&[(1, Value::Null), (2, int(10))]), JoinType::Left);

        let output = execute(&mut join, &mut db.ctx()).unwrap();

        assert_eq!(
            values_of(output),
            [
                [int(1), Value::Null, Value::Null, Value::Null],
                [int(2), int(10), Value::Null, Value::Null],
            ]
        );
    }

    /// A one-column table `name (key <key_type> NOT NULL)` with the unique index `<name>_key`.
    fn add_indexed_table(db: &mut TestDb, name: &str, key_type: DataType, keys: Vec<Value>) {
        let schema = Schema::new(vec![Column::not_null("key", key_type)]).unwrap();
        let rows: Vec<Vec<Value>> = keys.into_iter().map(|key| vec![key]).collect();
        db.create_table(name, schema, &rows);
        let ctx = db.ctx();
        ctx.catalog
            .create_index(ctx.bpm, &format!("{name}_key"), name, "key")
            .unwrap();
    }

    fn join_on_key(
        outer_keys: Vec<Value>,
        index: &str,
        join_type: JoinType,
    ) -> IndexNestedLoopJoin {
        let outer = rows(outer_keys.into_iter().map(|key| vec![key]).collect());
        IndexNestedLoopJoin::new(outer, col(0), index, join_type, None)
    }

    #[test]
    fn an_outer_key_too_long_for_the_index_should_match_nothing() {
        let mut db = TestDb::new(8);
        add_indexed_table(&mut db, "codes", DataType::VARCHAR_MAX, vec![text("a")]);
        let too_long = text(&"x".repeat(2_000));
        let keys = vec![too_long.clone(), text("a")];
        let mut inner = join_on_key(keys.clone(), "codes_key", JoinType::Inner);
        let mut left = join_on_key(keys, "codes_key", JoinType::Left);

        let from_inner = execute(&mut inner, &mut db.ctx()).unwrap();
        let from_left = execute(&mut left, &mut db.ctx()).unwrap();

        assert_eq!(values_of(from_inner), [[text("a"), text("a")]]);
        assert_eq!(
            values_of(from_left),
            [[too_long, Value::Null], [text("a"), text("a")]]
        );
    }

    #[test]
    fn outer_keys_of_other_types_should_follow_the_index_scan_rule() {
        let mut db = TestDb::new(8);
        let real = |value: f32| Value::Real(Finite32::new(value).unwrap());
        let double = |value: f64| Value::Double(Finite64::new(value).unwrap());
        let day = Date::from_ymd(2026, 10, 22).unwrap();
        add_indexed_table(
            &mut db,
            "weights",
            DataType::Real,
            vec![real(0.5), real(0.1)],
        );
        add_indexed_table(
            &mut db,
            "events",
            DataType::Timestamp,
            vec![Value::Timestamp(Timestamp::at_midnight(day))],
        );
        let mut by_weight = join_on_key(
            vec![double(0.5), double(0.1)],
            "weights_key",
            JoinType::Inner,
        );
        let mut by_day = join_on_key(vec![Value::Date(day)], "events_key", JoinType::Inner);

        let weights = execute(&mut by_weight, &mut db.ctx()).unwrap();
        let days = execute(&mut by_day, &mut db.ctx()).unwrap();

        // The DOUBLE PRECISION 0.1 is not the REAL 0.1, so only 0.5 matches.
        assert_eq!(values_of(weights), [[double(0.5), real(0.5)]]);
        assert_eq!(
            values_of(days),
            [[
                Value::Date(day),
                Value::Timestamp(Timestamp::at_midnight(day))
            ]]
        );
    }

    #[test]
    fn a_self_join_should_read_the_table_it_scans() {
        let mut db = TestDb::new(8);
        let schema = Schema::new(vec![
            Column::not_null("id", DataType::Integer),
            Column::new("manager_id", DataType::Integer),
        ])
        .unwrap();
        let employees = [
            vec![int(1), Value::Null],
            vec![int(2), int(1)],
            vec![int(3), int(1)],
            vec![int(4), int(2)],
        ];
        db.create_table("employees", schema, &employees);
        let ctx = db.ctx();
        ctx.catalog
            .create_index(ctx.bpm, "employees_id", "employees", "id")
            .unwrap();
        // SELECT * FROM employees e JOIN employees m ON e.manager_id = m.id
        let mut join = IndexNestedLoopJoin::new(
            Box::new(SeqScan::new("employees")),
            col(1),
            "employees_id",
            JoinType::Inner,
            None,
        );

        let output = execute(&mut join, &mut db.ctx()).unwrap();

        assert_eq!(
            values_of(output),
            [
                [int(2), int(1), int(1), Value::Null],
                [int(3), int(1), int(1), Value::Null],
                [int(4), int(2), int(2), int(1)],
            ]
        );
    }

    #[test]
    fn a_failing_outer_child_should_end_the_join() {
        let mut db = ana_and_bia();
        let join = join(Box::new(SeqScan::new("missing")), JoinType::Left);

        let err = join_err(&mut db, join);

        assert_eq!(err.sqlstate(), "42P01");
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
    }

    /// The residual predicates the random test draws, over the joined row
    /// `(order id, customer id, amount, id, name, active)`.
    #[derive(Debug, Clone, Copy)]
    enum ModelPredicate {
        None,
        IsActive,
        AmountAbove50,
    }

    impl ModelPredicate {
        fn expr(self) -> Option<Expr> {
            match self {
                ModelPredicate::None => None,
                ModelPredicate::IsActive => Some(col(5)),
                ModelPredicate::AmountAbove50 => {
                    Some(Expr::binary(BinaryOp::Gt, col(2), Expr::Literal(int(50))))
                }
            }
        }

        fn holds(self, order: &[Value], customer: &[Value]) -> bool {
            match self {
                ModelPredicate::None => true,
                ModelPredicate::IsActive => customer[2] == Value::Boolean(true),
                ModelPredicate::AmountAbove50 => {
                    matches!(order[2], Value::Integer(amount) if amount > 50)
                }
            }
        }
    }

    /// The plain nested loop over the stored rows: no index, no key conversion.
    fn model_join(
        db: &mut TestDb,
        join_type: JoinType,
        predicate: ModelPredicate,
    ) -> Vec<Vec<Value>> {
        let customers = db.stored_rows("customers");
        let mut output = Vec::new();
        for order in db.stored_rows("orders") {
            let mut has_match = false;
            for customer in &customers {
                let is_same_customer = match (&order.values[1], &customer.values[0]) {
                    (Value::SmallInt(order_customer), Value::Integer(id)) => {
                        i32::from(*order_customer) == *id
                    }
                    _ => false,
                };
                if is_same_customer && predicate.holds(&order.values, &customer.values) {
                    has_match = true;
                    output.push([order.values.clone(), customer.values.clone()].concat());
                }
            }
            if !has_match && join_type == JoinType::Left {
                output.push([order.values.clone(), vec![Value::Null; 3]].concat());
            }
        }
        output
    }

    fn insert(db: &mut TestDb, table: &str, rows: Vec<Vec<Value>>) {
        let rows = rows
            .into_iter()
            .map(|row| row.into_iter().map(Expr::Literal).collect())
            .collect();
        let mut insert = Insert::new(table, Box::new(Values::new(rows)));
        execute(&mut insert, &mut db.ctx()).unwrap();
    }

    /// `low <= column < low + 10`.
    fn delete_range(db: &mut TestDb, table: &str, column: usize, low: i32) {
        let in_range = Expr::binary(
            BinaryOp::And,
            Expr::binary(BinaryOp::Ge, col(column), Expr::Literal(int(low))),
            Expr::binary(BinaryOp::Lt, col(column), Expr::Literal(int(low + 10))),
        );
        let matching = Filter::new(Box::new(SeqScan::new(table)), in_range);
        execute(&mut Delete::new(table, Box::new(matching)), &mut db.ctx()).unwrap();
    }

    fn change_tables(db: &mut TestDb, rng: &mut Rng, next_order_id: &mut i32) {
        match rng.below(4) {
            0 => {
                let existing: Vec<Value> = db
                    .stored_rows("customers")
                    .into_iter()
                    .map(|row| row.values[0].clone())
                    .collect();
                let mut customers = Vec::new();
                let drawn: Vec<i32> = (0..40).filter(|_| rng.below(8) == 0).collect();
                for id in drawn {
                    if !existing.contains(&int(id)) {
                        let active = [Value::Boolean(true), Value::Boolean(false), Value::Null];
                        customers.push(vec![
                            int(id),
                            text(&format!("c{id}")),
                            active[rng.below(3)].clone(),
                        ]);
                    }
                }
                insert(db, "customers", customers);
            }
            1 => delete_range(db, "customers", 0, rng.below(40) as i32),
            2 => delete_range(db, "orders", 2, rng.below(100) as i32),
            _ => {
                let orders = (0..rng.below(20))
                    .map(|_| {
                        *next_order_id += 1;
                        let customer = match rng.below(6) {
                            0 => Value::Null,
                            _ => Value::SmallInt(rng.below(45) as i16),
                        };
                        vec![int(*next_order_id), customer, int(rng.below(100) as i32)]
                    })
                    .collect();
                insert(db, "orders", orders);
            }
        }
    }

    #[test]
    fn random_joins_should_match_a_plain_nested_loop() {
        let mut rng = Rng(0xA076_1D64_78BD_642F);
        let mut db = active_customers_db();
        let schema = Schema::new(vec![
            Column::not_null("id", DataType::Integer),
            Column::new("customer_id", DataType::SmallInt),
            Column::new("amount", DataType::Integer),
        ])
        .unwrap();
        db.create_table("orders", schema, &[]);
        let mut next_order_id = 0;
        let predicates = [
            ModelPredicate::None,
            ModelPredicate::IsActive,
            ModelPredicate::AmountAbove50,
        ];

        for round in 1..=80 {
            change_tables(&mut db, &mut rng, &mut next_order_id);
            let join_type = [JoinType::Inner, JoinType::Left][rng.below(2)];
            let predicate = predicates[rng.below(predicates.len())];
            let mut join = IndexNestedLoopJoin::new(
                Box::new(SeqScan::new("orders")),
                col(1),
                "customers_id",
                join_type,
                predicate.expr(),
            );

            let output = execute(&mut join, &mut db.ctx()).unwrap();

            let expected = model_join(&mut db, join_type, predicate);
            assert_eq!(
                values_of(output),
                expected,
                "round {round}: {join_type:?} {predicate:?}"
            );
        }
        assert!(
            db.stored_rows("orders").len() > 50,
            "orders should accumulate"
        );
        assert!(
            db.stored_rows("customers").len() > 5,
            "customers should remain"
        );
    }
}
