//! The binder: an expression of the AST, with names and literal text, as an expression the
//! executors evaluate, with column positions and typed values.

use crate::execution::{BinaryOp, BooleanTest, Expr};
use crate::sql::ast;
use crate::table::Schema;
use crate::types::{DataType, Value};

use super::PlannerError;

/// The columns an expression may read: those of the statement's table, or none (a row of
/// `VALUES`).
pub(super) struct Scope<'a> {
    table: Option<(&'a str, &'a Schema)>,
}

impl<'a> Scope<'a> {
    pub(super) fn table(name: &'a str, schema: &'a Schema) -> Self {
        Self {
            table: Some((name, schema)),
        }
    }

    pub(super) fn empty() -> Self {
        Self { table: None }
    }

    /// The position of a column, checking its table qualifier against the statement's table.
    pub(super) fn column(&self, table: Option<&str>, name: &str) -> Result<usize, PlannerError> {
        let undefined = || PlannerError::UndefinedColumn {
            column: name.to_string(),
        };
        let Some((table_name, schema)) = self.table else {
            return Err(undefined());
        };
        if let Some(qualifier) = table.filter(|qualifier| *qualifier != table_name) {
            return Err(PlannerError::UndefinedTableReference {
                table: qualifier.to_string(),
            });
        }
        schema
            .columns()
            .iter()
            .position(|column| column.name == name)
            .ok_or_else(undefined)
    }
}

pub(super) fn bind(expr: &ast::Expr, scope: &Scope<'_>) -> Result<Expr, PlannerError> {
    let bind_box = |expr: &ast::Expr| bind(expr, scope).map(Box::new);
    Ok(match expr {
        ast::Expr::Column { table, name } => Expr::Column(scope.column(table.as_deref(), name)?),
        ast::Expr::Literal(literal) => Expr::Literal(literal_value(literal)?),
        ast::Expr::Unary { op, expr } => match (op, number_text(expr)) {
            // The sign joins the literal before its type is chosen, so the smallest BIGINT,
            // whose absolute value is not one, is written as itself.
            (ast::UnaryOp::Minus, Some(text)) => Expr::Literal(number(&format!("-{text}"))?),
            (ast::UnaryOp::Minus, None) => Expr::Negate(bind_box(expr)?),
            (ast::UnaryOp::Not, _) => Expr::Not(bind_box(expr)?),
            (ast::UnaryOp::Plus, _) => bind(expr, scope)?,
        },
        ast::Expr::Binary { op, left, right } => Expr::Binary {
            op: binary_op(*op),
            left: bind_box(left)?,
            right: bind_box(right)?,
        },
        ast::Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: bind_box(expr)?,
            negated: *negated,
        },
        ast::Expr::Is {
            expr,
            test,
            negated,
        } => Expr::Test {
            expr: bind_box(expr)?,
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
            left: bind_box(left)?,
            right: bind_box(right)?,
            negated: *negated,
        },
        ast::Expr::Between {
            expr,
            low,
            high,
            negated,
        } => negate_if(
            *negated,
            between(bind(expr, scope)?, bind(low, scope)?, bind(high, scope)?),
        ),
        ast::Expr::InList {
            expr,
            list,
            negated,
        } => {
            let expr = bind(expr, scope)?;
            let list = list
                .iter()
                .map(|item| bind(item, scope))
                .collect::<Result<Vec<_>, _>>()?;
            negate_if(*negated, in_list(expr, list))
        }
        ast::Expr::Cast { expr, to } => Expr::Cast {
            expr: bind_box(expr)?,
            to: *to,
        },
        ast::Expr::Function { name, .. } => return Err(function_error(name)),
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

/// No scalar functions exist yet; the aggregates belong to a `SELECT`'s groups, never to a
/// `WHERE`, a row of `VALUES` or a `SET`.
fn function_error(name: &str) -> PlannerError {
    match name {
        "count" | "sum" | "avg" | "min" | "max" => PlannerError::AggregateNotAllowed {
            function: name.to_string(),
        },
        _ => PlannerError::UndefinedFunction {
            name: name.to_string(),
        },
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
    use crate::table::Column;
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
        bind(&ast_expr(sql), &Scope::table("t", &schema)).unwrap()
    }

    fn bind_err(sql: &str) -> PlannerError {
        let schema = schema();
        bind(&ast_expr(sql), &Scope::table("t", &schema)).unwrap_err()
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
        let in_values = bind(&ast_expr("id"), &Scope::empty()).unwrap_err();

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
            matches!(&aggregate, PlannerError::AggregateNotAllowed { function } if function == "count")
        );
        assert_eq!(aggregate.sqlstate(), "42803");
        assert_eq!(summed.sqlstate(), "42803");
        assert!(matches!(&unknown, PlannerError::UndefinedFunction { name } if name == "f"));
        assert_eq!(unknown.sqlstate(), "42883");
    }
}
