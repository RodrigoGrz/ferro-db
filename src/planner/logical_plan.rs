//! The logical plan of a `SELECT`: what to compute, as a tree of relational operators over the
//! executors' expressions. Each node's expressions read the rows of its input by position, as
//! the executors will; `columns` names those positions, for `EXPLAIN` and the errors.

use crate::execution::{Aggregate, Expr, JoinType, SortKey};

use super::explain;

#[derive(Debug, Clone, PartialEq)]
pub enum LogicalPlan {
    /// The rows of a table; `alias` is the name its columns are qualified by, if not the table's.
    Scan {
        table: String,
        alias: Option<String>,
        columns: Vec<String>,
    },
    /// `SELECT` without `FROM`: one row without columns.
    OneRow,
    Filter {
        input: Box<LogicalPlan>,
        predicate: Expr,
    },
    /// The left columns followed by the right ones; `CROSS JOIN` is `Inner` without `on`.
    Join {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        join_type: JoinType,
        on: Option<Expr>,
    },
    /// The group's values, then one value per aggregate, as the `Aggregation` executor gives.
    Aggregate {
        input: Box<LogicalPlan>,
        group_by: Vec<Expr>,
        aggregates: Vec<Aggregate>,
    },
    Sort {
        input: Box<LogicalPlan>,
        keys: Vec<SortKey>,
    },
    /// The counts read no columns: they are computed once, before the first row.
    Limit {
        input: Box<LogicalPlan>,
        count: Option<Expr>,
        offset: Option<Expr>,
    },
    Project {
        input: Box<LogicalPlan>,
        columns: Vec<OutputColumn>,
    },
}

/// A column of the result: its value over the input row, and its name.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputColumn {
    pub expr: Expr,
    pub name: String,
}

/// How a column of a node's rows is shown: a table's column (`u.name`), or above an aggregate or
/// a projection, without a table, the text it stands for (`count(*)`, an output name), shown as
/// it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnName {
    pub table: Option<String>,
    pub name: String,
}

impl ColumnName {
    fn unqualified(name: String) -> Self {
        Self { table: None, name }
    }
}

/// The columns of a table, qualified by `alias` or else by the table's name.
pub(super) fn table_columns(
    table: &str,
    alias: Option<&str>,
    columns: &[String],
) -> Vec<ColumnName> {
    let table = alias.unwrap_or(table);
    columns
        .iter()
        .map(|name| ColumnName {
            table: Some(table.to_string()),
            name: name.clone(),
        })
        .collect()
}

/// The row of an aggregation over `input`: each group (a column keeps its name, an expression
/// is named by its text), then each aggregate by its call.
pub(super) fn aggregate_columns(
    input: &[ColumnName],
    group_by: &[Expr],
    aggregates: &[Aggregate],
) -> Vec<ColumnName> {
    let groups = group_by.iter().map(|expr| {
        let column = match expr {
            Expr::Column(position) => input.get(*position).cloned(),
            _ => None,
        };
        column.unwrap_or_else(|| ColumnName::unqualified(explain::operand_text(expr, input)))
    });
    let aggregates = aggregates
        .iter()
        .map(|aggregate| ColumnName::unqualified(explain::aggregate_text(aggregate, input)));
    groups.chain(aggregates).collect()
}

/// The names of the output columns of a projection.
pub(super) fn output_names(columns: &[OutputColumn]) -> Vec<ColumnName> {
    columns
        .iter()
        .map(|column| ColumnName::unqualified(column.name.clone()))
        .collect()
}

impl LogicalPlan {
    /// The names of the values of each row this node gives.
    pub fn columns(&self) -> Vec<ColumnName> {
        match self {
            LogicalPlan::Scan {
                table,
                alias,
                columns,
            } => table_columns(table, alias.as_deref(), columns),
            LogicalPlan::OneRow => Vec::new(),
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Limit { input, .. } => input.columns(),
            LogicalPlan::Join { left, right, .. } => {
                let mut columns = left.columns();
                columns.extend(right.columns());
                columns
            }
            LogicalPlan::Aggregate {
                input,
                group_by,
                aggregates,
            } => aggregate_columns(&input.columns(), group_by, aggregates),
            LogicalPlan::Project { columns, .. } => output_names(columns),
        }
    }

    /// The nodes this one reads, left before right.
    pub(super) fn inputs(&self) -> Vec<&LogicalPlan> {
        match self {
            LogicalPlan::Scan { .. } | LogicalPlan::OneRow => Vec::new(),
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Aggregate { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Limit { input, .. }
            | LogicalPlan::Project { input, .. } => vec![input],
            LogicalPlan::Join { left, right, .. } => vec![left, right],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::types::Value;

    fn scan(table: &str, alias: Option<&str>, columns: &[&str]) -> LogicalPlan {
        LogicalPlan::Scan {
            table: table.to_string(),
            alias: alias.map(str::to_string),
            columns: columns.iter().map(|name| name.to_string()).collect(),
        }
    }

    fn name(table: Option<&str>, name: &str) -> ColumnName {
        ColumnName {
            table: table.map(str::to_string),
            name: name.to_string(),
        }
    }

    #[test]
    fn a_scan_should_qualify_its_columns_by_alias_or_table() {
        let aliased = scan("users", Some("u"), &["id", "name"]);
        let plain = scan("users", None, &["id"]);

        assert_eq!(
            aliased.columns(),
            [name(Some("u"), "id"), name(Some("u"), "name")]
        );
        assert_eq!(plain.columns(), [name(Some("users"), "id")]);
        assert!(LogicalPlan::OneRow.columns().is_empty());
    }

    #[test]
    fn a_join_should_give_the_left_columns_then_the_right() {
        let join = LogicalPlan::Join {
            left: Box::new(scan("users", Some("u"), &["id"])),
            right: Box::new(scan("orders", Some("o"), &["id", "total"])),
            join_type: JoinType::Left,
            on: None,
        };
        let filtered = LogicalPlan::Filter {
            input: Box::new(join.clone()),
            predicate: Expr::Literal(Value::Boolean(true)),
        };

        let expected = [
            name(Some("u"), "id"),
            name(Some("o"), "id"),
            name(Some("o"), "total"),
        ];
        assert_eq!(join.columns(), expected);
        assert_eq!(filtered.columns(), expected);
    }

    #[test]
    fn an_aggregate_should_name_its_groups_and_aggregates() {
        let aggregate = LogicalPlan::Aggregate {
            input: Box::new(scan("orders", Some("o"), &["user_id", "total"])),
            group_by: vec![
                Expr::Column(0),
                Expr::binary(
                    crate::execution::BinaryOp::Add,
                    Expr::Column(1),
                    Expr::Literal(Value::Integer(1)),
                ),
            ],
            aggregates: vec![Aggregate::CountStar, Aggregate::Sum(Expr::Column(1))],
        };

        assert_eq!(
            aggregate.columns(),
            [
                name(Some("o"), "user_id"),
                name(None, "(o.total + 1)"),
                name(None, "count(*)"),
                name(None, "sum(o.total)"),
            ]
        );
    }

    #[test]
    fn a_projection_should_give_its_output_names() {
        let project = LogicalPlan::Project {
            input: Box::new(scan("users", None, &["id"])),
            columns: vec![
                OutputColumn {
                    expr: Expr::Column(0),
                    name: "id".to_string(),
                },
                OutputColumn {
                    expr: Expr::Literal(Value::Integer(1)),
                    name: "?column?".to_string(),
                },
            ],
        };
        let limited = LogicalPlan::Limit {
            input: Box::new(project.clone()),
            count: Some(Expr::Literal(Value::Integer(1))),
            offset: None,
        };

        assert_eq!(
            project.columns(),
            [name(None, "id"), name(None, "?column?")]
        );
        assert_eq!(limited.columns(), project.columns());
    }
}
