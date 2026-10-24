//! The binder: an expression of the AST, with names and literal text, as an expression the
//! executors evaluate, with column positions and typed values.
//!
//! Above the groups of a grouped query it binds in another mode (ISO/IEC 9075, 7.12): an
//! aggregate, or a part equal to an expression of `GROUP BY`, becomes a column of the grouped
//! row, and any other column is an error. The parts are compared already bound, so `a`, `t.a`
//! and `A` are the same group.

use crate::execution::{Aggregate, BinaryOp, BooleanTest, Expr};
use crate::sql::ast;
use crate::types::{DataType, Value};

use super::PlannerError;
use super::scope::Scope;

/// The groups of a grouped query and the aggregates read above them: the row of the
/// `Aggregate` node is the groups' values, then the aggregates'.
pub(super) struct Grouping {
    pub(super) group_by: Vec<Expr>,
    pub(super) aggregates: Vec<Aggregate>,
}

impl Grouping {
    fn group_column(&self, expr: &Expr) -> Option<Expr> {
        self.group_by
            .iter()
            .position(|group| group == expr)
            .map(Expr::Column)
    }

    /// The column of the aggregate, computed once however many times it is written.
    fn aggregate_column(&mut self, aggregate: Aggregate) -> Expr {
        let index = match self.aggregates.iter().position(|other| *other == aggregate) {
            Some(index) => index,
            None => {
                self.aggregates.push(aggregate);
                self.aggregates.len() - 1
            }
        };
        Expr::Column(self.group_by.len() + index)
    }
}

/// The expression over the columns of `scope`; `clause` names where it is, for the error of an
/// aggregate there.
pub(super) fn bind(
    expr: &ast::Expr,
    scope: &Scope<'_>,
    clause: &'static str,
) -> Result<Expr, PlannerError> {
    Binder {
        scope,
        mode: Mode::Plain(clause),
    }
    .bind(expr)
}

/// The expression over the grouped row, adding the aggregates it calls to `grouping`.
pub(super) fn bind_grouped(
    expr: &ast::Expr,
    scope: &Scope<'_>,
    grouping: &mut Grouping,
) -> Result<Expr, PlannerError> {
    Binder {
        scope,
        mode: Mode::Grouped(grouping),
    }
    .bind(expr)
}

/// The column of the input at `position` read above the groups, as `*` does: the group that is
/// that column, or an error.
pub(super) fn grouped_column(
    position: usize,
    scope: &Scope<'_>,
    grouping: &Grouping,
) -> Result<Expr, PlannerError> {
    grouping
        .group_column(&Expr::Column(position))
        .ok_or_else(|| PlannerError::GroupingError {
            column: scope
                .qualified_name(position)
                .unwrap_or_else(|| format!("#{position}")),
        })
}

/// Whether `expr` calls an aggregate anywhere.
pub(super) fn contains_aggregate(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::Function { name, .. } if is_aggregate(name) => true,
        _ => children(expr).into_iter().any(contains_aggregate),
    }
}

pub(super) fn contains_column(expr: &ast::Expr) -> bool {
    matches!(expr, ast::Expr::Column { .. }) || children(expr).into_iter().any(contains_column)
}

fn children(expr: &ast::Expr) -> Vec<&ast::Expr> {
    match expr {
        ast::Expr::Column { .. } | ast::Expr::Literal(_) => Vec::new(),
        ast::Expr::Unary { expr, .. }
        | ast::Expr::IsNull { expr, .. }
        | ast::Expr::Is { expr, .. }
        | ast::Expr::Cast { expr, .. } => vec![expr],
        ast::Expr::Binary { left, right, .. } | ast::Expr::IsDistinctFrom { left, right, .. } => {
            vec![left, right]
        }
        ast::Expr::Between {
            expr, low, high, ..
        } => vec![expr, low, high],
        ast::Expr::InList { expr, list, .. } => std::iter::once(&**expr).chain(list).collect(),
        ast::Expr::Function { args, .. } => match args {
            ast::FunctionArgs::Star => Vec::new(),
            ast::FunctionArgs::List(list) => list.iter().collect(),
        },
    }
}

fn is_aggregate(name: &str) -> bool {
    matches!(name, "count" | "sum" | "avg" | "min" | "max")
}

/// Where an expression is, which decides what an aggregate in it means.
enum Mode<'g> {
    /// A clause that refuses aggregates, by name.
    Plain(&'static str),
    /// The argument of an aggregate: another one there is nested.
    AggregateArgument,
    /// Above the groups of a grouped query.
    Grouped(&'g mut Grouping),
}

struct Binder<'s, 'a, 'g> {
    scope: &'s Scope<'a>,
    mode: Mode<'g>,
}

impl Binder<'_, '_, '_> {
    fn bind(&mut self, expr: &ast::Expr) -> Result<Expr, PlannerError> {
        if let Mode::Grouped(grouping) = &mut self.mode {
            if let ast::Expr::Function { name, args } = expr
                && is_aggregate(name)
            {
                let aggregate = aggregate(expr, name, args, self.scope)?;
                return Ok(grouping.aggregate_column(aggregate));
            }
            if !contains_aggregate(expr) {
                // Without an aggregate inside, the part binds over the input like any other.
                let bound = bind(expr, self.scope, "SELECT")?;
                if let Some(column) = grouping.group_column(&bound) {
                    return Ok(column);
                }
                if let Expr::Column(position) = bound {
                    return grouped_column(position, self.scope, grouping);
                }
                if !contains_column(expr) {
                    return Ok(bound);
                }
            }
        }
        self.bind_parts(expr)
    }

    fn boxed(&mut self, expr: &ast::Expr) -> Result<Box<Expr>, PlannerError> {
        self.bind(expr).map(Box::new)
    }

    /// The expression made of its parts, each bound in the same mode.
    fn bind_parts(&mut self, expr: &ast::Expr) -> Result<Expr, PlannerError> {
        Ok(match expr {
            ast::Expr::Column { table, name } => {
                Expr::Column(self.scope.column(table.as_deref(), name)?)
            }
            ast::Expr::Literal(literal) => Expr::Literal(literal_value(literal)?),
            ast::Expr::Unary { op, expr } => match (op, number_text(expr)) {
                // The sign joins the literal before its type is chosen, so the smallest BIGINT,
                // whose absolute value is not one, is written as itself.
                (ast::UnaryOp::Minus, Some(text)) => Expr::Literal(number(&format!("-{text}"))?),
                (ast::UnaryOp::Minus, None) => Expr::Negate(self.boxed(expr)?),
                (ast::UnaryOp::Not, _) => Expr::Not(self.boxed(expr)?),
                (ast::UnaryOp::Plus, _) => self.bind(expr)?,
            },
            ast::Expr::Binary { op, left, right } => Expr::Binary {
                op: binary_op(*op),
                left: self.boxed(left)?,
                right: self.boxed(right)?,
            },
            ast::Expr::IsNull { expr, negated } => Expr::IsNull {
                expr: self.boxed(expr)?,
                negated: *negated,
            },
            ast::Expr::Is {
                expr,
                test,
                negated,
            } => Expr::Test {
                expr: self.boxed(expr)?,
                test: match test {
                    ast::BooleanTest::True => BooleanTest::True,
                    ast::BooleanTest::False => BooleanTest::False,
                    ast::BooleanTest::Unknown => BooleanTest::Unknown,
                },
                negated: *negated,
            },
            ast::Expr::IsDistinctFrom {
                left,
                right,
                negated,
            } => Expr::IsDistinctFrom {
                left: self.boxed(left)?,
                right: self.boxed(right)?,
                negated: *negated,
            },
            ast::Expr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let between = between(self.bind(expr)?, self.bind(low)?, self.bind(high)?);
                negate_if(*negated, between)
            }
            ast::Expr::InList {
                expr,
                list,
                negated,
            } => {
                let expr = self.bind(expr)?;
                let list = list
                    .iter()
                    .map(|item| self.bind(item))
                    .collect::<Result<Vec<_>, _>>()?;
                negate_if(*negated, in_list(expr, list))
            }
            ast::Expr::Cast { expr, to } => Expr::Cast {
                expr: self.boxed(expr)?,
                to: *to,
            },
            ast::Expr::Function { name, .. } => return Err(self.function_error(name)),
        })
    }

    /// No scalar functions exist yet, and an aggregate reaches here only where it may not be.
    fn function_error(&self, name: &str) -> PlannerError {
        match &self.mode {
            Mode::Plain(clause) if is_aggregate(name) => {
                PlannerError::AggregateNotAllowed { clause }
            }
            Mode::AggregateArgument if is_aggregate(name) => PlannerError::NestedAggregate,
            _ => PlannerError::UndefinedFunction {
                name: name.to_string(),
            },
        }
    }
}

/// The aggregate `call` names: `count(*)`, or one of the aggregates of one argument.
fn aggregate(
    call: &ast::Expr,
    name: &str,
    args: &ast::FunctionArgs,
    scope: &Scope<'_>,
) -> Result<Aggregate, PlannerError> {
    let undefined = || PlannerError::UndefinedFunction {
        name: call.to_string(),
    };
    let [argument] = (match args {
        ast::FunctionArgs::Star if name == "count" => return Ok(Aggregate::CountStar),
        ast::FunctionArgs::Star => &[],
        ast::FunctionArgs::List(list) => list.as_slice(),
    }) else {
        return Err(undefined());
    };
    let argument = Binder {
        scope,
        mode: Mode::AggregateArgument,
    }
    .bind(argument)?;
    Ok(match name {
        "count" => Aggregate::Count(argument),
        "sum" => Aggregate::Sum(argument),
        "avg" => Aggregate::Avg(argument),
        "min" => Aggregate::Min(argument),
        "max" => Aggregate::Max(argument),
        _ => return Err(undefined()),
    })
}

/// `x BETWEEN low AND high` is `x >= low AND x <= high` (ISO/IEC 9075, 8.3).
fn between(expr: Expr, low: Expr, high: Expr) -> Expr {
    Expr::binary(
        BinaryOp::And,
        Expr::binary(BinaryOp::Ge, expr.clone(), low),
        Expr::binary(BinaryOp::Le, expr, high),
    )
}

/// `x IN (a, b, …)` is `x = a OR x = b OR …` (ISO/IEC 9075, 8.4), the same three-valued
/// result: TRUE if one is equal, else UNKNOWN if one comparison is, else FALSE.
fn in_list(expr: Expr, list: Vec<Expr>) -> Expr {
    let comparisons = list
        .into_iter()
        .map(|item| Expr::binary(BinaryOp::Eq, expr.clone(), item))
        .collect();
    any(comparisons)
}

/// The comparisons joined by `OR` as a balanced tree, so that a long list does not become a
/// tree as deep as itself (evaluating one recurses once per level). `OR` is associative and
/// the tree is evaluated left to right, so the result and the first error are a chain's.
fn any(mut comparisons: Vec<Expr>) -> Expr {
    if comparisons.len() <= 1 {
        return comparisons
            .pop()
            .unwrap_or(Expr::Literal(Value::Boolean(false)));
    }
    let right = comparisons.split_off(comparisons.len() / 2);
    Expr::binary(BinaryOp::Or, any(comparisons), any(right))
}

fn negate_if(negated: bool, expr: Expr) -> Expr {
    match negated {
        true => Expr::Not(Box::new(expr)),
        false => expr,
    }
}

fn literal_value(literal: &ast::Literal) -> Result<Value, PlannerError> {
    Ok(match literal {
        ast::Literal::Number(text) => number(text)?,
        ast::Literal::String(text) => Value::Text(text.clone()),
        ast::Literal::Boolean(value) => Value::Boolean(*value),
        ast::Literal::Null => Value::Null,
        ast::Literal::Date(text) => Value::parse(DataType::Date, text)?,
        ast::Literal::Timestamp(text) => Value::parse(DataType::Timestamp, text)?,
    })
}

fn number_text(expr: &ast::Expr) -> Option<&str> {
    match expr {
        ast::Expr::Literal(ast::Literal::Number(text)) => Some(text),
        _ => None,
    }
}

/// An integer is an INTEGER if it fits, else a BIGINT; one past BIGINT is out of range (there
/// is no NUMERIC). A fraction or an exponent makes a DOUBLE PRECISION.
fn number(text: &str) -> Result<Value, PlannerError> {
    if text.contains(['.', 'e', 'E']) {
        return Ok(Value::parse(DataType::Double, text)?);
    }
    Ok(match Value::parse(DataType::BigInt, text)? {
        Value::BigInt(number) => {
            i32::try_from(number).map_or(Value::BigInt(number), Value::Integer)
        }
        other => other,
    })
}

fn binary_op(op: ast::BinaryOp) -> BinaryOp {
    match op {
        ast::BinaryOp::Or => BinaryOp::Or,
        ast::BinaryOp::And => BinaryOp::And,
        ast::BinaryOp::Eq => BinaryOp::Eq,
        ast::BinaryOp::NotEq => BinaryOp::Ne,
        ast::BinaryOp::Lt => BinaryOp::Lt,
        ast::BinaryOp::LtEq => BinaryOp::Le,
        ast::BinaryOp::Gt => BinaryOp::Gt,
        ast::BinaryOp::GtEq => BinaryOp::Ge,
        ast::BinaryOp::Concat => BinaryOp::Concat,
        ast::BinaryOp::Plus => BinaryOp::Add,
        ast::BinaryOp::Minus => BinaryOp::Sub,
        ast::BinaryOp::Multiply => BinaryOp::Mul,
        ast::BinaryOp::Divide => BinaryOp::Div,
        ast::BinaryOp::Modulo => BinaryOp::Rem,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::sql::{SelectItem, Statement, parse};
    use crate::table::{Column, Schema};
    use crate::types::{Date, Finite64, Timestamp};

    /// `t (id INTEGER, v INTEGER, s VARCHAR)`.
    fn schema() -> Schema {
        Schema::new(vec![
            Column::new("id", DataType::Integer),
            Column::new("v", DataType::Integer),
            Column::new("s", DataType::VARCHAR_MAX),
        ])
        .unwrap()
    }

    fn ast_expr(sql: &str) -> ast::Expr {
        match parse(&format!("SELECT {sql}")).unwrap().as_slice() {
            [Statement::Select(select)] => match select.projection.as_slice() {
                [SelectItem::Expr { expr, .. }] => expr.clone(),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    fn bound(sql: &str) -> Expr {
        let schema = schema();
        bind(&ast_expr(sql), &Scope::table("t", &schema), "WHERE").unwrap()
    }

    fn bind_err(sql: &str) -> PlannerError {
        let schema = schema();
        bind(&ast_expr(sql), &Scope::table("t", &schema), "WHERE").unwrap_err()
    }

    fn literal(sql: &str) -> Value {
        match bound(sql) {
            Expr::Literal(value) => value,
            other => panic!("{other:?}"),
        }
    }

    /// The bound expression evaluated over a row `(id, v, s)`.
    fn eval(sql: &str, v: Value) -> Value {
        bound(sql)
            .eval(&[Value::Integer(1), v, Value::Null])
            .unwrap()
    }

    #[test]
    fn columns_should_become_positions_in_the_table() {
        assert_eq!(bound("id"), Expr::Column(0));
        assert_eq!(bound("t.v"), Expr::Column(1));
        assert_eq!(bound("S"), Expr::Column(2));
    }

    #[test]
    fn an_unknown_column_or_table_should_fail() {
        let unknown = bind_err("zz");
        let other_table = bind_err("u.v");
        let in_values = bind(&ast_expr("id"), &Scope::empty(), "VALUES").unwrap_err();

        assert!(matches!(&unknown, PlannerError::UndefinedColumn { column } if column == "zz"));
        assert_eq!(unknown.sqlstate(), "42703");
        assert!(
            matches!(&other_table, PlannerError::UndefinedTableReference { table } if table == "u")
        );
        assert_eq!(other_table.sqlstate(), "42P01");
        assert_eq!(in_values.sqlstate(), "42703");
    }

    #[test]
    fn integers_should_be_integer_or_bigint_by_their_value() {
        let cases = [
            ("42", Value::Integer(42)),
            ("007", Value::Integer(7)),
            ("2147483647", Value::Integer(i32::MAX)),
            ("2147483648", Value::BigInt(2_147_483_648)),
            ("-2147483648", Value::Integer(i32::MIN)),
            ("-2147483649", Value::BigInt(-2_147_483_649)),
            ("-9223372036854775808", Value::BigInt(i64::MIN)),
        ];

        for (sql, expected) in cases {
            assert_eq!(literal(sql), expected, "{sql}");
        }
    }

    #[test]
    fn an_integer_past_bigint_should_be_out_of_range() {
        for sql in ["9223372036854775808", "-9223372036854775809"] {
            assert_eq!(bind_err(sql).sqlstate(), "22003", "{sql}");
        }
    }

    #[test]
    fn decimals_and_exponents_should_be_double_precision() {
        let double = |value: f64| Value::Double(Finite64::new(value).unwrap());

        assert_eq!(literal("1.5"), double(1.5));
        assert_eq!(literal(".5"), double(0.5));
        assert_eq!(literal("1e3"), double(1000.0));
        assert_eq!(literal("-2.5E-1"), double(-0.25));
        assert_eq!(bind_err("1e400").sqlstate(), "22003");
    }

    #[test]
    fn other_literals_should_become_their_values() {
        let day = Date::from_ymd(2026, 10, 23).unwrap();

        assert_eq!(literal("'it''s'"), Value::Text("it's".to_string()));
        assert_eq!(literal("TRUE"), Value::Boolean(true));
        assert_eq!(literal("NULL"), Value::Null);
        assert_eq!(literal("DATE '2026-10-23'"), Value::Date(day));
        assert_eq!(
            literal("TIMESTAMP '2026-10-23 00:00:00'"),
            Value::Timestamp(Timestamp::at_midnight(day))
        );
        assert_eq!(bind_err("DATE '2026-02-30'").sqlstate(), "22008");
        assert_eq!(bind_err("DATE 'soon'").sqlstate(), "22007");
    }

    #[test]
    fn operators_should_become_the_executor_operators() {
        assert_eq!(
            bound("v % 2"),
            Expr::binary(
                BinaryOp::Rem,
                Expr::Column(1),
                Expr::Literal(Value::Integer(2))
            )
        );
        assert_eq!(bound("- v"), Expr::Negate(Box::new(Expr::Column(1))));
        assert_eq!(bound("+ v"), Expr::Column(1));
        assert_eq!(bound("NOT v"), Expr::Not(Box::new(Expr::Column(1))));
        assert_eq!(
            bound("CAST(v AS BIGINT)"),
            Expr::Cast {
                expr: Box::new(Expr::Column(1)),
                to: DataType::BigInt
            }
        );
    }

    #[test]
    fn tests_should_keep_their_three_valued_meaning() {
        assert_eq!(eval("v IS NULL", Value::Null), Value::Boolean(true));
        assert_eq!(eval("v IS NOT NULL", Value::Null), Value::Boolean(false));
        assert_eq!(eval("v = 1 IS UNKNOWN", Value::Null), Value::Boolean(true));
        assert_eq!(
            eval("v IS NOT DISTINCT FROM NULL", Value::Null),
            Value::Boolean(true)
        );
    }

    #[test]
    fn between_should_mean_greater_or_equal_and_less_or_equal() {
        let cases = [
            ("v BETWEEN 1 AND 3", Value::Integer(2), Value::Boolean(true)),
            ("v BETWEEN 1 AND 3", Value::Integer(1), Value::Boolean(true)),
            ("v BETWEEN 1 AND 3", Value::Integer(3), Value::Boolean(true)),
            (
                "v BETWEEN 1 AND 3",
                Value::Integer(5),
                Value::Boolean(false),
            ),
            ("v BETWEEN 1 AND 3", Value::Null, Value::Null),
            (
                "v BETWEEN 3 AND 1",
                Value::Integer(2),
                Value::Boolean(false),
            ),
            (
                "v NOT BETWEEN 1 AND 3",
                Value::Integer(5),
                Value::Boolean(true),
            ),
            (
                "v NOT BETWEEN 1 AND NULL",
                Value::Integer(0),
                Value::Boolean(true),
            ),
            ("v NOT BETWEEN 1 AND NULL", Value::Integer(2), Value::Null),
        ];

        for (sql, v, expected) in cases {
            assert_eq!(eval(sql, v.clone()), expected, "{sql} with {v:?}");
        }
    }

    #[test]
    fn in_should_mean_equal_to_any() {
        let cases = [
            ("v IN (1, 2)", Value::Integer(2), Value::Boolean(true)),
            ("v IN (1, 2)", Value::Integer(3), Value::Boolean(false)),
            ("v IN (1, NULL)", Value::Integer(1), Value::Boolean(true)),
            ("v IN (1, NULL)", Value::Integer(3), Value::Null),
            ("v IN (1)", Value::Null, Value::Null),
            ("v NOT IN (1, 2)", Value::Integer(3), Value::Boolean(true)),
            ("v NOT IN (1, NULL)", Value::Integer(3), Value::Null),
        ];

        for (sql, v, expected) in cases {
            assert_eq!(eval(sql, v.clone()), expected, "{sql} with {v:?}");
        }
    }

    /// How many nodes the longest path from `expr` to a leaf has.
    fn depth(expr: &Expr) -> usize {
        match expr {
            Expr::Binary { left, right, .. } => 1 + depth(left).max(depth(right)),
            Expr::Not(expr) => 1 + depth(expr),
            _ => 1,
        }
    }

    #[test]
    fn in_should_bind_a_long_list_as_a_shallow_tree() {
        let list = (0..1_000).map(|i| i.to_string()).collect::<Vec<_>>();
        let sql = format!("v IN ({})", list.join(", "));

        let expr = bound(&sql);

        // Ten levels of OR hold 1,024 comparisons, each a node over two leaves.
        assert_eq!(depth(&expr), 12);
        for (v, expected) in [(0, true), (500, true), (999, true), (1_000, false)] {
            assert_eq!(
                eval(&sql, Value::Integer(v)),
                Value::Boolean(expected),
                "{v}"
            );
        }
    }

    #[test]
    fn in_should_report_the_first_failing_item() {
        let expr = bound("v IN (1, 1 / 0, 'a', 2)");

        let err = expr
            .eval(&[Value::Integer(1), Value::Integer(5), Value::Null])
            .unwrap_err();

        assert_eq!(err.sqlstate(), "22012");
    }

    #[test]
    fn functions_should_be_refused() {
        let aggregate = bind_err("count(*)");
        let summed = bind_err("SUM(v)");
        let unknown = bind_err("f(1)");

        assert!(
            matches!(
                aggregate,
                PlannerError::AggregateNotAllowed { clause: "WHERE" }
            ),
            "{aggregate:?}"
        );
        assert_eq!(aggregate.sqlstate(), "42803");
        assert_eq!(summed.sqlstate(), "42803");
        assert!(matches!(&unknown, PlannerError::UndefinedFunction { name } if name == "f"));
        assert_eq!(unknown.sqlstate(), "42883");
    }
}
