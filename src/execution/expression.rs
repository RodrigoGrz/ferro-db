//! Expressions evaluated over one row. Columns are referred to by position (the planner will
//! resolve names); every operator is the `Value` operation of the same name, so `NULL`,
//! three-valued logic, type rules and SQLSTATEs are those of the types layer. Both sides of
//! `AND` and `OR` are always evaluated: the standard does not promise short-circuiting, and
//! `Value::and` type-checks both operands anyway.

use crate::types::{DataType, TypeError, Value};

use super::ExecutionError;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Column(usize),
    Literal(Value),
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Not(Box<Expr>),
    Negate(Box<Expr>),
    /// `IS NULL`, or `IS NOT NULL` when negated: never UNKNOWN.
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// `IS [NOT] TRUE / FALSE / UNKNOWN`: never UNKNOWN.
    Test {
        expr: Box<Expr>,
        test: BooleanTest,
        negated: bool,
    },
    /// `IS DISTINCT FROM`, or `IS NOT DISTINCT FROM` when negated: `NULL` equals `NULL`.
    IsDistinctFrom {
        left: Box<Expr>,
        right: Box<Expr>,
        negated: bool,
    },
    Cast {
        expr: Box<Expr>,
        to: DataType,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Concat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BooleanTest {
    True,
    False,
    Unknown,
}

impl Expr {
    pub fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
        Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    pub fn eval(&self, row: &[Value]) -> Result<Value, ExecutionError> {
        Ok(match self {
            Expr::Column(column) => {
                row.get(*column)
                    .cloned()
                    .ok_or(ExecutionError::ColumnOutOfRange {
                        column: *column,
                        len: row.len(),
                    })?
            }
            Expr::Literal(value) => value.clone(),
            Expr::Binary { op, left, right } => op.apply(&left.eval(row)?, &right.eval(row)?)?,
            Expr::Not(expr) => expr.eval(row)?.not()?,
            Expr::Negate(expr) => expr.eval(row)?.neg()?,
            Expr::IsNull { expr, negated } => Value::Boolean(expr.eval(row)?.is_null() != *negated),
            Expr::Test {
                expr,
                test,
                negated,
            } => {
                let value = expr.eval(row)?;
                let holds = match test {
                    BooleanTest::True => value.is_true()?,
                    BooleanTest::False => value.is_false()?,
                    BooleanTest::Unknown => value.is_unknown()?,
                };
                Value::Boolean(holds != *negated)
            }
            Expr::IsDistinctFrom {
                left,
                right,
                negated,
            } => {
                let is_distinct = left.eval(row)?.is_distinct_from(&right.eval(row)?)?;
                Value::Boolean(is_distinct != *negated)
            }
            Expr::Cast { expr, to } => expr.eval(row)?.cast(*to)?,
        })
    }
}

impl BinaryOp {
    fn apply(self, left: &Value, right: &Value) -> Result<Value, TypeError> {
        match self {
            BinaryOp::Eq => left.sql_eq(right),
            BinaryOp::Ne => left.sql_ne(right),
            BinaryOp::Lt => left.sql_lt(right),
            BinaryOp::Le => left.sql_le(right),
            BinaryOp::Gt => left.sql_gt(right),
            BinaryOp::Ge => left.sql_ge(right),
            BinaryOp::And => left.and(right),
            BinaryOp::Or => left.or(right),
            BinaryOp::Add => left.add(right),
            BinaryOp::Sub => left.sub(right),
            BinaryOp::Mul => left.mul(right),
            BinaryOp::Div => left.div(right),
            BinaryOp::Rem => left.rem(right),
            BinaryOp::Concat => left.concat(right),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use BinaryOp::*;

    fn col(column: usize) -> Expr {
        Expr::Column(column)
    }

    fn int(value: i32) -> Expr {
        Expr::Literal(Value::Integer(value))
    }

    fn null() -> Expr {
        Expr::Literal(Value::Null)
    }

    fn boolean(value: bool) -> Expr {
        Expr::Literal(Value::Boolean(value))
    }

    fn eval(expr: &Expr) -> Value {
        expr.eval(&[]).unwrap()
    }

    fn sqlstate(expr: &Expr) -> &'static str {
        expr.eval(&[]).unwrap_err().sqlstate()
    }

    #[test]
    fn columns_should_read_the_row_and_literals_should_be_themselves() {
        let row = [Value::Integer(7), Value::Text("ana".to_string())];

        assert_eq!(col(1).eval(&row).unwrap(), Value::Text("ana".to_string()));
        assert_eq!(int(3).eval(&row).unwrap(), Value::Integer(3));
    }

    #[test]
    fn a_column_outside_the_row_should_fail() {
        let err = col(2).eval(&[Value::Null, Value::Null]).unwrap_err();

        assert!(
            matches!(err, ExecutionError::ColumnOutOfRange { column: 2, len: 2 }),
            "{err:?}"
        );
        assert_eq!(err.sqlstate(), "XX000");
    }

    #[test]
    fn comparisons_should_be_unknown_with_null() {
        let cases = [
            (Eq, 1, 1, true),
            (Ne, 1, 2, true),
            (Lt, 1, 2, true),
            (Le, 2, 2, true),
            (Gt, 1, 2, false),
            (Ge, 1, 2, false),
        ];

        for (op, left, right, expected) in cases {
            assert_eq!(
                eval(&Expr::binary(op, int(left), int(right))),
                Value::Boolean(expected),
                "{op:?}"
            );
            assert_eq!(
                eval(&Expr::binary(op, int(left), null())),
                Value::Null,
                "{op:?}"
            );
            assert_eq!(
                eval(&Expr::binary(op, null(), int(right))),
                Value::Null,
                "{op:?}"
            );
        }
    }

    #[test]
    fn and_or_not_should_follow_kleene_logic() {
        let values = [boolean(true), boolean(false), null()];
        let truth = |expr: &Expr| match eval(expr) {
            Value::Boolean(value) => Some(value),
            _ => None,
        };

        for left in &values {
            for right in &values {
                let (l, r) = (truth(left), truth(right));
                let and = match (l, r) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                };
                let or = match (l, r) {
                    (Some(true), _) | (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                };

                assert_eq!(
                    truth(&Expr::binary(And, left.clone(), right.clone())),
                    and,
                    "{l:?} AND {r:?}"
                );
                assert_eq!(
                    truth(&Expr::binary(Or, left.clone(), right.clone())),
                    or,
                    "{l:?} OR {r:?}"
                );
            }
            assert_eq!(
                truth(&Expr::Not(Box::new(left.clone()))),
                truth(left).map(|value| !value)
            );
        }
    }

    #[test]
    fn logic_on_a_non_boolean_should_fail() {
        assert_eq!(
            sqlstate(&Expr::binary(And, boolean(false), int(1))),
            "42804"
        );
        assert_eq!(sqlstate(&Expr::binary(Or, int(1), boolean(true))), "42804");
        assert_eq!(sqlstate(&Expr::Not(Box::new(int(1)))), "42804");
    }

    #[test]
    fn arithmetic_should_compute_and_propagate_null() {
        let cases = [
            (Add, 7, 2, 9),
            (Sub, 7, 2, 5),
            (Mul, 7, 2, 14),
            (Div, 7, 2, 3),
            (Rem, 7, 2, 1),
        ];

        for (op, left, right, expected) in cases {
            assert_eq!(
                eval(&Expr::binary(op, int(left), int(right))),
                Value::Integer(expected),
                "{op:?}"
            );
            assert_eq!(
                eval(&Expr::binary(op, int(left), null())),
                Value::Null,
                "{op:?}"
            );
        }
        let widened = Expr::binary(
            Add,
            Expr::Literal(Value::SmallInt(1)),
            Expr::Literal(Value::BigInt(2)),
        );
        assert_eq!(eval(&widened), Value::BigInt(3));
        assert_eq!(eval(&Expr::Negate(Box::new(int(5)))), Value::Integer(-5));
    }

    #[test]
    fn arithmetic_errors_should_keep_their_sqlstate() {
        assert_eq!(sqlstate(&Expr::binary(Div, int(1), int(0))), "22012");
        assert_eq!(sqlstate(&Expr::binary(Add, int(i32::MAX), int(1))), "22003");
        assert_eq!(sqlstate(&Expr::Negate(Box::new(int(i32::MIN)))), "22003");
        let text = Expr::Literal(Value::Text("a".to_string()));
        assert_eq!(sqlstate(&Expr::binary(Lt, int(1), text.clone())), "42883");
        assert_eq!(sqlstate(&Expr::binary(Add, int(1), text)), "42883");
    }

    #[test]
    fn concat_should_join_text_and_propagate_null() {
        let text = |value: &str| Expr::Literal(Value::Text(value.to_string()));

        assert_eq!(
            eval(&Expr::binary(Concat, text("fer"), text("ro"))),
            Value::Text("ferro".to_string())
        );
        assert_eq!(eval(&Expr::binary(Concat, text("a"), null())), Value::Null);
    }

    #[test]
    fn is_null_should_never_be_unknown() {
        let is_null = |expr: Expr, negated: bool| Expr::IsNull {
            expr: Box::new(expr),
            negated,
        };

        assert_eq!(eval(&is_null(null(), false)), Value::Boolean(true));
        assert_eq!(eval(&is_null(int(1), false)), Value::Boolean(false));
        assert_eq!(eval(&is_null(null(), true)), Value::Boolean(false));
        assert_eq!(eval(&is_null(int(1), true)), Value::Boolean(true));
    }

    #[test]
    fn boolean_tests_should_never_be_unknown() {
        let tests = [BooleanTest::True, BooleanTest::False, BooleanTest::Unknown];
        let values = [(boolean(true), 0), (boolean(false), 1), (null(), 2)];

        for (value, holds_for) in values {
            for (position, test) in tests.iter().enumerate() {
                for negated in [false, true] {
                    let expr = Expr::Test {
                        expr: Box::new(value.clone()),
                        test: *test,
                        negated,
                    };

                    let expected = (position == holds_for) != negated;
                    assert_eq!(
                        eval(&expr),
                        Value::Boolean(expected),
                        "{value:?} {test:?} {negated}"
                    );
                }
            }
        }
        let on_integer = Expr::Test {
            expr: Box::new(int(1)),
            test: BooleanTest::True,
            negated: false,
        };
        assert_eq!(sqlstate(&on_integer), "42804");
    }

    #[test]
    fn is_distinct_from_should_treat_null_as_a_value() {
        let distinct = |left: Expr, right: Expr, negated: bool| Expr::IsDistinctFrom {
            left: Box::new(left),
            right: Box::new(right),
            negated,
        };

        assert_eq!(
            eval(&distinct(null(), null(), false)),
            Value::Boolean(false)
        );
        assert_eq!(eval(&distinct(int(1), null(), false)), Value::Boolean(true));
        assert_eq!(eval(&distinct(int(1), int(1), true)), Value::Boolean(true));
        assert_eq!(eval(&distinct(int(1), int(2), false)), Value::Boolean(true));
    }

    #[test]
    fn cast_should_convert_or_fail_with_its_sqlstate() {
        let cast = |text: &str| Expr::Cast {
            expr: Box::new(Expr::Literal(Value::Text(text.to_string()))),
            to: DataType::Integer,
        };

        assert_eq!(eval(&cast(" 42 ")), Value::Integer(42));
        assert_eq!(sqlstate(&cast("forty")), "22P02");
    }

    #[test]
    fn is_distinct_from_across_types_should_fail() {
        let expr = Expr::IsDistinctFrom {
            left: Box::new(int(1)),
            right: Box::new(Expr::Literal(Value::Text("1".to_string()))),
            negated: false,
        };

        assert_eq!(sqlstate(&expr), "42883");
    }

    #[test]
    fn cast_of_null_should_stay_null() {
        let expr = Expr::Cast {
            expr: Box::new(null()),
            to: DataType::Boolean,
        };

        assert_eq!(eval(&expr), Value::Null);
    }

    #[test]
    fn nested_expressions_should_combine() {
        // (c0 + 1) > 2 AND c1 IS NOT NULL
        let expr = Expr::binary(
            And,
            Expr::binary(Gt, Expr::binary(Add, col(0), int(1)), int(2)),
            Expr::IsNull {
                expr: Box::new(col(1)),
                negated: true,
            },
        );

        let holds = |row: &[Value]| expr.eval(row).unwrap();

        assert_eq!(
            holds(&[Value::Integer(2), Value::Integer(0)]),
            Value::Boolean(true)
        );
        assert_eq!(
            holds(&[Value::Integer(1), Value::Integer(0)]),
            Value::Boolean(false)
        );
        assert_eq!(
            holds(&[Value::Integer(2), Value::Null]),
            Value::Boolean(false)
        );
        assert_eq!(holds(&[Value::Null, Value::Integer(0)]), Value::Null);
    }
}
