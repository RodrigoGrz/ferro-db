//! `Sort` orders its child's rows by sort keys, in memory; `TopN` keeps only the first rows
//! of that order in a bounded heap. Both keep each row's `Rid`.
//!
//! Values compare with `Value::sort_cmp` (numbers interleaved by value, text by its bytes,
//! `FALSE < TRUE`, datetimes by instant), reversed for `DESC`. NULLs go where the key's
//! `NullOrder` says whatever the direction; by default, as in PostgreSQL, NULL is greater than
//! every value: last for `ASC`, first for `DESC`. Rows with equal keys keep their input order,
//! so the output is deterministic and `TopN` gives exactly the rows of `Limit(Sort)`.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::types::Value;

use super::limit::row_count;
use super::materialized::Materialized;
use super::{ExecutionContext, ExecutionError, Executor, Expr, Row, RowCountClause};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullOrder {
    First,
    Last,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    pub expr: Expr,
    pub order: SortOrder,
    pub nulls: NullOrder,
}

impl SortKey {
    /// `ASC`, with NULLs last.
    pub fn asc(expr: Expr) -> Self {
        Self {
            expr,
            order: SortOrder::Asc,
            nulls: NullOrder::Last,
        }
    }

    /// `DESC`, with NULLs first.
    pub fn desc(expr: Expr) -> Self {
        Self {
            expr,
            order: SortOrder::Desc,
            nulls: NullOrder::First,
        }
    }
}

/// A value of a sort key that carries the direction it sorts in, so that the keys of two rows
/// compare with `Ord`.
#[derive(Debug)]
struct SortValue {
    value: Value,
    order: SortOrder,
    nulls: NullOrder,
}

/// Where NULLs go does not depend on the direction: `NULLS FIRST` is first in `DESC` too.
impl Ord for SortValue {
    fn cmp(&self, other: &SortValue) -> Ordering {
        let null_first = match self.nulls {
            NullOrder::First => Ordering::Less,
            NullOrder::Last => Ordering::Greater,
        };
        match (self.value.is_null(), other.value.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => null_first,
            (false, true) => null_first.reverse(),
            (false, false) => match self.order {
                SortOrder::Asc => self.value.sort_cmp(&other.value),
                SortOrder::Desc => other.value.sort_cmp(&self.value),
            },
        }
    }
}

impl PartialOrd for SortValue {
    fn partial_cmp(&self, other: &SortValue) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SortValue {
    fn eq(&self, other: &SortValue) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for SortValue {}

/// The child's rows in the order of `keys`, rows with equal keys in input order, each with
/// its `Rid`. The first call reads and sorts the whole child, in memory.
pub struct Sort {
    child: Box<dyn Executor>,
    keys: Vec<SortKey>,
    state: Materialized<()>,
}

impl Sort {
    pub fn new(child: Box<dyn Executor>, keys: Vec<SortKey>) -> Self {
        Self {
            child,
            keys,
            state: Materialized::Start(()),
        }
    }
}

impl Executor for Sort {
    fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError> {
        self.state
            .next(|()| sorted_rows(self.child.as_mut(), &self.keys, ctx))
    }
}

/// The keys are computed once per row, before sorting: the comparison cannot fail, and a
/// failing key stops the sort before any row is out.
fn sorted_rows(
    child: &mut dyn Executor,
    keys: &[SortKey],
    ctx: &mut ExecutionContext<'_>,
) -> Result<Vec<Row>, ExecutionError> {
    let mut keyed = Vec::new();
    while let Some(row) = child.next(ctx)? {
        keyed.push((sort_key(keys, &row.values)?, row));
    }
    // Stable, so ties keep their input order.
    keyed.sort_by(|lhs, rhs| lhs.0.cmp(&rhs.0));
    Ok(keyed.into_iter().map(|(_, row)| row).collect())
}

/// `ORDER BY … LIMIT count OFFSET offset` keeping only the best `offset + count` rows seen
/// so far, in a max-heap whose root is the worst of them (BusTub's TopN, PostgreSQL's bounded
/// heap sort). The counts are validated before the child is read, as in `Limit`.
pub struct TopN {
    child: Box<dyn Executor>,
    keys: Vec<SortKey>,
    state: Materialized<(Expr, Option<Expr>)>,
}

/// A row in the heap. The arrival number breaks ties, so equal keys keep their input order
/// and the rows are exactly those of a stable sort.
struct RankedRow {
    key: Vec<SortValue>,
    arrival: u64,
    row: Row,
}

impl Ord for RankedRow {
    fn cmp(&self, other: &RankedRow) -> Ordering {
        self.key
            .cmp(&other.key)
            .then(self.arrival.cmp(&other.arrival))
    }
}

impl PartialOrd for RankedRow {
    fn partial_cmp(&self, other: &RankedRow) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for RankedRow {
    fn eq(&self, other: &RankedRow) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for RankedRow {}

impl TopN {
    /// The same rows as `Limit(Sort(child, keys), Some(count), offset)`.
    pub fn new(
        child: Box<dyn Executor>,
        keys: Vec<SortKey>,
        count: Expr,
        offset: Option<Expr>,
    ) -> Self {
        Self {
            child,
            keys,
            state: Materialized::Start((count, offset)),
        }
    }
}

impl Executor for TopN {
    fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError> {
        self.state.next(|(count, offset)| {
            top_rows(
                self.child.as_mut(),
                &self.keys,
                ctx,
                &count,
                offset.as_ref(),
            )
        })
    }
}

fn top_rows(
    child: &mut dyn Executor,
    keys: &[SortKey],
    ctx: &mut ExecutionContext<'_>,
    count: &Expr,
    offset: Option<&Expr>,
) -> Result<Vec<Row>, ExecutionError> {
    let count = row_count(count, RowCountClause::Limit)?;
    let offset = offset
        .map(|offset| row_count(offset, RowCountClause::Offset))
        .transpose()?
        .unwrap_or(0);
    if count == 0 {
        return Ok(Vec::new());
    }
    // Not reserved up front: the bound can be as large as `i64::MAX` rows.
    let bound = usize::try_from(offset.saturating_add(count)).unwrap_or(usize::MAX);
    let mut heap = BinaryHeap::new();
    let mut arrival: u64 = 0;
    while let Some(row) = child.next(ctx)? {
        let ranked = RankedRow {
            key: sort_key(keys, &row.values)?,
            arrival,
            row,
        };
        arrival = arrival.saturating_add(1);
        if heap.len() < bound {
            heap.push(ranked);
        } else if let Some(mut worst) = heap.peek_mut() {
            // A full heap only takes a row better than its worst, which it replaces.
            if ranked < *worst {
                *worst = ranked;
            }
        }
    }
    let to_skip = usize::try_from(offset).unwrap_or(usize::MAX);
    Ok(heap
        .into_sorted_vec()
        .into_iter()
        .skip(to_skip)
        .map(|ranked| ranked.row)
        .collect())
}

fn sort_key(keys: &[SortKey], row: &[Value]) -> Result<Vec<SortValue>, ExecutionError> {
    keys.iter()
        .map(|key| {
            Ok(SortValue {
                value: key.expr.eval(row)?,
                order: key.order,
                nulls: key.nulls,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::test_support::TestDb;
    use crate::execution::{BinaryOp, Delete, Filter, Insert, Limit, SeqScan, Values, execute};
    use crate::table::{Column, Schema};
    use crate::types::{DataType, Date, Finite32, Finite64, Timestamp};

    fn int(value: i32) -> Value {
        Value::Integer(value)
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_string())
    }

    fn col(column: usize) -> Expr {
        Expr::Column(column)
    }

    /// `rows` in the order of their keys, stable.
    fn sorted(keys: &[SortKey], rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
        let mut keyed: Vec<(Vec<SortValue>, Vec<Value>)> = rows
            .into_iter()
            .map(|row| (sort_key(keys, &row).unwrap(), row))
            .collect();
        keyed.sort_by(|lhs, rhs| lhs.0.cmp(&rhs.0));
        keyed.into_iter().map(|(_, row)| row).collect()
    }

    fn one_column(values: Vec<Value>) -> Vec<Vec<Value>> {
        values.into_iter().map(|value| vec![value]).collect()
    }

    fn sorted_column(key: SortKey, values: Vec<Value>) -> Vec<Value> {
        sorted(&[key], one_column(values))
            .into_iter()
            .flatten()
            .collect()
    }

    #[test]
    fn asc_and_desc_should_put_nulls_last_and_first_by_default() {
        let values = vec![int(2), Value::Null, int(3), int(1)];

        let ascending = sorted_column(SortKey::asc(col(0)), values.clone());
        let descending = sorted_column(SortKey::desc(col(0)), values);

        assert_eq!(ascending, [int(1), int(2), int(3), Value::Null]);
        assert_eq!(descending, [Value::Null, int(3), int(2), int(1)]);
    }

    #[test]
    fn explicit_null_order_should_hold_in_both_directions() {
        let values = vec![int(2), Value::Null, int(1)];
        let asc_nulls_first = SortKey {
            nulls: NullOrder::First,
            ..SortKey::asc(col(0))
        };
        let desc_nulls_last = SortKey {
            nulls: NullOrder::Last,
            ..SortKey::desc(col(0))
        };

        let ascending = sorted_column(asc_nulls_first, values.clone());
        let descending = sorted_column(desc_nulls_last, values);

        assert_eq!(ascending, [Value::Null, int(1), int(2)]);
        assert_eq!(descending, [int(2), int(1), Value::Null]);
    }

    #[test]
    fn later_keys_should_only_break_ties() {
        let rows = vec![
            vec![text("b"), int(1)],
            vec![text("a"), int(1)],
            vec![text("b"), int(2)],
            vec![text("a"), int(2)],
        ];
        let keys = [SortKey::asc(col(0)), SortKey::desc(col(1))];

        let output = sorted(&keys, rows);

        assert_eq!(
            output,
            [
                [text("a"), int(2)],
                [text("a"), int(1)],
                [text("b"), int(2)],
                [text("b"), int(1)],
            ]
        );
    }

    #[test]
    fn booleans_timestamps_and_reals_should_sort_like_sort_cmp() {
        let booleans = vec![Value::Boolean(true), Value::Boolean(false)];
        let timestamps = vec![
            Value::Timestamp(Timestamp::MIN),
            Value::Timestamp(Timestamp::MAX),
        ];
        let reals = vec![
            Value::Real(Finite32::new(0.5).unwrap()),
            Value::Real(Finite32::new(-1.5).unwrap()),
        ];

        let booleans = sorted_column(SortKey::asc(col(0)), booleans);
        let timestamps = sorted_column(SortKey::desc(col(0)), timestamps);
        let reals = sorted_column(SortKey::asc(col(0)), reals);

        assert_eq!(booleans, [Value::Boolean(false), Value::Boolean(true)]);
        assert_eq!(
            timestamps,
            [
                Value::Timestamp(Timestamp::MAX),
                Value::Timestamp(Timestamp::MIN),
            ]
        );
        assert_eq!(
            reals,
            [
                Value::Real(Finite32::new(-1.5).unwrap()),
                Value::Real(Finite32::new(0.5).unwrap()),
            ]
        );
    }

    #[test]
    fn equal_values_of_different_widths_should_tie() {
        let rows = vec![
            vec![Value::BigInt(1), text("first")],
            vec![Value::SmallInt(1), text("second")],
            vec![Value::Integer(0), text("third")],
        ];

        let output = sorted(&[SortKey::asc(col(0))], rows);

        assert_eq!(
            output,
            [
                [Value::Integer(0), text("third")],
                [Value::BigInt(1), text("first")],
                [Value::SmallInt(1), text("second")],
            ]
        );
    }

    #[test]
    fn values_should_sort_like_sort_cmp() {
        let numbers = vec![
            Value::BigInt(3),
            Value::Double(Finite64::new(2.5).unwrap()),
            Value::SmallInt(1),
        ];
        let texts = vec![text("a "), text("b"), text("a")];
        let dates = vec![
            Value::Date(Date::from_ymd(2026, 1, 2).unwrap()),
            Value::Date(Date::from_ymd(1999, 12, 31).unwrap()),
        ];

        let numbers = sorted_column(SortKey::asc(col(0)), numbers);
        let texts = sorted_column(SortKey::asc(col(0)), texts);
        let dates = sorted_column(SortKey::desc(col(0)), dates);

        assert_eq!(
            numbers,
            [
                Value::SmallInt(1),
                Value::Double(Finite64::new(2.5).unwrap()),
                Value::BigInt(3),
            ]
        );
        assert_eq!(texts, [text("a"), text("a "), text("b")]);
        assert_eq!(
            dates,
            [
                Value::Date(Date::from_ymd(2026, 1, 2).unwrap()),
                Value::Date(Date::from_ymd(1999, 12, 31).unwrap()),
            ]
        );
    }

    fn rows(rows: Vec<Vec<Value>>) -> Box<dyn Executor> {
        let rows = rows
            .into_iter()
            .map(|row| row.into_iter().map(Expr::Literal).collect())
            .collect();
        Box::new(Values::new(rows))
    }

    fn values_of(output: Vec<Row>) -> Vec<Vec<Value>> {
        output.into_iter().map(|row| row.values).collect()
    }

    #[test]
    fn sort_should_order_the_rows_and_keep_ties_in_input_order() {
        let mut db = TestDb::new(4);
        let input = vec![
            vec![int(2), text("first two")],
            vec![int(1), text("one")],
            vec![Value::Null, text("null")],
            vec![int(2), text("second two")],
        ];
        let mut sort = Sort::new(rows(input), vec![SortKey::asc(col(0))]);

        let output = execute(&mut sort, &mut db.ctx()).unwrap();

        assert_eq!(
            values_of(output),
            [
                [int(1), text("one")],
                [int(2), text("first two")],
                [int(2), text("second two")],
                [Value::Null, text("null")],
            ]
        );
        assert_eq!(sort.next(&mut db.ctx()).unwrap(), None);
    }

    #[test]
    fn sort_without_keys_should_keep_the_input_order() {
        let mut db = TestDb::new(4);
        let input = vec![vec![int(3)], vec![int(1)], vec![int(2)]];
        let mut sort = Sort::new(rows(input.clone()), vec![]);

        let output = execute(&mut sort, &mut db.ctx()).unwrap();

        assert_eq!(values_of(output), input);
    }

    #[test]
    fn sort_of_no_rows_and_of_one_row_should_give_them_back() {
        let mut db = TestDb::new(4);
        let mut empty = Sort::new(rows(vec![]), vec![SortKey::asc(col(0))]);
        let mut single = Sort::new(rows(vec![vec![int(7)]]), vec![SortKey::desc(col(0))]);

        let empty = execute(&mut empty, &mut db.ctx()).unwrap();
        let single = execute(&mut single, &mut db.ctx()).unwrap();

        assert!(empty.is_empty());
        assert_eq!(values_of(single), [[int(7)]]);
    }

    #[test]
    fn sort_should_keep_the_rids_of_a_table_spanning_pages() {
        let mut db = TestDb::new(4);
        let schema = Schema::new(vec![Column::new("n", DataType::Integer)]).unwrap();
        let numbers: Vec<Vec<Value>> = (0..200).map(|n| vec![int((n * 37) % 200)]).collect();
        db.create_table("numbers", schema, &numbers);
        let mut sort = Sort::new(
            Box::new(SeqScan::new("numbers")),
            vec![SortKey::desc(col(0))],
        );

        let output = execute(&mut sort, &mut db.ctx()).unwrap();

        let mut expected = db.stored_rows("numbers");
        expected.sort_by(|lhs, rhs| rhs.values[0].sort_cmp(&lhs.values[0]));
        assert_eq!(output, expected);
    }

    #[test]
    fn a_failing_key_should_end_the_sort() {
        let mut db = TestDb::new(4);
        let one_over_n = Expr::binary(BinaryOp::Div, Expr::Literal(int(1)), col(0));
        let mut sort = Sort::new(
            rows(vec![vec![int(1)], vec![int(0)], vec![int(2)]]),
            vec![SortKey::asc(one_over_n)],
        );

        let err = sort.next(&mut db.ctx()).unwrap_err();

        assert_eq!(err.sqlstate(), "22012");
        assert_eq!(sort.next(&mut db.ctx()).unwrap(), None);
    }

    #[test]
    fn a_failing_child_should_end_the_sort() {
        let mut db = TestDb::new(4);
        let mut sort = Sort::new(
            Box::new(SeqScan::new("missing")),
            vec![SortKey::asc(col(0))],
        );

        let err = sort.next(&mut db.ctx()).unwrap_err();

        assert_eq!(err.sqlstate(), "42P01");
        assert_eq!(sort.next(&mut db.ctx()).unwrap(), None);
    }

    fn bigint(value: i64) -> Expr {
        Expr::Literal(Value::BigInt(value))
    }

    /// `(score, name)` rows with ties and NULLs.
    fn scores() -> Vec<Vec<Value>> {
        [
            (Some(3), "ana"),
            (None, "bia"),
            (Some(1), "caio"),
            (Some(3), "davi"),
            (Some(2), "eva"),
            (None, "fred"),
            (Some(3), "gil"),
            (Some(1), "hugo"),
        ]
        .into_iter()
        .map(|(score, name)| vec![score.map_or(Value::Null, int), text(name)])
        .collect()
    }

    fn failing_first_row() -> Box<dyn Executor> {
        let one_over_zero =
            Expr::binary(BinaryOp::Div, Expr::Literal(int(1)), Expr::Literal(int(0)));
        Box::new(Values::new(vec![vec![one_over_zero]]))
    }

    #[test]
    fn top_n_should_give_the_rows_of_limit_over_sort() {
        let mut db = TestDb::new(4);
        let keys = vec![SortKey::desc(col(0))];
        let cases = [
            (1, None),
            (3, None),
            (2, Some(2)),
            (4, Some(5)),
            (20, Some(1)),
            (3, Some(8)),
        ];

        for (count, offset) in cases {
            let mut top_n = TopN::new(
                rows(scores()),
                keys.clone(),
                bigint(count),
                offset.map(bigint),
            );
            let mut limit = Limit::new(
                Box::new(Sort::new(rows(scores()), keys.clone())),
                Some(bigint(count)),
                offset.map(bigint),
            );

            let from_top_n = execute(&mut top_n, &mut db.ctx()).unwrap();
            let from_limit = execute(&mut limit, &mut db.ctx()).unwrap();

            assert_eq!(from_top_n, from_limit, "LIMIT {count} OFFSET {offset:?}");
            assert_eq!(top_n.next(&mut db.ctx()).unwrap(), None);
        }
    }

    #[test]
    fn top_n_should_keep_ties_in_input_order() {
        let mut db = TestDb::new(4);
        let mut top_n = TopN::new(rows(scores()), vec![SortKey::desc(col(0))], bigint(4), None);

        let output = execute(&mut top_n, &mut db.ctx()).unwrap();

        assert_eq!(
            values_of(output),
            [
                [Value::Null, text("bia")],
                [Value::Null, text("fred")],
                [int(3), text("ana")],
                [int(3), text("davi")],
            ]
        );
    }

    #[test]
    fn top_n_without_keys_should_give_the_first_rows() {
        let mut db = TestDb::new(4);
        let mut top_n = TopN::new(rows(scores()), vec![], bigint(3), Some(bigint(2)));

        let output = execute(&mut top_n, &mut db.ctx()).unwrap();

        assert_eq!(values_of(output), scores()[2..5]);
    }

    #[test]
    fn a_failing_count_should_end_the_top_n() {
        let mut db = TestDb::new(4);
        let one_over_zero = Expr::binary(BinaryOp::Div, bigint(1), bigint(0));
        let mut top_n = TopN::new(
            rows(scores()),
            vec![SortKey::asc(col(0))],
            one_over_zero,
            None,
        );

        let err = top_n.next(&mut db.ctx()).unwrap_err();

        assert_eq!(err.sqlstate(), "22012");
        assert_eq!(top_n.next(&mut db.ctx()).unwrap(), None);
    }

    #[test]
    fn top_n_with_a_count_of_zero_should_not_read_the_child() {
        let mut db = TestDb::new(4);
        let mut top_n = TopN::new(
            failing_first_row(),
            vec![SortKey::asc(col(0))],
            bigint(0),
            None,
        );

        let output = execute(&mut top_n, &mut db.ctx()).unwrap();

        assert!(output.is_empty());
    }

    #[test]
    fn top_n_with_a_huge_count_should_give_every_row_after_the_offset() {
        let mut db = TestDb::new(4);
        let mut top_n = TopN::new(
            rows(scores()),
            vec![SortKey::asc(col(1))],
            bigint(i64::MAX),
            Some(bigint(i64::MAX - 1)),
        );
        let mut all_but_first = TopN::new(
            rows(scores()),
            vec![SortKey::asc(col(1))],
            bigint(i64::MAX),
            Some(bigint(1)),
        );

        let nothing = execute(&mut top_n, &mut db.ctx()).unwrap();
        let output = execute(&mut all_but_first, &mut db.ctx()).unwrap();

        assert!(nothing.is_empty());
        let names: Vec<Value> = values_of(output)
            .into_iter()
            .map(|row| row[1].clone())
            .collect();
        assert_eq!(
            names,
            ["bia", "caio", "davi", "eva", "fred", "gil", "hugo"].map(text)
        );
    }

    #[test]
    fn top_n_should_validate_its_counts_before_reading_the_child() {
        let cases = [
            (Expr::Literal(Value::Null), None, "2201W"),
            (bigint(-1), None, "2201W"),
            (bigint(1), Some(bigint(-1)), "2201X"),
            (Expr::Literal(Value::Text("1".to_string())), None, "42804"),
        ];

        for (count, offset, sqlstate) in cases {
            let mut db = TestDb::new(4);
            let mut top_n = TopN::new(
                failing_first_row(),
                vec![SortKey::asc(col(0))],
                count,
                offset,
            );

            let err = top_n.next(&mut db.ctx()).unwrap_err();

            assert_eq!(err.sqlstate(), sqlstate);
            assert_eq!(top_n.next(&mut db.ctx()).unwrap(), None);
        }
    }

    #[test]
    fn a_failing_key_should_end_the_top_n() {
        let mut db = TestDb::new(4);
        let one_over_n = Expr::binary(BinaryOp::Div, Expr::Literal(int(1)), col(0));
        let mut top_n = TopN::new(
            rows(vec![vec![int(1)], vec![int(0)]]),
            vec![SortKey::asc(one_over_n)],
            bigint(1),
            None,
        );

        let err = top_n.next(&mut db.ctx()).unwrap_err();

        assert_eq!(err.sqlstate(), "22012");
        assert_eq!(top_n.next(&mut db.ctx()).unwrap(), None);
    }

    #[test]
    fn top_n_should_keep_the_rids() {
        let mut db = TestDb::new(4);
        let schema = Schema::new(vec![Column::new("n", DataType::Integer)]).unwrap();
        let numbers: Vec<Vec<Value>> = (0..100).map(|n| vec![int((n * 37) % 10)]).collect();
        db.create_table("numbers", schema, &numbers);
        let keys = vec![SortKey::asc(col(0))];
        let mut top_n = TopN::new(
            Box::new(SeqScan::new("numbers")),
            keys.clone(),
            bigint(15),
            None,
        );
        let mut limit = Limit::new(
            Box::new(Sort::new(Box::new(SeqScan::new("numbers")), keys)),
            Some(bigint(15)),
            None,
        );

        let from_top_n = execute(&mut top_n, &mut db.ctx()).unwrap();
        let from_limit = execute(&mut limit, &mut db.ctx()).unwrap();

        assert!(from_top_n.iter().all(|row| row.rid.is_some()));
        assert_eq!(from_top_n, from_limit);
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

        /// A `(n INTEGER, name VARCHAR, score DOUBLE PRECISION)` row from small pools, so that
        /// keys tie often.
        fn fact(&mut self) -> Vec<Value> {
            const NAMES: [&str; 5] = ["a", "a ", "b", "B", ""];
            vec![
                self.maybe(5, |rng| rng.below(6) as i32 - 2)
                    .map_or(Value::Null, int),
                self.maybe(5, |rng| text(NAMES[rng.below(NAMES.len())]))
                    .unwrap_or(Value::Null),
                self.maybe(5, |rng| (rng.below(5) as f64 - 2.0) / 2.0)
                    .map_or(Value::Null, |score| {
                        Value::Double(Finite64::new(score).unwrap())
                    }),
            ]
        }

        fn sort_key(&mut self) -> SortKey {
            SortKey {
                expr: col(self.below(3)),
                order: [SortOrder::Asc, SortOrder::Desc][self.below(2)],
                nulls: [NullOrder::First, NullOrder::Last][self.below(2)],
            }
        }

        fn row_count(&mut self) -> Option<u64> {
            self.maybe(4, |rng| rng.below(12) as u64)
        }
    }

    /// A stored row as plain Rust values, compared without `Value`.
    type ModelFact = (Option<i32>, Option<String>, Option<f64>);

    fn model_fact(values: &[Value]) -> ModelFact {
        let n = match values[0] {
            Value::Integer(n) => Some(n),
            _ => None,
        };
        let name = match &values[1] {
            Value::Text(name) => Some(name.clone()),
            _ => None,
        };
        let score = match values[2] {
            Value::Double(score) => Some(score.get()),
            _ => None,
        };
        (n, name, score)
    }

    fn model_compare<T>(
        lhs: Option<T>,
        rhs: Option<T>,
        key: &SortKey,
        compare: impl Fn(T, T) -> Ordering,
    ) -> Ordering {
        let null_first = match key.nulls {
            NullOrder::First => Ordering::Less,
            NullOrder::Last => Ordering::Greater,
        };
        match (lhs, rhs) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => null_first,
            (Some(_), None) => null_first.reverse(),
            (Some(lhs), Some(rhs)) => match key.order {
                SortOrder::Asc => compare(lhs, rhs),
                SortOrder::Desc => compare(rhs, lhs),
            },
        }
    }

    fn model_order(lhs: &ModelFact, rhs: &ModelFact, keys: &[SortKey]) -> Ordering {
        keys.iter()
            .map(|key| match key.expr {
                Expr::Column(0) => model_compare(lhs.0, rhs.0, key, |l, r| l.cmp(&r)),
                Expr::Column(1) => {
                    model_compare(lhs.1.as_ref(), rhs.1.as_ref(), key, |l, r| l.cmp(r))
                }
                _ => model_compare(lhs.2, rhs.2, key, |l, r| l.total_cmp(&r)),
            })
            .find(|order| order.is_ne())
            .unwrap_or(Ordering::Equal)
    }

    /// The table's rows in heap order (a `SeqScan`'s), stably sorted by the model.
    fn model_sorted(db: &mut TestDb, keys: &[SortKey]) -> Vec<Row> {
        let mut stored: Vec<(ModelFact, Row)> = db
            .stored_rows("facts")
            .into_iter()
            .map(|row| (model_fact(&row.values), row))
            .collect();
        stored.sort_by(|lhs, rhs| model_order(&lhs.0, &rhs.0, keys));
        stored.into_iter().map(|(_, row)| row).collect()
    }

    fn count_expr(count: Option<u64>) -> Option<Expr> {
        count.map(|count| bigint(count as i64))
    }

    fn change_facts(db: &mut TestDb, rng: &mut Rng) {
        if rng.below(4) == 0 {
            let n_equals = Expr::binary(
                BinaryOp::Eq,
                col(0),
                Expr::Literal(int(rng.below(6) as i32 - 2)),
            );
            let matching = Filter::new(Box::new(SeqScan::new("facts")), n_equals);
            execute(&mut Delete::new("facts", Box::new(matching)), &mut db.ctx()).unwrap();
        } else {
            let facts: Vec<Vec<Expr>> = (0..rng.below(25))
                .map(|_| rng.fact().into_iter().map(Expr::Literal).collect())
                .collect();
            let mut insert = Insert::new("facts", Box::new(Values::new(facts)));
            execute(&mut insert, &mut db.ctx()).unwrap();
        }
    }

    #[test]
    fn random_sorts_and_limits_should_match_a_model() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let mut db = TestDb::new(8);
        let schema = Schema::new(vec![
            Column::new("n", DataType::Integer),
            Column::new("name", DataType::VARCHAR_MAX),
            Column::new("score", DataType::Double),
        ])
        .unwrap();
        db.create_table("facts", schema, &[]);

        for round in 1..=80 {
            change_facts(&mut db, &mut rng);
            let keys: Vec<SortKey> = (0..1 + rng.below(3)).map(|_| rng.sort_key()).collect();
            let (count, offset) = (rng.row_count(), rng.row_count());
            let scan = || Box::new(SeqScan::new("facts"));

            let sorted = execute(&mut Sort::new(scan(), keys.clone()), &mut db.ctx()).unwrap();
            let limited = execute(
                &mut Limit::new(
                    Box::new(Sort::new(scan(), keys.clone())),
                    count_expr(count),
                    count_expr(offset),
                ),
                &mut db.ctx(),
            )
            .unwrap();

            let expected = model_sorted(&mut db, &keys);
            let context = format!("round {round}: {keys:?} LIMIT {count:?} OFFSET {offset:?}");
            assert_eq!(sorted, expected, "{context}");
            let window: Vec<Row> = expected
                .into_iter()
                .skip(offset.unwrap_or(0) as usize)
                .take(count.map_or(usize::MAX, |count| count as usize))
                .collect();
            assert_eq!(limited, window, "{context}");
            if let Some(count) = count {
                let mut top_n = TopN::new(
                    scan(),
                    keys.clone(),
                    bigint(count as i64),
                    count_expr(offset),
                );
                let top = execute(&mut top_n, &mut db.ctx()).unwrap();
                assert_eq!(top, window, "{context}");
            }
        }
        assert!(db.stored_rows("facts").len() > 50, "the table should grow");
    }
}
