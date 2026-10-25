//! The text of `EXPLAIN`: one node of a plan (logical or physical) per line, each input
//! indented two spaces under the node that reads it, the columns of the expressions by name
//! instead of position.

use std::fmt;

use crate::execution::{
    Aggregate, BinaryOp, BooleanTest, Expr, JoinType, NullOrder, SortKey, SortOrder,
};
use crate::sql::ast::{Literal, Name};
use crate::types::Value;

use super::logical_plan::{ColumnName, LogicalPlan, OutputColumn, table_columns};
use super::physical_plan::PhysicalPlan;

impl fmt::Display for LogicalPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_tree(f, self, 0, &logical_line, &LogicalPlan::inputs)
    }
}

impl fmt::Display for PhysicalPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_tree(f, self, 0, &physical_line, &PhysicalPlan::inputs)
    }
}

/// `node` on a line at `depth`, then its inputs one level deeper.
fn write_tree<T>(
    f: &mut fmt::Formatter<'_>,
    node: &T,
    depth: usize,
    line: &dyn Fn(&T) -> String,
    inputs: &dyn Fn(&T) -> Vec<&T>,
) -> fmt::Result {
    write!(f, "{:indent$}{}", "", line(node), indent = 2 * depth)?;
    for input in inputs(node) {
        writeln!(f)?;
        write_tree(f, input, depth + 1, line, inputs)?;
    }
    Ok(())
}

impl fmt::Display for ColumnName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.table {
            Some(table) => write!(f, "{}.{}", Name(table), Name(&self.name)),
            None => f.write_str(&self.name),
        }
    }
}

/// The expression with its columns named by `columns`, the names of the row it reads.
pub(super) fn expr_text(expr: &Expr, columns: &[ColumnName]) -> String {
    Shown::top(expr, columns).to_string()
}

/// The text of `expr` as part of another expression: in parentheses if it is compound. It names
/// a group's column above an aggregate, where it may be read inside other expressions.
pub(super) fn operand_text(expr: &Expr, columns: &[ColumnName]) -> String {
    Shown {
        expr,
        columns,
        is_nested: true,
    }
    .to_string()
}

pub(super) fn aggregate_text(aggregate: &Aggregate, columns: &[ColumnName]) -> String {
    let (function, argument) = match aggregate {
        Aggregate::CountStar => return "count(*)".to_string(),
        Aggregate::Count(expr) => ("count", expr),
        Aggregate::Sum(expr) => ("sum", expr),
        Aggregate::Avg(expr) => ("avg", expr),
        Aggregate::Min(expr) => ("min", expr),
        Aggregate::Max(expr) => ("max", expr),
    };
    format!("{function}({})", Shown::top(argument, columns))
}

fn logical_line(plan: &LogicalPlan) -> String {
    let input_columns = || match plan.inputs().as_slice() {
        [input] => input.columns(),
        _ => Vec::new(),
    };
    match plan {
        LogicalPlan::Scan { table, alias, .. } => format!("Scan: {}", table_text(table, alias)),
        LogicalPlan::OneRow => "OneRow".to_string(),
        LogicalPlan::Filter { predicate, .. } => {
            format!("Filter: {}", expr_text(predicate, &input_columns()))
        }
        LogicalPlan::Join {
            left,
            right,
            join_type,
            on,
        } => {
            let mut columns = left.columns();
            columns.extend(right.columns());
            let on: Vec<String> = on.iter().map(|on| expr_text(on, &columns)).collect();
            with_parts(
                format!("Join {}", join_type_text(*join_type, on.is_empty())),
                on,
            )
        }
        LogicalPlan::Aggregate {
            group_by,
            aggregates,
            ..
        } => format!(
            "Aggregate: {}",
            aggregation_text(group_by, aggregates, &input_columns())
        ),
        LogicalPlan::Sort { keys, .. } => {
            format!("Sort: {}", sort_keys_text(keys, &input_columns()))
        }
        LogicalPlan::Limit { count, offset, .. } => {
            format!("Limit: {}", row_count_text(count.as_ref(), offset.as_ref()))
        }
        LogicalPlan::Project { columns, .. } => {
            format!("Project: {}", outputs_text(columns, &input_columns()))
        }
    }
}

fn physical_line(plan: &PhysicalPlan) -> String {
    let input_columns = || match plan.inputs().as_slice() {
        [input] => input.columns(),
        _ => Vec::new(),
    };
    match plan {
        PhysicalPlan::SeqScan { table, alias, .. } => {
            format!("SeqScan: {}", table_text(table, alias))
        }
        PhysicalPlan::IndexScan {
            table,
            alias,
            index,
            key,
            ..
        } => format!(
            "IndexScan: {} (index {} = {})",
            table_text(table, alias),
            Name(index),
            expr_text(key, &[])
        ),
        PhysicalPlan::OneRow => "OneRow".to_string(),
        PhysicalPlan::Filter { predicate, .. } => {
            format!("Filter: {}", expr_text(predicate, &input_columns()))
        }
        PhysicalPlan::HashJoin {
            left,
            right,
            keys,
            join_type,
            predicate,
        } => {
            let (left, right) = (left.columns(), right.columns());
            let mut parts = Vec::new();
            if !keys.is_empty() {
                let keys: Vec<String> = keys
                    .iter()
                    .map(|(left_key, right_key)| {
                        format!(
                            "{} = {}",
                            expr_text(left_key, &left),
                            expr_text(right_key, &right)
                        )
                    })
                    .collect();
                parts.push(keys.join(", "));
            }
            if let Some(predicate) = predicate {
                let joined = [left, right].concat();
                parts.push(format!("filter: {}", expr_text(predicate, &joined)));
            }
            let is_cross = parts.is_empty();
            with_parts(
                format!("HashJoin {}", join_type_text(*join_type, is_cross)),
                parts,
            )
        }
        PhysicalPlan::IndexNestedLoopJoin {
            outer,
            outer_key,
            index,
            index_column,
            table,
            alias,
            columns,
            join_type,
            predicate,
        } => {
            let outer = outer.columns();
            let inner = table_columns(table, alias.as_deref(), columns);
            let inner_key = match inner.get(*index_column) {
                Some(column) => column.to_string(),
                None => format!("#{index_column}"),
            };
            let mut parts = vec![format!(
                "{inner_key} = {} (index {} ON {})",
                expr_text(outer_key, &outer),
                Name(index),
                table_text(table, alias)
            )];
            if let Some(predicate) = predicate {
                let joined = [outer, inner].concat();
                parts.push(format!("filter: {}", expr_text(predicate, &joined)));
            }
            with_parts(
                format!("IndexNestedLoopJoin {}", join_type_text(*join_type, false)),
                parts,
            )
        }
        PhysicalPlan::Aggregation {
            group_by,
            aggregates,
            ..
        } => format!(
            "Aggregation: {}",
            aggregation_text(group_by, aggregates, &input_columns())
        ),
        PhysicalPlan::Sort { keys, .. } => {
            format!("Sort: {}", sort_keys_text(keys, &input_columns()))
        }
        PhysicalPlan::TopN {
            keys,
            count,
            offset,
            ..
        } => format!(
            "TopN: {}; {}",
            sort_keys_text(keys, &input_columns()),
            row_count_text(Some(count), offset.as_ref())
        ),
        PhysicalPlan::Limit { count, offset, .. } => {
            format!("Limit: {}", row_count_text(count.as_ref(), offset.as_ref()))
        }
        PhysicalPlan::Projection { columns, .. } => {
            format!("Projection: {}", outputs_text(columns, &input_columns()))
        }
    }
}

/// `head`, then `: ` and the parts separated by `; `, if there are any.
fn with_parts(head: String, parts: Vec<String>) -> String {
    match parts.is_empty() {
        true => head,
        false => format!("{head}: {}", parts.join("; ")),
    }
}

fn table_text(table: &str, alias: &Option<String>) -> String {
    match alias {
        Some(alias) => format!("{} AS {}", Name(table), Name(alias)),
        None => Name(table).to_string(),
    }
}

/// An inner join with nothing to match on is a cross product.
fn join_type_text(join_type: JoinType, has_no_condition: bool) -> &'static str {
    match join_type {
        JoinType::Inner if has_no_condition => "CROSS",
        JoinType::Inner => "INNER",
        JoinType::Left => "LEFT",
        JoinType::Right => "RIGHT",
        JoinType::Full => "FULL",
    }
}

fn aggregation_text(group_by: &[Expr], aggregates: &[Aggregate], columns: &[ColumnName]) -> String {
    let mut parts = Vec::new();
    if !group_by.is_empty() {
        let groups: Vec<String> = group_by
            .iter()
            .map(|expr| expr_text(expr, columns))
            .collect();
        parts.push(format!("group by {}", groups.join(", ")));
    }
    if !aggregates.is_empty() {
        let aggregates: Vec<String> = aggregates
            .iter()
            .map(|aggregate| aggregate_text(aggregate, columns))
            .collect();
        parts.push(aggregates.join(", "));
    }
    match parts.is_empty() {
        true => "one group".to_string(),
        false => parts.join("; "),
    }
}

fn sort_keys_text(keys: &[SortKey], columns: &[ColumnName]) -> String {
    let keys: Vec<String> = keys.iter().map(|key| sort_key_text(key, columns)).collect();
    keys.join(", ")
}

/// The counts of a `LIMIT`: `5`, `5 OFFSET 2`, or `ALL OFFSET 2` without a count.
fn row_count_text(count: Option<&Expr>, offset: Option<&Expr>) -> String {
    let count = match count {
        Some(count) => expr_text(count, &[]),
        None => "ALL".to_string(),
    };
    match offset {
        Some(offset) => format!("{count} OFFSET {}", expr_text(offset, &[])),
        None => count,
    }
}

fn outputs_text(columns: &[OutputColumn], input: &[ColumnName]) -> String {
    let columns: Vec<String> = columns
        .iter()
        .map(|column| {
            format!(
                "{} AS {}",
                expr_text(&column.expr, input),
                Name(&column.name)
            )
        })
        .collect();
    columns.join(", ")
}

fn sort_key_text(key: &SortKey, columns: &[ColumnName]) -> String {
    let order = match key.order {
        SortOrder::Asc => "ASC",
        SortOrder::Desc => "DESC",
    };
    let nulls = match key.nulls {
        NullOrder::First => "FIRST",
        NullOrder::Last => "LAST",
    };
    format!("{} {order} NULLS {nulls}", Shown::top(&key.expr, columns))
}

/// An expression with its columns by name; a compound one inside another is in parentheses,
/// so the text never depends on the precedence of operators.
struct Shown<'a> {
    expr: &'a Expr,
    columns: &'a [ColumnName],
    is_nested: bool,
}

impl<'a> Shown<'a> {
    fn top(expr: &'a Expr, columns: &'a [ColumnName]) -> Self {
        Self {
            expr,
            columns,
            is_nested: false,
        }
    }

    fn inner(&self, expr: &'a Expr) -> Self {
        Self {
            expr,
            columns: self.columns,
            is_nested: true,
        }
    }
}

impl fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A negative number is one too: `-` before `-5` would start a comment.
        let is_compound = match self.expr {
            Expr::Column(_) | Expr::Cast { .. } => false,
            Expr::Literal(value) => value.to_string().starts_with('-'),
            _ => true,
        };
        let parenthesized = self.is_nested && is_compound;
        if parenthesized {
            f.write_str("(")?;
        }
        match self.expr {
            Expr::Column(position) => match self.columns.get(*position) {
                Some(column) => write!(f, "{column}")?,
                None => write!(f, "#{position}")?,
            },
            Expr::Literal(value) => write!(f, "{}", literal(value))?,
            Expr::Binary { op, left, right } => write!(
                f,
                "{} {} {}",
                self.inner(left),
                symbol(*op),
                self.inner(right)
            )?,
            Expr::Not(expr) => write!(f, "NOT {}", self.inner(expr))?,
            Expr::Negate(expr) => write!(f, "-{}", self.inner(expr))?,
            Expr::IsNull { expr, negated } => {
                write!(f, "{} IS {}NULL", self.inner(expr), not(*negated))?;
            }
            Expr::Test {
                expr,
                test,
                negated,
            } => {
                let test = match test {
                    BooleanTest::True => "TRUE",
                    BooleanTest::False => "FALSE",
                    BooleanTest::Unknown => "UNKNOWN",
                };
                write!(f, "{} IS {}{test}", self.inner(expr), not(*negated))?;
            }
            Expr::IsDistinctFrom {
                left,
                right,
                negated,
            } => write!(
                f,
                "{} IS {}DISTINCT FROM {}",
                self.inner(left),
                not(*negated),
                self.inner(right)
            )?,
            Expr::Cast { expr, to } => {
                write!(f, "CAST({} AS {to})", Shown::top(expr, self.columns))?;
            }
        }
        if parenthesized {
            f.write_str(")")?;
        }
        Ok(())
    }
}

fn not(negated: bool) -> &'static str {
    match negated {
        true => "NOT ",
        false => "",
    }
}

/// The value as the literal that reads back as it: text quoted, dates and timestamps typed.
fn literal(value: &Value) -> Literal {
    match value {
        Value::Null => Literal::Null,
        Value::Boolean(value) => Literal::Boolean(*value),
        Value::Text(text) => Literal::String(text.clone()),
        Value::Date(_) => Literal::Date(value.to_string()),
        Value::Timestamp(_) => Literal::Timestamp(value.to_string()),
        Value::SmallInt(_)
        | Value::Integer(_)
        | Value::BigInt(_)
        | Value::Real(_)
        | Value::Double(_) => Literal::Number(value.to_string()),
    }
}

fn symbol(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Eq => "=",
        BinaryOp::Ne => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::Le => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::Ge => ">=",
        BinaryOp::And => "AND",
        BinaryOp::Or => "OR",
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Rem => "%",
        BinaryOp::Concat => "||",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::execution::{BinaryOp, BooleanTest, JoinType, NullOrder, SortKey, SortOrder};
    use crate::planner::OutputColumn;
    use crate::types::{DataType, Value};

    fn scan(table: &str, alias: Option<&str>, columns: &[&str]) -> LogicalPlan {
        LogicalPlan::Scan {
            table: table.to_string(),
            alias: alias.map(str::to_string),
            columns: columns.iter().map(|name| name.to_string()).collect(),
        }
    }

    fn column(table: &str, name: &str) -> ColumnName {
        ColumnName {
            table: Some(table.to_string()),
            name: name.to_string(),
        }
    }

    fn int(value: i32) -> Expr {
        Expr::Literal(Value::Integer(value))
    }

    fn output(expr: Expr, name: &str) -> OutputColumn {
        OutputColumn {
            expr,
            name: name.to_string(),
        }
    }

    #[test]
    fn explain_should_show_every_node_indented_under_its_reader() {
        // users u (id, name) JOIN orders o (id, user_id, total)
        let join = LogicalPlan::Join {
            left: Box::new(scan("users", Some("u"), &["id", "name"])),
            right: Box::new(scan("orders", Some("o"), &["id", "user_id", "total"])),
            join_type: JoinType::Inner,
            on: Some(Expr::binary(BinaryOp::Eq, Expr::Column(3), Expr::Column(0))),
        };
        let filter = LogicalPlan::Filter {
            input: Box::new(join),
            predicate: Expr::binary(BinaryOp::Gt, Expr::Column(4), int(10)),
        };
        let aggregate = LogicalPlan::Aggregate {
            input: Box::new(filter),
            group_by: vec![Expr::Column(1)],
            aggregates: vec![Aggregate::CountStar],
        };
        let sort = LogicalPlan::Sort {
            input: Box::new(aggregate),
            keys: vec![SortKey {
                expr: Expr::Column(1),
                order: SortOrder::Desc,
                nulls: NullOrder::First,
            }],
        };
        let limit = LogicalPlan::Limit {
            input: Box::new(sort),
            count: Some(int(5)),
            offset: None,
        };
        let project = LogicalPlan::Project {
            input: Box::new(limit),
            columns: vec![
                output(Expr::Column(0), "name"),
                output(Expr::Column(1), "count"),
            ],
        };

        let text = project.to_string();

        assert_eq!(
            text,
            "Project: u.name AS name, count(*) AS count\n\
             \x20 Limit: 5\n\
             \x20   Sort: count(*) DESC NULLS FIRST\n\
             \x20     Aggregate: group by u.name; count(*)\n\
             \x20       Filter: o.total > 10\n\
             \x20         Join INNER: o.user_id = u.id\n\
             \x20           Scan: users AS u\n\
             \x20           Scan: orders AS o"
        );
    }

    #[test]
    fn explain_should_show_one_row_and_names_that_need_quotes() {
        let project = LogicalPlan::Project {
            input: Box::new(LogicalPlan::OneRow),
            columns: vec![output(int(1), "?column?"), output(int(2), "Total")],
        };
        let scan = scan("Users", None, &["select"]);
        let filter = LogicalPlan::Filter {
            input: Box::new(scan),
            predicate: Expr::IsNull {
                expr: Box::new(Expr::Column(0)),
                negated: false,
            },
        };

        assert_eq!(
            project.to_string(),
            "Project: 1 AS \"?column?\", 2 AS \"Total\"\n  OneRow"
        );
        assert_eq!(
            filter.to_string(),
            "Filter: \"Users\".\"select\" IS NULL\n  Scan: \"Users\""
        );
    }

    #[test]
    fn literals_should_be_written_as_sql() {
        let cases = [
            (Value::Null, "NULL"),
            (Value::Boolean(true), "TRUE"),
            (Value::Integer(-5), "-5"),
            (Value::BigInt(i64::MAX), "9223372036854775807"),
            (Value::Text("it's".to_string()), "'it''s'"),
            (Value::parse(DataType::Double, "1.5").unwrap(), "1.5"),
            (
                Value::parse(DataType::Date, "2024-01-31").unwrap(),
                "DATE '2024-01-31'",
            ),
            (
                Value::parse(DataType::Timestamp, "2024-01-31 10:00:00").unwrap(),
                "TIMESTAMP '2024-01-31 10:00:00'",
            ),
        ];

        for (value, expected) in cases {
            assert_eq!(expr_text(&Expr::Literal(value), &[]), expected);
        }
    }

    #[test]
    fn compound_expressions_inside_others_should_be_in_parentheses() {
        let columns = [column("t", "a"), column("t", "b")];
        let a = || Box::new(Expr::Column(0));
        let cases = [
            (
                Expr::binary(
                    BinaryOp::Mul,
                    Expr::binary(BinaryOp::Add, Expr::Column(0), int(1)),
                    int(2),
                ),
                "(t.a + 1) * 2",
            ),
            (
                Expr::binary(
                    BinaryOp::And,
                    Expr::binary(BinaryOp::Ge, Expr::Column(0), int(1)),
                    Expr::binary(BinaryOp::Le, Expr::Column(0), int(2)),
                ),
                "(t.a >= 1) AND (t.a <= 2)",
            ),
            (Expr::Not(a()), "NOT t.a"),
            (Expr::Negate(a()), "-t.a"),
            (Expr::Negate(Box::new(Expr::Negate(a()))), "-(-t.a)"),
            (Expr::Negate(Box::new(int(-5))), "-(-5)"),
            (
                Expr::binary(BinaryOp::Sub, Expr::Column(0), int(-5)),
                "t.a - (-5)",
            ),
            (
                Expr::IsNull {
                    expr: a(),
                    negated: true,
                },
                "t.a IS NOT NULL",
            ),
            (
                Expr::Test {
                    expr: a(),
                    test: BooleanTest::Unknown,
                    negated: false,
                },
                "t.a IS UNKNOWN",
            ),
            (
                Expr::IsDistinctFrom {
                    left: a(),
                    right: Box::new(Expr::Column(1)),
                    negated: true,
                },
                "t.a IS NOT DISTINCT FROM t.b",
            ),
            (
                Expr::Cast {
                    expr: Box::new(Expr::binary(
                        BinaryOp::Concat,
                        Expr::Column(0),
                        Expr::Column(1),
                    )),
                    to: DataType::BigInt,
                },
                "CAST(t.a || t.b AS BIGINT)",
            ),
            (
                Expr::binary(
                    BinaryOp::Ne,
                    Expr::binary(BinaryOp::Rem, Expr::Column(0), int(2)),
                    Expr::binary(BinaryOp::Sub, Expr::Column(1), int(1)),
                ),
                "(t.a % 2) <> (t.b - 1)",
            ),
            // A position past the row, which only a plan built wrong has.
            (Expr::Column(7), "#7"),
        ];

        for (expr, expected) in cases {
            assert_eq!(expr_text(&expr, &columns), expected);
        }
    }

    #[test]
    fn aggregates_should_be_written_as_calls() {
        let columns = [column("t", "a")];
        let cases = [
            (Aggregate::CountStar, "count(*)"),
            (Aggregate::Count(Expr::Column(0)), "count(t.a)"),
            (Aggregate::Sum(Expr::Column(0)), "sum(t.a)"),
            (Aggregate::Avg(Expr::Column(0)), "avg(t.a)"),
            (Aggregate::Min(Expr::Column(0)), "min(t.a)"),
            (
                Aggregate::Max(Expr::binary(BinaryOp::Div, Expr::Column(0), int(2))),
                "max(t.a / 2)",
            ),
        ];

        for (aggregate, expected) in cases {
            assert_eq!(aggregate_text(&aggregate, &columns), expected);
        }
    }

    #[test]
    fn joins_should_show_their_type_and_condition() {
        let join = |join_type, on| LogicalPlan::Join {
            left: Box::new(scan("a", None, &["x"])),
            right: Box::new(scan("b", None, &["y"])),
            join_type,
            on,
        };
        let on = || Some(Expr::binary(BinaryOp::Eq, Expr::Column(0), Expr::Column(1)));
        let first_line = |plan: LogicalPlan| plan.to_string().lines().next().unwrap().to_string();

        assert_eq!(first_line(join(JoinType::Inner, None)), "Join CROSS");
        assert_eq!(
            first_line(join(JoinType::Left, on())),
            "Join LEFT: a.x = b.y"
        );
        assert_eq!(
            first_line(join(JoinType::Right, on())),
            "Join RIGHT: a.x = b.y"
        );
        assert_eq!(
            first_line(join(JoinType::Full, on())),
            "Join FULL: a.x = b.y"
        );
    }

    #[test]
    fn limits_and_aggregates_should_show_only_what_they_have() {
        let limit = |count, offset| LogicalPlan::Limit {
            input: Box::new(LogicalPlan::OneRow),
            count,
            offset,
        };
        let aggregate = |group_by, aggregates| LogicalPlan::Aggregate {
            input: Box::new(scan("t", None, &["a"])),
            group_by,
            aggregates,
        };
        let first_line = |plan: LogicalPlan| plan.to_string().lines().next().unwrap().to_string();

        assert_eq!(
            first_line(limit(Some(int(5)), Some(int(2)))),
            "Limit: 5 OFFSET 2"
        );
        assert_eq!(first_line(limit(None, Some(int(2)))), "Limit: ALL OFFSET 2");
        assert_eq!(
            first_line(aggregate(vec![Expr::Column(0)], vec![])),
            "Aggregate: group by t.a"
        );
        assert_eq!(
            first_line(aggregate(vec![], vec![Aggregate::CountStar])),
            "Aggregate: count(*)"
        );
        assert_eq!(
            first_line(aggregate(vec![], vec![])),
            "Aggregate: one group"
        );
        let sort = LogicalPlan::Sort {
            input: Box::new(scan("t", None, &["a"])),
            keys: vec![SortKey::asc(Expr::Column(0)), SortKey::asc(int(1))],
        };
        assert_eq!(
            first_line(sort),
            "Sort: t.a ASC NULLS LAST, 1 ASC NULLS LAST"
        );
    }
}
