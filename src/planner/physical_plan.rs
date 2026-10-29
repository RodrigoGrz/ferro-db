//! The physical plan of a `SELECT`: the executors that will run it, chosen by fixed rules from
//! the logical plan, as a tree that can be printed (`EXPLAIN`) and compared before it becomes
//! `Box<dyn Executor>` (`into_executor`).

use crate::execution::{
    Aggregate, Aggregation, BinaryOp, Executor, Expr, Filter, HashJoin, IndexNestedLoopJoin,
    IndexScan, JoinInput, JoinType, Limit, Projection, SeqScan, Sort, SortKey, TopN, Values,
    converts_to_key,
};

use crate::catalog::Catalog;
use crate::types::{DataType, Value};

use super::PlannerError;
use super::logical_plan::{
    ColumnName, LogicalPlan, OutputColumn, aggregate_columns, output_names, table_columns,
};
use super::predicate::{Side, conjunct_refs, conjunction, conjuncts, rebased, side};
use super::pushdown::push_down_filters;

#[derive(Debug, Clone, PartialEq)]
pub enum PhysicalPlan {
    SeqScan {
        table: String,
        alias: Option<String>,
        columns: Vec<String>,
    },
    /// The rows of the table whose `index` key equals `key`, a literal.
    IndexScan {
        table: String,
        alias: Option<String>,
        columns: Vec<String>,
        index: String,
        key: Expr,
    },
    /// `SELECT` without `FROM`: one row of no values.
    OneRow,
    Filter {
        input: Box<PhysicalPlan>,
        predicate: Expr,
    },
    /// `keys` pairs an expression over the left row with one over the right row; `predicate` is
    /// the rest of the `ON`, over the joined row. Without keys, every pair is tried.
    HashJoin {
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        keys: Vec<(Expr, Expr)>,
        join_type: JoinType,
        predicate: Option<Expr>,
    },
    /// One descent of `index`, on the column at `index_column` of `table`, per outer row; the
    /// table's columns follow the outer ones.
    IndexNestedLoopJoin {
        outer: Box<PhysicalPlan>,
        outer_key: Expr,
        index: String,
        index_column: usize,
        table: String,
        alias: Option<String>,
        columns: Vec<String>,
        join_type: JoinType,
        predicate: Option<Expr>,
    },
    Aggregation {
        input: Box<PhysicalPlan>,
        group_by: Vec<Expr>,
        aggregates: Vec<Aggregate>,
    },
    Sort {
        input: Box<PhysicalPlan>,
        keys: Vec<SortKey>,
    },
    TopN {
        input: Box<PhysicalPlan>,
        keys: Vec<SortKey>,
        count: Expr,
        offset: Option<Expr>,
    },
    Limit {
        input: Box<PhysicalPlan>,
        count: Option<Expr>,
        offset: Option<Expr>,
    },
    Projection {
        input: Box<PhysicalPlan>,
        columns: Vec<OutputColumn>,
    },
}

impl PhysicalPlan {
    /// The names of the values of each row this node gives.
    pub fn columns(&self) -> Vec<ColumnName> {
        match self {
            PhysicalPlan::SeqScan {
                table,
                alias,
                columns,
            }
            | PhysicalPlan::IndexScan {
                table,
                alias,
                columns,
                ..
            } => table_columns(table, alias.as_deref(), columns),
            PhysicalPlan::OneRow => Vec::new(),
            PhysicalPlan::Filter { input, .. }
            | PhysicalPlan::Sort { input, .. }
            | PhysicalPlan::TopN { input, .. }
            | PhysicalPlan::Limit { input, .. } => input.columns(),
            PhysicalPlan::HashJoin { left, right, .. } => {
                [left.columns(), right.columns()].concat()
            }
            PhysicalPlan::IndexNestedLoopJoin {
                outer,
                table,
                alias,
                columns,
                ..
            } => [
                outer.columns(),
                table_columns(table, alias.as_deref(), columns),
            ]
            .concat(),
            PhysicalPlan::Aggregation {
                input,
                group_by,
                aggregates,
            } => aggregate_columns(&input.columns(), group_by, aggregates),
            PhysicalPlan::Projection { columns, .. } => output_names(columns),
        }
    }

    /// The tree of executors that runs this plan.
    pub fn into_executor(self) -> Box<dyn Executor> {
        match self {
            PhysicalPlan::SeqScan { table, .. } => Box::new(SeqScan::new(&table)),
            PhysicalPlan::IndexScan { index, key, .. } => Box::new(IndexScan::point(&index, key)),
            PhysicalPlan::OneRow => Box::new(Values::new(vec![Vec::new()])),
            PhysicalPlan::Filter { input, predicate } => {
                Box::new(Filter::new(input.into_executor(), predicate))
            }
            PhysicalPlan::HashJoin {
                left,
                right,
                keys,
                join_type,
                predicate,
            } => Box::new(HashJoin::new(
                join_input(*left),
                join_input(*right),
                keys,
                join_type,
                predicate,
            )),
            PhysicalPlan::IndexNestedLoopJoin {
                outer,
                outer_key,
                index,
                join_type,
                predicate,
                ..
            } => Box::new(IndexNestedLoopJoin::new(
                outer.into_executor(),
                outer_key,
                &index,
                join_type,
                predicate,
            )),
            PhysicalPlan::Aggregation {
                input,
                group_by,
                aggregates,
            } => Box::new(Aggregation::new(
                input.into_executor(),
                group_by,
                aggregates,
            )),
            PhysicalPlan::Sort { input, keys } => Box::new(Sort::new(input.into_executor(), keys)),
            PhysicalPlan::TopN {
                input,
                keys,
                count,
                offset,
            } => Box::new(TopN::new(input.into_executor(), keys, count, offset)),
            PhysicalPlan::Limit {
                input,
                count,
                offset,
            } => Box::new(Limit::new(input.into_executor(), count, offset)),
            PhysicalPlan::Projection { input, columns } => Box::new(Projection::new(
                input.into_executor(),
                columns.into_iter().map(|column| column.expr).collect(),
            )),
        }
    }

    /// The nodes this one reads as executors, left before right.
    pub(super) fn inputs(&self) -> Vec<&PhysicalPlan> {
        match self {
            PhysicalPlan::SeqScan { .. }
            | PhysicalPlan::IndexScan { .. }
            | PhysicalPlan::OneRow => Vec::new(),
            PhysicalPlan::Filter { input, .. }
            | PhysicalPlan::Aggregation { input, .. }
            | PhysicalPlan::Sort { input, .. }
            | PhysicalPlan::TopN { input, .. }
            | PhysicalPlan::Limit { input, .. }
            | PhysicalPlan::Projection { input, .. } => vec![input],
            PhysicalPlan::IndexNestedLoopJoin { outer, .. } => vec![outer],
            PhysicalPlan::HashJoin { left, right, .. } => vec![left, right],
        }
    }
}

/// The physical plan of `plan`, reading the catalog for the indexes.
pub fn plan_physical(plan: LogicalPlan, catalog: &Catalog) -> Result<PhysicalPlan, PlannerError> {
    plan_physical_with(plan, catalog, Rules::ALL)
}

/// Which rules apply. With none, the filters stay where the query wrote them, every table is
/// read by a `SeqScan`, every join tries every pair with the whole `ON` as its filter, and
/// `ORDER BY … LIMIT` sorts everything: the plain plan the random test compares the rules
/// against.
#[derive(Debug, Clone, Copy)]
pub(super) struct Rules {
    pub(super) pushdown: bool,
    pub(super) join_keys: bool,
    pub(super) index_joins: bool,
    pub(super) index_scans: bool,
    pub(super) top_n: bool,
}

impl Rules {
    pub(super) const ALL: Rules = Rules {
        pushdown: true,
        join_keys: true,
        index_joins: true,
        index_scans: true,
        top_n: true,
    };
    #[cfg(test)]
    pub(super) const NONE: Rules = Rules {
        pushdown: false,
        join_keys: false,
        index_joins: false,
        index_scans: false,
        top_n: false,
    };
}

/// The physical plan of `plan` under `rules`: first the filters pushed down, so that the rules
/// of the translation find them on the tables and in the `ON`s.
pub(super) fn plan_physical_with(
    plan: LogicalPlan,
    catalog: &Catalog,
    rules: Rules,
) -> Result<PhysicalPlan, PlannerError> {
    let plan = match rules.pushdown {
        true => push_down_filters(plan),
        false => plan,
    };
    Planner { catalog, rules }.plan(plan)
}

struct Planner<'a> {
    catalog: &'a Catalog,
    rules: Rules,
}

impl Planner<'_> {
    fn plan(&self, plan: LogicalPlan) -> Result<PhysicalPlan, PlannerError> {
        let boxed = |plan: Box<LogicalPlan>| self.plan(*plan).map(Box::new);
        Ok(match plan {
            LogicalPlan::Scan {
                table,
                alias,
                columns,
            } => PhysicalPlan::SeqScan {
                table,
                alias,
                columns,
            },
            LogicalPlan::OneRow => PhysicalPlan::OneRow,
            LogicalPlan::Filter { input, predicate } => {
                let input = match *input {
                    LogicalPlan::Scan {
                        table,
                        alias,
                        columns,
                    } if self.rules.index_scans => {
                        self.filtered_scan(table, alias, columns, &predicate)?
                    }
                    input => self.plan(input)?,
                };
                // The whole predicate stays above an index scan: the conjunct the index
                // served is TRUE again, and the rest still filters.
                PhysicalPlan::Filter {
                    input: Box::new(input),
                    predicate,
                }
            }
            LogicalPlan::Join {
                left,
                right,
                join_type,
                on,
            } => self.join(*left, *right, join_type, on)?,
            LogicalPlan::Aggregate {
                input,
                group_by,
                aggregates,
            } => PhysicalPlan::Aggregation {
                input: boxed(input)?,
                group_by,
                aggregates,
            },
            LogicalPlan::Sort { input, keys } => PhysicalPlan::Sort {
                input: boxed(input)?,
                keys,
            },
            LogicalPlan::Limit {
                input,
                count,
                offset,
            } => match (*input, count) {
                // A count over a sort keeps only the first rows in a bounded heap.
                (LogicalPlan::Sort { input, keys }, Some(count)) if self.rules.top_n => {
                    PhysicalPlan::TopN {
                        input: boxed(input)?,
                        keys,
                        count,
                        offset,
                    }
                }
                (input, count) => PhysicalPlan::Limit {
                    input: Box::new(self.plan(input)?),
                    count,
                    offset,
                },
            },
            LogicalPlan::Project { input, columns } => PhysicalPlan::Projection {
                input: boxed(input)?,
                columns,
            },
        })
    }

    /// An `IndexNestedLoopJoin` when an index of the right table serves one of the keys, else a
    /// `HashJoin` on the equalities of the `ON` between the two sides; the rest of the `ON` is
    /// the filter.
    fn join(
        &self,
        left: LogicalPlan,
        right: LogicalPlan,
        join_type: JoinType,
        on: Option<Expr>,
    ) -> Result<PhysicalPlan, PlannerError> {
        let conjuncts = on.map(conjuncts).unwrap_or_default();
        let mut condition = match self.rules.join_keys {
            true => JoinCondition::split(conjuncts, left.columns().len()),
            false => JoinCondition {
                keys: Vec::new(),
                rest: conjuncts,
            },
        };
        let can_use_index =
            self.rules.index_joins && matches!(join_type, JoinType::Inner | JoinType::Left);
        if let LogicalPlan::Scan {
            table,
            alias,
            columns,
        } = &right
            && can_use_index
            && let Some(choice) = self.index_key(&left, table, &condition.keys)?
        {
            let key = condition.keys.remove(choice.key);
            let others = condition.keys.into_iter().map(|key| key.equality);
            return Ok(PhysicalPlan::IndexNestedLoopJoin {
                outer: Box::new(self.plan(left)?),
                outer_key: key.left,
                index: choice.index,
                index_column: choice.column,
                table: table.clone(),
                alias: alias.clone(),
                columns: columns.clone(),
                join_type,
                predicate: conjunction(others.chain(condition.rest).collect()),
            });
        }
        Ok(PhysicalPlan::HashJoin {
            left: Box::new(self.plan(left)?),
            right: Box::new(self.plan(right)?),
            keys: condition
                .keys
                .into_iter()
                .map(|key| (key.left, key.right))
                .collect(),
            join_type,
            predicate: conjunction(condition.rest),
        })
    }

    /// A table read under `predicate`: through an index when a conjunct `column = literal` can
    /// use one, else whole.
    fn filtered_scan(
        &self,
        table: String,
        alias: Option<String>,
        columns: Vec<String>,
        predicate: &Expr,
    ) -> Result<PhysicalPlan, PlannerError> {
        Ok(match self.index_point(&table, predicate)? {
            Some((index, key)) => PhysicalPlan::IndexScan {
                table,
                alias,
                columns,
                index,
                key,
            },
            None => PhysicalPlan::SeqScan {
                table,
                alias,
                columns,
            },
        })
    }

    /// An index of `table` that finds the rows of a conjunct `column = literal`, with the
    /// literal as the key. The literal is not NULL and converts to the key's type without an
    /// error (`converts_to_key`), so the index finds the rows `=` would keep; a float for an
    /// integer key would fail there (42804) where `=` compares.
    fn index_point(
        &self,
        table: &str,
        predicate: &Expr,
    ) -> Result<Option<(String, Expr)>, PlannerError> {
        let schema = &self.catalog.table(table)?.schema;
        let indexes = self.catalog.table_indexes(table)?;
        for conjunct in conjunct_refs(predicate) {
            let Some((column, value)) = column_equals_literal(conjunct) else {
                continue;
            };
            let Some(column_type) = schema.columns().get(column).map(|column| column.data_type)
            else {
                continue;
            };
            if !value
                .data_type()
                .is_some_and(|ty| converts_to_key(ty, column_type))
            {
                continue;
            }
            if let Some(index) = indexes
                .iter()
                .find(|index| usize::from(index.column) == column)
            {
                return Ok(Some((index.name.clone(), Expr::Literal(value.clone()))));
            }
        }
        Ok(None)
    }

    /// The key an index of `table` can serve: its right side is the indexed column, and its
    /// left side a column of the outer rows whose type converts to the key's without an error
    /// (`converts_to_key`). Any other key would fail as an index bound (42804) where `=`
    /// compares or fails with 42883, so it is left to the hash join.
    fn index_key(
        &self,
        outer: &LogicalPlan,
        table: &str,
        keys: &[JoinKey],
    ) -> Result<Option<IndexChoice>, PlannerError> {
        let Some(outer_types) = self.column_types(outer)? else {
            return Ok(None);
        };
        let schema = &self.catalog.table(table)?.schema;
        for index in self.catalog.table_indexes(table)? {
            let column = usize::from(index.column);
            let Some(indexed) = schema.columns().get(column) else {
                continue;
            };
            let serves = |key: &JoinKey| {
                let Expr::Column(outer_column) = key.left else {
                    return false;
                };
                key.right == Expr::Column(column)
                    && outer_types
                        .get(outer_column)
                        .is_some_and(|ty| converts_to_key(*ty, indexed.data_type))
            };
            if let Some(key) = keys.iter().position(serves) {
                return Ok(Some(IndexChoice {
                    key,
                    index: index.name.clone(),
                    column,
                }));
            }
        }
        Ok(None)
    }

    /// The types of the columns of a `FROM`, from the tables of its scans, through the filters
    /// pushed down onto them (a filter keeps its input's columns); `None` for anything else,
    /// which has no types until there is a type system.
    fn column_types(&self, plan: &LogicalPlan) -> Result<Option<Vec<DataType>>, PlannerError> {
        Ok(match plan {
            LogicalPlan::Filter { input, .. } => self.column_types(input)?,
            LogicalPlan::Scan { table, .. } => Some(
                self.catalog
                    .table(table)?
                    .schema
                    .columns()
                    .iter()
                    .map(|column| column.data_type)
                    .collect(),
            ),
            LogicalPlan::Join { left, right, .. } => {
                match (self.column_types(left)?, self.column_types(right)?) {
                    (Some(left), Some(right)) => Some([left, right].concat()),
                    _ => None,
                }
            }
            _ => None,
        })
    }
}

/// The key of a join an index serves: its position among the keys, the index, and the indexed
/// column's position in its table.
struct IndexChoice {
    key: usize,
    index: String,
    column: usize,
}

/// The `ON` of a join as hash keys and the rest of its conjuncts.
struct JoinCondition {
    keys: Vec<JoinKey>,
    rest: Vec<Expr>,
}

/// A conjunct `left = right` with `left` over the left row and `right` over the right row, as
/// a hash join evaluates its keys; `equality` is the conjunct as written, over the joined row.
struct JoinKey {
    left: Expr,
    right: Expr,
    equality: Expr,
}

impl JoinCondition {
    /// Each conjunct that compares one side with the other is a key; the right side of a key is
    /// rebased onto the right row by taking the left's width off its columns.
    fn split(conjuncts: Vec<Expr>, left_width: usize) -> Self {
        let mut condition = JoinCondition {
            keys: Vec::new(),
            rest: Vec::new(),
        };
        for conjunct in conjuncts {
            match join_key(&conjunct, left_width) {
                Some(key) => condition.keys.push(key),
                None => condition.rest.push(conjunct),
            }
        }
        condition
    }
}

fn join_key(conjunct: &Expr, left_width: usize) -> Option<JoinKey> {
    let Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
    } = conjunct
    else {
        return None;
    };
    let (left, right) = match (side(left, left_width), side(right, left_width)) {
        (Side::Left, Side::Right) => (left, right),
        (Side::Right, Side::Left) => (right, left),
        _ => return None,
    };
    Some(JoinKey {
        left: (**left).clone(),
        right: rebased(right, left_width),
        equality: conjunct.clone(),
    })
}

/// The column and the literal of `column = literal`, written either way round.
fn column_equals_literal(expr: &Expr) -> Option<(usize, &Value)> {
    let Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
    } = expr
    else {
        return None;
    };
    match (&**left, &**right) {
        (Expr::Column(column), Expr::Literal(value))
        | (Expr::Literal(value), Expr::Column(column)) => Some((*column, value)),
        _ => None,
    }
}

/// One side of a hash join, with the width its rows have, which an outer join pads with NULLs.
fn join_input(plan: PhysicalPlan) -> JoinInput {
    JoinInput {
        width: plan.columns().len(),
        child: plan.into_executor(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::execution::execute;
    use crate::execution::test_support::TestDb;
    use crate::planner::{plan_select, run_sql};
    use crate::sql::{Statement, parse};

    /// `users (id, name)` with the unique index `users_id`, `orders (id, user_id, total)` and
    /// `tags (user_id BIGINT, label, score DOUBLE PRECISION)`.
    fn db() -> TestDb {
        let mut db = TestDb::new(8);
        run_sql(
            "CREATE TABLE users (id INTEGER NOT NULL, name VARCHAR(20)); \
             CREATE UNIQUE INDEX users_id ON users (id); \
             CREATE TABLE orders (id INTEGER NOT NULL, user_id INTEGER, total BIGINT); \
             INSERT INTO users VALUES (1, 'ana'), (2, 'rui'), (3, NULL); \
             INSERT INTO orders VALUES (10, 1, 50), (11, 1, 70), (12, 2, 20), (13, NULL, 5); \
             CREATE TABLE tags (user_id BIGINT, label VARCHAR(10), score DOUBLE PRECISION); \
             INSERT INTO tags VALUES (1, 'x', 1.0), (NULL, 'y', 2.5); \
             CREATE TABLE empty (x INTEGER); \
             CREATE TABLE nobody (id INTEGER NOT NULL); \
             CREATE UNIQUE INDEX nobody_id ON nobody (id); \
             CREATE TABLE events (code VARCHAR(3) NOT NULL, at TIMESTAMP NOT NULL); \
             CREATE UNIQUE INDEX events_code ON events (code); \
             CREATE UNIQUE INDEX events_at ON events (at); \
             INSERT INTO events VALUES ('a', TIMESTAMP '2024-01-31 00:00:00'), \
             ('b', TIMESTAMP '2024-02-01 10:00:00')",
            &mut db.ctx(),
        )
        .unwrap();
        db
    }

    fn rows(db: &mut TestDb, plan: PhysicalPlan) -> Vec<Vec<Value>> {
        let mut executor = plan.into_executor();
        execute(executor.as_mut(), &mut db.ctx())
            .unwrap()
            .into_iter()
            .map(|row| row.values)
            .collect()
    }

    fn scan(table: &str, alias: Option<&str>, columns: &[&str]) -> PhysicalPlan {
        PhysicalPlan::SeqScan {
            table: table.to_string(),
            alias: alias.map(str::to_string),
            columns: columns.iter().map(|name| name.to_string()).collect(),
        }
    }

    fn users() -> PhysicalPlan {
        scan("users", Some("u"), &["id", "name"])
    }

    fn orders() -> PhysicalPlan {
        scan("orders", Some("o"), &["id", "user_id", "total"])
    }

    fn int(value: i32) -> Expr {
        Expr::Literal(Value::Integer(value))
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_string())
    }

    fn eq(left: Expr, right: Expr) -> Expr {
        Expr::binary(BinaryOp::Eq, left, right)
    }

    fn output(expr: Expr, name: &str) -> OutputColumn {
        OutputColumn {
            expr,
            name: name.to_string(),
        }
    }

    fn index_join(join_type: JoinType, predicate: Option<Expr>) -> PhysicalPlan {
        PhysicalPlan::IndexNestedLoopJoin {
            outer: Box::new(orders()),
            outer_key: Expr::Column(1),
            index: "users_id".to_string(),
            index_column: 0,
            table: "users".to_string(),
            alias: Some("u".to_string()),
            columns: vec!["id".to_string(), "name".to_string()],
            join_type,
            predicate,
        }
    }

    #[test]
    fn scans_should_read_the_table_or_its_index() {
        let mut db = db();
        let index_scan = PhysicalPlan::IndexScan {
            table: "users".to_string(),
            alias: None,
            columns: vec!["id".to_string(), "name".to_string()],
            index: "users_id".to_string(),
            key: int(2),
        };

        let all = rows(&mut db, users());
        let by_key = rows(&mut db, index_scan);
        let one_row = rows(&mut db, PhysicalPlan::OneRow);

        assert_eq!(all.len(), 3);
        assert_eq!(by_key, [vec![Value::Integer(2), text("rui")]]);
        assert_eq!(one_row, [Vec::<Value>::new()]);
    }

    #[test]
    fn joins_should_pair_the_rows_by_keys_or_by_index() {
        let mut db = db();
        let hash = PhysicalPlan::HashJoin {
            left: Box::new(orders()),
            right: Box::new(users()),
            keys: vec![(Expr::Column(1), Expr::Column(0))],
            join_type: JoinType::Inner,
            predicate: Some(Expr::binary(
                BinaryOp::Gt,
                Expr::Column(2),
                Expr::Literal(Value::BigInt(30)),
            )),
        };
        let cross = PhysicalPlan::HashJoin {
            left: Box::new(orders()),
            right: Box::new(users()),
            keys: Vec::new(),
            join_type: JoinType::Inner,
            predicate: None,
        };

        let by_hash = rows(&mut db, hash);
        let by_index = rows(&mut db, index_join(JoinType::Left, None));
        let every_pair = rows(&mut db, cross);

        let ana = [Value::Integer(1), text("ana")];
        assert_eq!(
            by_hash,
            [
                [
                    &[Value::Integer(10), Value::Integer(1), Value::BigInt(50)][..],
                    &ana
                ]
                .concat(),
                [
                    &[Value::Integer(11), Value::Integer(1), Value::BigInt(70)][..],
                    &ana
                ]
                .concat(),
            ]
        );
        assert_eq!(by_index.len(), 4);
        assert_eq!(by_index[3][3..], [Value::Null, Value::Null]);
        assert_eq!(every_pair.len(), 12);
    }

    #[test]
    fn the_nodes_above_should_filter_group_sort_and_cut() {
        let mut db = db();
        let filter = PhysicalPlan::Filter {
            input: Box::new(orders()),
            predicate: eq(Expr::Column(1), int(1)),
        };
        let aggregation = PhysicalPlan::Aggregation {
            input: Box::new(orders()),
            group_by: vec![Expr::Column(1)],
            aggregates: vec![Aggregate::CountStar],
        };
        let by_total = || vec![SortKey::asc(Expr::Column(2))];
        let top_n = PhysicalPlan::TopN {
            input: Box::new(orders()),
            keys: by_total(),
            count: int(2),
            offset: Some(int(1)),
        };
        let limit = PhysicalPlan::Limit {
            input: Box::new(PhysicalPlan::Sort {
                input: Box::new(orders()),
                keys: by_total(),
            }),
            count: Some(int(2)),
            offset: Some(int(1)),
        };
        let projection = PhysicalPlan::Projection {
            input: Box::new(orders()),
            columns: vec![output(Expr::Column(0), "id")],
        };

        let filtered = rows(&mut db, filter);
        let groups = rows(&mut db, aggregation);
        let top = rows(&mut db, top_n);
        let limited = rows(&mut db, limit);
        let projected = rows(&mut db, projection);

        assert_eq!(filtered.len(), 2);
        assert_eq!(
            groups,
            [
                vec![Value::Integer(1), Value::BigInt(2)],
                vec![Value::Integer(2), Value::BigInt(1)],
                vec![Value::Null, Value::BigInt(1)],
            ]
        );
        assert_eq!(top, limited);
        assert_eq!(
            top.iter().map(|row| row[0].clone()).collect::<Vec<_>>(),
            [Value::Integer(12), Value::Integer(10)]
        );
        assert_eq!(
            projected,
            [10, 11, 12, 13].map(|id| vec![Value::Integer(id)])
        );
    }

    fn name(table: Option<&str>, name: &str) -> ColumnName {
        ColumnName {
            table: table.map(str::to_string),
            name: name.to_string(),
        }
    }

    #[test]
    fn columns_should_name_the_values_of_each_row() {
        let aggregation = PhysicalPlan::Aggregation {
            input: Box::new(orders()),
            group_by: vec![Expr::Column(1)],
            aggregates: vec![Aggregate::CountStar],
        };
        let projection = PhysicalPlan::Projection {
            input: Box::new(PhysicalPlan::OneRow),
            columns: vec![output(int(1), "one")],
        };

        assert_eq!(
            index_join(JoinType::Inner, None).columns(),
            [
                name(Some("o"), "id"),
                name(Some("o"), "user_id"),
                name(Some("o"), "total"),
                name(Some("u"), "id"),
                name(Some("u"), "name"),
            ]
        );
        assert_eq!(
            aggregation.columns(),
            [name(Some("o"), "user_id"), name(None, "count(*)")]
        );
        assert_eq!(projection.columns(), [name(None, "one")]);
        assert!(PhysicalPlan::OneRow.columns().is_empty());
    }

    #[test]
    fn explain_should_show_each_executor_indented_under_its_reader() {
        let hash = PhysicalPlan::HashJoin {
            left: Box::new(index_join(
                JoinType::Inner,
                Some(eq(Expr::Column(4), Expr::Literal(text("ana")))),
            )),
            right: Box::new(scan("orders", Some("p"), &["id"])),
            keys: vec![(Expr::Column(0), Expr::Column(0))],
            join_type: JoinType::Left,
            predicate: Some(Expr::Literal(Value::Boolean(true))),
        };
        let top_n = PhysicalPlan::TopN {
            input: Box::new(PhysicalPlan::Filter {
                input: Box::new(hash),
                predicate: Expr::IsNull {
                    expr: Box::new(Expr::Column(5)),
                    negated: false,
                },
            }),
            keys: vec![SortKey::asc(Expr::Column(2))],
            count: int(3),
            offset: Some(int(1)),
        };
        let projection = PhysicalPlan::Projection {
            input: Box::new(top_n),
            columns: vec![output(Expr::Column(4), "name")],
        };

        let text = projection.to_string();

        assert_eq!(
            text,
            "Projection: u.name AS name\n\
             \x20 TopN: o.total ASC NULLS LAST; 3 OFFSET 1\n\
             \x20   Filter: p.id IS NULL\n\
             \x20     HashJoin LEFT: o.id = p.id; filter: TRUE\n\
             \x20       IndexNestedLoopJoin INNER: u.id = o.user_id (index users_id ON users AS u); \
             filter: u.name = 'ana'\n\
             \x20         SeqScan: orders AS o\n\
             \x20       SeqScan: orders AS p"
        );
    }

    #[test]
    fn explain_should_show_scans_joins_without_keys_and_the_rest() {
        let first_line = |plan: PhysicalPlan| plan.to_string().lines().next().unwrap().to_string();
        let index_scan = PhysicalPlan::IndexScan {
            table: "users".to_string(),
            alias: None,
            columns: vec!["id".to_string()],
            index: "users_id".to_string(),
            key: int(5),
        };
        let join = |join_type, predicate| PhysicalPlan::HashJoin {
            left: Box::new(orders()),
            right: Box::new(users()),
            keys: Vec::new(),
            join_type,
            predicate,
        };
        let aggregation = PhysicalPlan::Aggregation {
            input: Box::new(orders()),
            group_by: vec![Expr::Column(1)],
            aggregates: vec![Aggregate::Sum(Expr::Column(2))],
        };
        let limit = PhysicalPlan::Limit {
            input: Box::new(PhysicalPlan::Sort {
                input: Box::new(orders()),
                keys: vec![SortKey::asc(Expr::Column(0))],
            }),
            count: None,
            offset: Some(int(2)),
        };

        assert_eq!(
            first_line(index_scan),
            "IndexScan: users (index users_id = 5)"
        );
        assert_eq!(first_line(join(JoinType::Inner, None)), "HashJoin CROSS");
        assert_eq!(
            first_line(join(
                JoinType::Full,
                Some(eq(Expr::Column(1), Expr::Column(3)))
            )),
            "HashJoin FULL: filter: o.user_id = u.id"
        );
        assert_eq!(
            first_line(index_join(JoinType::Left, None)),
            "IndexNestedLoopJoin LEFT: u.id = o.user_id (index users_id ON users AS u)"
        );
        assert_eq!(
            first_line(aggregation),
            "Aggregation: group by o.user_id; sum(o.total)"
        );
        assert_eq!(
            limit.to_string(),
            "Limit: ALL OFFSET 2\n  Sort: o.id ASC NULLS LAST\n    SeqScan: orders AS o"
        );
        assert_eq!(PhysicalPlan::OneRow.to_string(), "OneRow");
    }

    fn logical(db: &TestDb, sql: &str) -> LogicalPlan {
        match parse(sql).unwrap().as_slice() {
            [Statement::Select(select)] => plan_select(select, &db.catalog).unwrap(),
            other => panic!("{other:?}"),
        }
    }

    fn physical(db: &TestDb, sql: &str) -> PhysicalPlan {
        plan_physical(logical(db, sql), &db.catalog).unwrap_or_else(|err| panic!("{err}: {sql}"))
    }

    /// The rows of `sql`, in the order the plan gives them.
    fn select_rows(db: &mut TestDb, sql: &str) -> Vec<Vec<Value>> {
        let plan = physical(db, sql);
        rows(db, plan)
    }

    /// The rows of `sql` in a fixed order, for a query whose order is not specified.
    fn sorted_rows(db: &mut TestDb, sql: &str) -> Vec<Vec<Value>> {
        let mut rows = select_rows(db, sql);
        sort_rows(&mut rows);
        rows
    }

    fn ints(values: &[Option<i32>]) -> Vec<Value> {
        values
            .iter()
            .map(|value| value.map_or(Value::Null, Value::Integer))
            .collect()
    }

    #[test]
    fn a_select_should_run_and_give_its_rows() {
        let mut db = db();

        let filtered = sorted_rows(&mut db, "SELECT name FROM users WHERE id > 1");
        let without_from = select_rows(&mut db, "SELECT 1 + 1 AS two");
        let grouped = select_rows(
            &mut db,
            "SELECT user_id, sum(total) FROM orders GROUP BY user_id ORDER BY 1 NULLS FIRST",
        );
        let top = select_rows(&mut db, "SELECT id FROM orders ORDER BY total DESC LIMIT 2");
        let none_counted = select_rows(&mut db, "SELECT count(*) FROM orders WHERE total > 1000");

        assert_eq!(filtered, [vec![text("rui")], vec![Value::Null]]);
        assert_eq!(without_from, [vec![Value::Integer(2)]]);
        assert_eq!(
            grouped,
            [
                vec![Value::Null, Value::BigInt(5)],
                vec![Value::Integer(1), Value::BigInt(120)],
                vec![Value::Integer(2), Value::BigInt(20)],
            ]
        );
        assert_eq!(top, [ints(&[Some(11)]), ints(&[Some(10)])]);
        assert_eq!(none_counted, [vec![Value::BigInt(0)]]);
    }

    #[test]
    fn every_join_type_should_pair_its_rows() {
        let mut db = db();
        let on = "ON o.user_id = u.id";

        let left = sorted_rows(
            &mut db,
            &format!("SELECT o.id, u.id FROM orders o LEFT JOIN users u {on}"),
        );
        let right = sorted_rows(
            &mut db,
            &format!("SELECT o.id, u.id FROM orders o RIGHT JOIN users u {on}"),
        );
        let full = sorted_rows(
            &mut db,
            &format!("SELECT o.id, u.id FROM orders o FULL JOIN users u {on}"),
        );
        let cross = select_rows(&mut db, "SELECT count(*) FROM users, orders");

        let matched = [
            [Some(10), Some(1)],
            [Some(11), Some(1)],
            [Some(12), Some(2)],
        ];
        let with = |extra: &[[Option<i32>; 2]]| {
            let mut expected: Vec<Vec<Value>> = matched.iter().map(|row| ints(row)).collect();
            expected.extend(extra.iter().map(|row| ints(row)));
            sort_rows(&mut expected);
            expected
        };
        assert_eq!(left, with(&[[Some(13), None]]));
        assert_eq!(right, with(&[[None, Some(3)]]));
        assert_eq!(full, with(&[[Some(13), None], [None, Some(3)]]));
        assert_eq!(cross, [vec![Value::BigInt(12)]]);
    }

    /// The line of the plan of `sql` that starts with `node`.
    fn plan_line(db: &TestDb, sql: &str, node: &str) -> String {
        let text = physical(db, sql).to_string();
        text.lines()
            .map(str::trim_start)
            .find(|line| line.starts_with(node))
            .unwrap_or_else(|| panic!("no {node} in\n{text}"))
            .to_string()
    }

    #[test]
    fn equalities_between_the_two_sides_should_become_hash_keys() {
        let db = db();
        let join = |on: &str| {
            plan_line(
                &db,
                &format!("SELECT 1 FROM users u JOIN orders o ON {on}"),
                "HashJoin",
            )
        };

        assert_eq!(join("o.user_id = u.id"), "HashJoin INNER: u.id = o.user_id");
        assert_eq!(join("u.id = o.user_id"), "HashJoin INNER: u.id = o.user_id");
        assert_eq!(
            join("o.user_id = u.id AND o.total > u.id"),
            "HashJoin INNER: u.id = o.user_id; filter: o.total > u.id"
        );
        // A condition on one table goes down to it instead (see pushdown.rs).
        assert_eq!(
            join("o.user_id = u.id AND o.total > 10"),
            "HashJoin INNER: u.id = o.user_id"
        );
        assert_eq!(
            join("o.user_id = u.id AND u.id + 0 = o.id - 10"),
            "HashJoin INNER: u.id = o.user_id, u.id + 0 = o.id - 10"
        );
    }

    #[test]
    fn conditions_that_do_not_compare_the_two_sides_should_stay_a_filter() {
        let db = db();
        let join = |on: &str| {
            plan_line(
                &db,
                &format!("SELECT 1 FROM users u JOIN orders o ON {on}"),
                "HashJoin",
            )
        };

        // A condition on one table leaves the join for the table: the join is a cross product.
        assert_eq!(join("u.id = 1"), "HashJoin CROSS");
        assert_eq!(
            join("u.id + o.user_id = 2"),
            "HashJoin INNER: filter: (u.id + o.user_id) = 2"
        );
        assert_eq!(
            join("o.user_id = u.id OR o.total > 1"),
            "HashJoin INNER: filter: (o.user_id = u.id) OR (o.total > 1)"
        );
        assert_eq!(
            join("o.total < u.id"),
            "HashJoin INNER: filter: o.total < u.id"
        );
        assert_eq!(join("TRUE"), "HashJoin INNER: filter: TRUE");
    }

    #[test]
    fn hash_keys_should_serve_every_join_type() {
        let db = db();
        let join = |kind: &str| {
            plan_line(
                &db,
                &format!("SELECT 1 FROM users u {kind} JOIN orders o ON o.user_id = u.id"),
                "HashJoin",
            )
        };

        assert_eq!(join("LEFT"), "HashJoin LEFT: u.id = o.user_id");
        assert_eq!(join("RIGHT"), "HashJoin RIGHT: u.id = o.user_id");
        assert_eq!(join("FULL"), "HashJoin FULL: u.id = o.user_id");
    }

    #[test]
    fn without_the_rules_a_join_should_try_every_pair() {
        let db = db();
        let sql = "SELECT 1 FROM users u JOIN orders o ON o.user_id = u.id";

        let plain = plan_physical_with(logical(&db, sql), &db.catalog, Rules::NONE).unwrap();

        assert_eq!(
            plain.to_string().lines().nth(1).unwrap(),
            "  HashJoin INNER: filter: o.user_id = u.id"
        );
    }

    #[test]
    fn a_join_on_the_indexed_column_of_the_right_table_should_use_the_index() {
        let db = db();
        let join = |kind: &str, on: &str| {
            plan_line(
                &db,
                &format!("SELECT 1 FROM orders o {kind} JOIN users u ON {on}"),
                "IndexNestedLoopJoin",
            )
        };

        assert_eq!(
            join("", "o.user_id = u.id"),
            "IndexNestedLoopJoin INNER: u.id = o.user_id (index users_id ON users AS u)"
        );
        assert_eq!(
            join("LEFT", "u.id = o.user_id AND u.id < o.total"),
            "IndexNestedLoopJoin LEFT: u.id = o.user_id (index users_id ON users AS u); \
             filter: u.id < o.total"
        );
        assert_eq!(
            join("", "o.id = u.id + 9 AND o.user_id = u.id"),
            "IndexNestedLoopJoin INNER: u.id = o.user_id (index users_id ON users AS u); \
             filter: o.id = (u.id + 9)"
        );
        // BIGINT is of the INTEGER key's family: the key is converted as an index bound.
        assert_eq!(
            plan_line(
                &db,
                "SELECT 1 FROM tags t JOIN users u ON t.user_id = u.id",
                "IndexNestedLoopJoin"
            ),
            "IndexNestedLoopJoin INNER: u.id = t.user_id (index users_id ON users AS u)"
        );
    }

    #[test]
    fn a_join_the_index_cannot_serve_should_stay_a_hash_join() {
        let db = db();
        let cases = [
            "SELECT 1 FROM orders o RIGHT JOIN users u ON o.user_id = u.id",
            "SELECT 1 FROM orders o FULL JOIN users u ON o.user_id = u.id",
            // The index is on the left table.
            "SELECT 1 FROM users u JOIN orders o ON o.user_id = u.id",
            // A key of another family, or a float for an integer index, would fail as an
            // index bound where `=` compares or fails with another SQLSTATE.
            "SELECT 1 FROM tags t JOIN users u ON t.label = u.id",
            "SELECT 1 FROM tags t JOIN users u ON t.score = u.id",
            // The outer key must be a column.
            "SELECT 1 FROM orders o JOIN users u ON o.user_id + 0 = u.id",
            // The right side is a join, not a table.
            "SELECT 1 FROM orders o JOIN (users u JOIN tags t ON t.label = u.name) \
             ON o.user_id = u.id",
        ];

        for sql in cases {
            let text = physical(&db, sql).to_string();

            assert!(!text.contains("IndexNestedLoopJoin"), "{sql}\n{text}");
            assert!(text.contains("HashJoin"), "{sql}\n{text}");
        }
    }

    #[test]
    fn a_key_of_another_family_should_fail_as_without_the_index() {
        let mut db = db();
        let sql = "SELECT 1 FROM tags t JOIN users u ON t.label = u.id";
        let plain = plan_physical_with(logical(&db, sql), &db.catalog, Rules::NONE).unwrap();
        let chosen = physical(&db, sql);

        let errors = [plain, chosen].map(|plan| {
            let mut executor = plan.into_executor();
            execute(executor.as_mut(), &mut db.ctx())
                .unwrap_err()
                .sqlstate()
        });

        assert_eq!(errors, ["42883", "42883"]);
    }

    /// The node names of the plan of `sql`, from the root down.
    fn plan_nodes(plan: &PhysicalPlan) -> Vec<String> {
        plan.to_string()
            .lines()
            .map(|line| {
                let line = line.trim_start();
                line.split([':', ' ']).next().unwrap_or(line).to_string()
            })
            .collect()
    }

    #[test]
    fn order_by_with_a_limit_should_keep_only_the_first_rows() {
        let db = db();

        let top = physical(
            &db,
            "SELECT id FROM orders ORDER BY total DESC LIMIT 2 OFFSET 1",
        );
        let offset_only = physical(&db, "SELECT id FROM orders ORDER BY total OFFSET 1");
        let unsorted = physical(&db, "SELECT id FROM orders LIMIT 2");

        assert_eq!(
            top.to_string().lines().nth(1).unwrap(),
            "  TopN: orders.total DESC NULLS FIRST; 2 OFFSET 1"
        );
        assert_eq!(
            plan_nodes(&offset_only),
            ["Projection", "Limit", "Sort", "SeqScan"]
        );
        assert_eq!(plan_nodes(&unsorted), ["Projection", "Limit", "SeqScan"]);
    }

    #[test]
    fn without_the_rules_a_limit_should_cut_a_full_sort() {
        let db = db();
        let sql = "SELECT id FROM orders ORDER BY total DESC LIMIT 2";

        let plain = plan_physical_with(logical(&db, sql), &db.catalog, Rules::NONE).unwrap();

        assert_eq!(
            plan_nodes(&plain),
            ["Projection", "Limit", "Sort", "SeqScan"]
        );
    }

    #[test]
    fn an_equality_with_a_literal_on_an_indexed_column_should_read_the_index() {
        let mut db = db();
        let cases = [
            (
                "SELECT name FROM users WHERE id = 2",
                vec![vec![text("rui")]],
            ),
            (
                "SELECT name FROM users WHERE 2 = id",
                vec![vec![text("rui")]],
            ),
            (
                "SELECT name FROM users u WHERE u.name IS NOT NULL AND u.id = 1",
                vec![vec![text("ana")]],
            ),
            // An integer of another width converts to the key, or matches nothing.
            ("SELECT name FROM users WHERE id = 3000000000", Vec::new()),
        ];

        for (sql, expected) in cases {
            let plan = physical(&db, sql);
            let nodes = plan_nodes(&plan);

            assert_eq!(nodes, ["Projection", "Filter", "IndexScan"], "{sql}");
            assert_eq!(rows(&mut db, plan), expected, "{sql}");
        }
        assert_eq!(
            plan_line(&db, "SELECT 1 FROM users u WHERE u.id = 2", "IndexScan"),
            "IndexScan: users AS u (index users_id = 2)"
        );
    }

    #[test]
    fn a_condition_the_index_cannot_serve_should_scan_the_table() {
        let db = db();
        let cases = [
            "SELECT 1 FROM users WHERE id = NULL",
            // Another family, or a float for an integer key, would fail as an index bound.
            "SELECT 1 FROM users WHERE id = 'x'",
            "SELECT 1 FROM users WHERE id = 1.0",
            "SELECT 1 FROM users WHERE id = 1 OR id = 2",
            "SELECT 1 FROM users WHERE id + 0 = 1",
            "SELECT 1 FROM users WHERE name = 'ana'",
            "SELECT 1 FROM users WHERE id > 1",
            // A LEFT join pads its right rows: the WHERE stays above it, off the table.
            "SELECT 1 FROM orders o LEFT JOIN users u ON u.id = o.user_id WHERE u.id = 1",
        ];

        for sql in cases {
            let text = physical(&db, sql).to_string();

            assert!(!text.contains("IndexScan"), "{sql}\n{text}");
        }
        let plain = plan_physical_with(
            logical(&db, "SELECT 1 FROM users WHERE id = 2"),
            &db.catalog,
            Rules::NONE,
        )
        .unwrap();
        assert_eq!(plan_nodes(&plain), ["Projection", "Filter", "SeqScan"]);
    }

    #[test]
    fn a_literal_of_another_family_should_fail_as_without_the_index() {
        let mut db = db();

        let err = run_sql("SELECT 1 FROM users WHERE id = 'x'", &mut db.ctx()).unwrap_err();

        assert_eq!(err.sqlstate(), "42883");
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

        fn chance(&mut self, one_in: usize) -> bool {
            self.below(one_in) == 0
        }

        fn pick<T: Copy>(&mut self, items: &[T]) -> T {
            items[self.below(items.len())]
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Number,
        Text,
    }

    type Table = (&'static str, &'static [(&'static str, Kind)]);

    /// `a` and `b` have unique indexes on `id`, `d` on its text `code`; `a.f` is a DOUBLE
    /// PRECISION, `c.b_id` a BIGINT, and `e` is always empty.
    const TABLES: [Table; 5] = [
        (
            "a",
            &[
                ("id", Kind::Number),
                ("k", Kind::Number),
                ("s", Kind::Text),
                ("f", Kind::Number),
            ],
        ),
        (
            "b",
            &[
                ("id", Kind::Number),
                ("a_id", Kind::Number),
                ("k", Kind::Number),
            ],
        ),
        (
            "c",
            &[
                ("b_id", Kind::Number),
                ("k", Kind::Number),
                ("s", Kind::Text),
            ],
        ),
        ("d", &[("id", Kind::Number), ("code", Kind::Text)]),
        ("e", &[("id", Kind::Number), ("v", Kind::Number)]),
    ];

    /// The columns of `TABLES` with an index.
    const INDEXED: [(&str, &str, Kind); 3] = [
        ("a", "id", Kind::Number),
        ("b", "id", Kind::Number),
        ("d", "code", Kind::Text),
    ];

    fn random_db(rng: &mut Rng) -> TestDb {
        let mut db = TestDb::new(16);
        let mut sql = String::from(
            "CREATE TABLE a (id INTEGER NOT NULL, k INTEGER, s VARCHAR(5), f DOUBLE PRECISION); \
             CREATE UNIQUE INDEX a_id ON a (id); \
             CREATE TABLE b (id INTEGER NOT NULL, a_id INTEGER, k INTEGER); \
             CREATE UNIQUE INDEX b_id ON b (id); \
             CREATE TABLE c (b_id BIGINT, k INTEGER, s VARCHAR(5)); \
             CREATE TABLE d (id INTEGER NOT NULL, code VARCHAR(3) NOT NULL); \
             CREATE UNIQUE INDEX d_code ON d (code); \
             CREATE TABLE e (id INTEGER, v INTEGER);",
        );
        for id in 0..10 {
            sql += &format!(" INSERT INTO d VALUES ({id}, 'x{id}');");
        }
        let small = |rng: &mut Rng| match rng.chance(5) {
            true => "NULL".to_string(),
            false => rng.below(6).to_string(),
        };
        let text = |rng: &mut Rng| rng.pick(&["NULL", "'x'", "'y'", "'z'"]).to_string();
        for id in 0..25 {
            let f = rng.pick(&["NULL", "0.5", "1.0", "2.0"]);
            let (k, s) = (small(rng), text(rng));
            sql += &format!(" INSERT INTO a VALUES ({id}, {k}, {s}, {f});");
            let a_id = match rng.chance(5) {
                true => "NULL".to_string(),
                false => rng.below(30).to_string(),
            };
            let k = small(rng);
            sql += &format!(" INSERT INTO b VALUES ({id}, {a_id}, {k});");
        }
        for _ in 0..20 {
            let b_id = match rng.chance(5) {
                true => "NULL".to_string(),
                false => rng.below(30).to_string(),
            };
            let (k, s) = (small(rng), text(rng));
            sql += &format!(" INSERT INTO c VALUES ({b_id}, {k}, {s});");
        }
        run_sql(&sql, &mut db.ctx()).unwrap();
        db
    }

    /// Random `SELECT`s over `TABLES`, with valid names only: what changes between plans is
    /// the rules, never the meaning.
    struct QueryGen {
        rng: Rng,
        tables: Vec<Table>,
        /// The query compares two kinds, which fails on the rows it is evaluated on: then one
        /// plan may fail where the other does not, since they evaluate it on other rows.
        has_failing_comparison: bool,
    }

    impl QueryGen {
        fn column(&mut self, kind: Option<Kind>) -> Option<String> {
            let mut columns = Vec::new();
            for (table, table_columns) in &self.tables {
                for (column, column_kind) in table_columns.iter() {
                    if kind.is_none_or(|kind| kind == *column_kind) {
                        columns.push(format!("{table}.{column}"));
                    }
                }
            }
            match columns.is_empty() {
                true => None,
                false => Some(columns.swap_remove(self.rng.below(columns.len()))),
            }
        }

        fn number(&mut self) -> String {
            self.column(Some(Kind::Number))
                .unwrap_or_else(|| "1".to_string())
        }

        fn literal(&mut self, kind: Kind) -> String {
            match kind {
                Kind::Number => self
                    .rng
                    .pick(&["0", "1", "2", "5", "1.0", "1.5", "3000000000"]),
                Kind::Text => self
                    .rng
                    .pick(&["'x'", "'y'", "NULL", "'x1'", "'x9'", "'abcd'"]),
            }
            .to_string()
        }

        fn condition(&mut self) -> String {
            // An indexed column often, so that the index scans are tried.
            let indexed: Vec<(&str, &str, Kind)> = INDEXED
                .iter()
                .filter(|(table, ..)| self.tables.iter().any(|(name, _)| name == table))
                .copied()
                .collect();
            if !indexed.is_empty() && self.rng.chance(3) {
                let (table, column, kind) = self.rng.pick(&indexed);
                return format!("{table}.{column} = {}", self.literal(kind));
            }
            let kind = match self.rng.chance(3) {
                true => Kind::Text,
                false => Kind::Number,
            };
            let Some(column) = self.column(Some(kind)) else {
                return "TRUE".to_string();
            };
            match self.rng.below(5) {
                0 | 1 => format!("{column} = {}", self.literal(kind)),
                2 if kind == Kind::Number => format!("{column} > {}", self.rng.below(5)),
                3 => format!("{column} IS NULL"),
                _ => format!("{column} IS NOT NULL"),
            }
        }

        /// An `ON` between the tables so far and `right`: mostly an equality of the same
        /// kind, sometimes with more, sometimes no equality, rarely two kinds that fail.
        fn on(&mut self, right: Table) -> String {
            let left_tables = self.tables.clone();
            self.tables = vec![right];
            let kind = match self.rng.chance(4) {
                true => Kind::Text,
                false => Kind::Number,
            };
            let right_column = self.column(Some(kind));
            self.tables = left_tables;
            let Some(right_column) = right_column else {
                return "TRUE".to_string();
            };
            let other_kind = match kind {
                Kind::Number => Kind::Text,
                Kind::Text => Kind::Number,
            };
            let left_kind = match self.rng.chance(25) {
                true => {
                    self.has_failing_comparison = true;
                    other_kind
                }
                false => kind,
            };
            let Some(left_column) = self.column(Some(left_kind)) else {
                return "TRUE".to_string();
            };
            let mut on = match self.rng.below(6) {
                0 if kind == Kind::Number => format!("{left_column} < {right_column}"),
                _ => format!("{left_column} = {right_column}"),
            };
            if self.rng.chance(4) {
                self.tables.push(right);
                on = format!("{on} AND {}", self.condition());
                self.tables.pop();
            }
            on
        }

        /// The text of a query, and whether its `ORDER BY` fixes the order of every row.
        fn select(&mut self) -> (String, bool) {
            self.has_failing_comparison = false;
            let mut order: Vec<Table> = TABLES.to_vec();
            for index in (1..order.len()).rev() {
                order.swap(index, self.rng.below(index + 1));
            }
            order.truncate(1 + self.rng.below(3));
            self.tables = vec![order[0]];
            let mut from = order[0].0.to_string();
            for right in order.into_iter().skip(1) {
                let kind =
                    self.rng
                        .pick(&["JOIN", "LEFT JOIN", "RIGHT JOIN", "FULL JOIN", "CROSS JOIN"]);
                from = format!("{from} {kind} {}", right.0);
                if kind != "CROSS JOIN" {
                    from = format!("{from} ON {}", self.on(right));
                }
                self.tables.push(right);
            }
            let mut sql = String::from("SELECT ");
            let mut group_by = None;
            let items: Vec<String> = match self.rng.chance(3) {
                true => {
                    let group = self.column(None).unwrap_or_else(|| "1".to_string());
                    let function = self.rng.pick(&["sum", "min", "max", "count"]);
                    let integer = self
                        .column(Some(Kind::Number))
                        .filter(|c| !c.ends_with(".f"));
                    let aggregate =
                        format!("{function}({})", integer.unwrap_or_else(|| self.number()));
                    group_by = Some(group.clone());
                    vec![group, "count(*)".to_string(), aggregate]
                }
                false => (0..1 + self.rng.below(3))
                    .map(|_| self.column(None).unwrap_or_else(|| "1".to_string()))
                    .collect(),
            };
            sql += &items.join(", ");
            sql += &format!(" FROM {from}");
            if self.rng.chance(2) {
                let conditions: Vec<String> = (0..1 + self.rng.below(2))
                    .map(|_| self.condition())
                    .collect();
                sql += &format!(" WHERE {}", conditions.join(" AND "));
            }
            if let Some(group) = group_by {
                sql += &format!(" GROUP BY {group}");
            }
            let is_ordered = self.rng.chance(2);
            if is_ordered {
                let keys: Vec<String> = (1..=items.len())
                    .map(|position| match self.rng.chance(2) {
                        true => format!("{position} DESC"),
                        false => position.to_string(),
                    })
                    .collect();
                sql += &format!(" ORDER BY {}", keys.join(", "));
                if self.rng.chance(2) {
                    sql += &format!(" LIMIT {} OFFSET {}", self.rng.below(6), self.rng.below(3));
                }
            }
            (sql, is_ordered)
        }
    }

    fn run_plan(db: &mut TestDb, plan: PhysicalPlan) -> Result<Vec<Vec<Value>>, &'static str> {
        let mut executor = plan.into_executor();
        execute(executor.as_mut(), &mut db.ctx())
            .map(|rows| rows.into_iter().map(|row| row.values).collect())
            .map_err(|err| err.sqlstate())
    }

    fn sort_rows(rows: &mut [Vec<Value>]) {
        rows.sort_by(|left, right| {
            left.iter()
                .zip(right)
                .map(|(left, right)| left.sort_cmp(right))
                .find(|order| order.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    #[test]
    fn random_queries_should_give_the_rows_of_the_plain_plan() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let mut db = random_db(&mut rng);
        let mut queries = QueryGen {
            rng,
            tables: Vec::new(),
            has_failing_comparison: false,
        };
        let mut used = std::collections::BTreeMap::<&str, usize>::new();

        for _ in 0..1_000 {
            let (sql, is_ordered) = queries.select();
            let logical = logical(&db, &sql);
            if push_down_filters(logical.clone()) != logical {
                *used.entry("pushdown").or_default() += 1;
            }
            let chosen = plan_physical_with(logical.clone(), &db.catalog, Rules::ALL).unwrap();
            let plain = plan_physical_with(logical, &db.catalog, Rules::NONE).unwrap();
            let text = chosen.to_string();
            for node in ["IndexNestedLoopJoin", "IndexScan", "TopN", "index d_code"] {
                if text.contains(node) {
                    *used.entry(node).or_default() += 1;
                }
            }
            if text.lines().any(|line| {
                line.contains("HashJoin") && !line.contains(": filter") && !line.contains("CROSS")
            }) {
                *used.entry("HashJoin keys").or_default() += 1;
            }

            let with_rules = run_plan(&mut db, chosen);
            let without = run_plan(&mut db, plain);

            match (with_rules, without) {
                (Ok(mut with_rules), Ok(mut without)) => {
                    if !is_ordered {
                        sort_rows(&mut with_rules);
                        sort_rows(&mut without);
                    }
                    assert_eq!(with_rules, without, "{sql}\n{text}");
                }
                (Err(with_rules), Err(without)) => assert_eq!(with_rules, without, "{sql}"),
                // An expression that fails on some rows fails in the plan that evaluates it on
                // them (documented); every other query must give the same outcome.
                (Ok(_), Err("42883")) | (Err("42883"), Ok(_)) if queries.has_failing_comparison => {
                }
                (with_rules, without) => {
                    panic!("{sql}\n{text}\nwith the rules {with_rules:?}\nwithout {without:?}")
                }
            }
        }
        // Every rule must have been chosen often enough for the comparison to mean something.
        for node in [
            "IndexNestedLoopJoin",
            "IndexScan",
            "TopN",
            "HashJoin keys",
            "index d_code",
            "pushdown",
        ] {
            assert!(used.get(node).copied().unwrap_or(0) >= 10, "{used:?}");
        }
    }

    /// The rows of `sql` with every rule and with none, or each one's SQLSTATE.
    fn with_and_without_rules(
        db: &mut TestDb,
        sql: &str,
    ) -> [Result<Vec<Vec<Value>>, &'static str>; 2] {
        let logical = logical(db, sql);
        let chosen = plan_physical_with(logical.clone(), &db.catalog, Rules::ALL).unwrap();
        let plain = plan_physical_with(logical, &db.catalog, Rules::NONE).unwrap();
        [run_plan(db, chosen), run_plan(db, plain)]
    }

    #[test]
    fn a_predicate_failing_on_rows_one_plan_skips_should_fail_only_in_the_other() {
        let mut db = db();

        // The index reads only the row of id 2; a scan evaluates `10 / 0` on the row of id 1.
        let by_index = with_and_without_rules(
            &mut db,
            "SELECT name FROM users WHERE id = 2 AND 10 / (id - 1) > 0",
        );
        // The hash join computes the keys of every right row though the left input is empty.
        let by_keys = with_and_without_rules(
            &mut db,
            "SELECT 1 FROM empty e JOIN orders o ON e.x = 10 / (o.total - 5)",
        );

        assert_eq!(by_index, [Ok(vec![vec![text("rui")]]), Err("22012")]);
        assert_eq!(by_keys, [Err("22012"), Ok(Vec::new())]);
    }

    #[test]
    fn queries_over_an_empty_table_should_give_no_rows_or_one_group() {
        let mut db = db();
        let orders = [10, 11, 12, 13];
        let padded_right: Vec<Vec<Value>> = orders
            .iter()
            .map(|id| vec![Value::Integer(*id), Value::Null])
            .collect();
        let padded_left: Vec<Vec<Value>> = orders
            .iter()
            .map(|id| vec![Value::Null, Value::Integer(*id)])
            .collect();
        let cases = [
            (
                "SELECT o.id, e.x FROM orders o LEFT JOIN empty e ON e.x = o.user_id ORDER BY 1",
                padded_right.clone(),
            ),
            (
                "SELECT o.id FROM orders o JOIN nobody n ON n.id = o.user_id",
                Vec::new(),
            ),
            (
                "SELECT o.id, n.id FROM orders o LEFT JOIN nobody n ON n.id = o.user_id \
                 ORDER BY 1",
                padded_right,
            ),
            (
                "SELECT e.x, o.id FROM empty e RIGHT JOIN orders o ON e.x = o.user_id ORDER BY 2",
                padded_left,
            ),
            (
                "SELECT count(*), max(x) FROM empty",
                vec![vec![Value::BigInt(0), Value::Null]],
            ),
            ("SELECT id FROM orders ORDER BY id LIMIT 0", Vec::new()),
            (
                "SELECT id FROM orders ORDER BY id LIMIT 2 OFFSET 10",
                Vec::new(),
            ),
        ];

        for (sql, expected) in cases {
            let outcomes = with_and_without_rules(&mut db, sql);

            assert_eq!(outcomes, [Ok(expected.clone()), Ok(expected)], "{sql}");
        }
        assert!(
            physical(
                &db,
                "SELECT o.id FROM orders o JOIN nobody n ON n.id = o.user_id"
            )
            .to_string()
            .contains("IndexNestedLoopJoin")
        );
    }

    #[test]
    fn an_index_on_text_or_timestamp_should_serve_a_literal_of_its_family() {
        let mut db = db();
        let cases = [
            (
                "SELECT code FROM events WHERE code = 'b'",
                vec![vec![text("b")]],
                "events_code",
            ),
            // Longer than the column: no key can equal it.
            (
                "SELECT code FROM events WHERE code = 'abcd'",
                Vec::new(),
                "events_code",
            ),
            // A DATE is the TIMESTAMP of its midnight.
            (
                "SELECT code FROM events WHERE at = DATE '2024-01-31'",
                vec![vec![text("a")]],
                "events_at",
            ),
        ];

        for (sql, expected, index) in cases {
            let line = plan_line(&db, sql, "IndexScan");
            let outcomes = with_and_without_rules(&mut db, sql);

            assert!(line.contains(&format!("index {index} =")), "{sql}: {line}");
            assert_eq!(outcomes, [Ok(expected.clone()), Ok(expected)], "{sql}");
        }
    }

    #[test]
    fn a_where_on_a_joined_table_should_reach_its_index() {
        let mut db = db();
        let sql = "SELECT o.id FROM orders o JOIN users u ON u.id = o.user_id WHERE u.id = 2";

        let plan = physical(&db, sql);
        let text = plan.to_string();
        let found = rows(&mut db, plan);

        assert!(
            text.contains("IndexScan: users AS u (index users_id = 2)"),
            "{text}"
        );
        // The inner table is filtered now: the join hashes its one row instead of looking the
        // index up once per order.
        assert!(text.contains("HashJoin INNER: o.user_id = u.id"), "{text}");
        assert_eq!(found, [vec![Value::Integer(12)]]);
    }

    #[test]
    fn an_equality_in_the_where_should_turn_a_cross_product_into_a_hash_join() {
        let mut db = db();
        let sql = "SELECT count(*) FROM users u, orders o WHERE o.user_id = u.id";

        let plan = physical(&db, sql);
        let text = plan.to_string();
        let found = rows(&mut db, plan);

        assert!(text.contains("HashJoin INNER: u.id = o.user_id"), "{text}");
        assert!(!text.contains("CROSS"), "{text}");
        assert_eq!(found, [vec![Value::BigInt(3)]]);
    }

    #[test]
    fn without_the_rules_the_where_should_stay_above_the_join() {
        let db = db();
        let sql = "SELECT count(*) FROM users u, orders o WHERE o.user_id = u.id";

        let plain = plan_physical_with(logical(&db, sql), &db.catalog, Rules::NONE).unwrap();

        assert_eq!(
            plan_nodes(&plain),
            [
                "Projection",
                "Aggregation",
                "Filter",
                "HashJoin",
                "SeqScan",
                "SeqScan"
            ]
        );
    }

    #[test]
    fn a_filter_pushed_onto_the_outer_table_should_keep_the_index_join() {
        let mut db = db();
        let sql = "SELECT o.id FROM orders o JOIN users u ON u.id = o.user_id WHERE o.total > 30";

        let plan = physical(&db, sql);
        let text = plan.to_string();
        let mut found = rows(&mut db, plan);
        sort_rows(&mut found);

        assert_eq!(
            text,
            "Projection: o.id AS id\n\
             \x20 IndexNestedLoopJoin INNER: u.id = o.user_id (index users_id ON users AS u)\n\
             \x20   Filter: o.total > 30\n\
             \x20     SeqScan: orders AS o"
        );
        assert_eq!(found, [vec![Value::Integer(10)], vec![Value::Integer(11)]]);
    }

    #[test]
    fn an_anti_join_should_give_the_rows_without_a_match() {
        let mut db = db();

        let outcomes = with_and_without_rules(
            &mut db,
            "SELECT u.id FROM users u LEFT JOIN orders o ON o.user_id = u.id WHERE o.id IS NULL",
        );

        assert_eq!(
            outcomes,
            [
                Ok(vec![vec![Value::Integer(3)]]),
                Ok(vec![vec![Value::Integer(3)]])
            ]
        );
    }

    #[test]
    fn a_filter_above_an_outer_join_should_keep_the_index_join_above_it() {
        let mut db = db();
        let sql = "SELECT t.label FROM orders o LEFT JOIN tags t ON t.user_id = o.user_id \
                   JOIN users u ON u.id = o.user_id WHERE t.label IS NULL";

        let text = physical(&db, sql).to_string();
        let outcomes = with_and_without_rules(&mut db, sql);

        // The WHERE stays above the LEFT join (it reads its padded side), and the index join
        // above still reads the types of its outer columns through that filter.
        assert_eq!(
            text,
            "Projection: t.label AS label\n\
             \x20 IndexNestedLoopJoin INNER: u.id = o.user_id (index users_id ON users AS u)\n\
             \x20   Filter: t.label IS NULL\n\
             \x20     HashJoin LEFT: o.user_id = t.user_id\n\
             \x20       SeqScan: orders AS o\n\
             \x20       SeqScan: tags AS t"
        );
        assert_eq!(
            outcomes,
            [Ok(vec![vec![Value::Null]]), Ok(vec![vec![Value::Null]])]
        );
    }

    #[test]
    fn a_select_without_from_should_keep_its_filter() {
        let mut db = db();

        let outcomes = with_and_without_rules(&mut db, "SELECT 1 WHERE FALSE");

        assert_eq!(outcomes, [Ok(Vec::new()), Ok(Vec::new())]);
    }
}
