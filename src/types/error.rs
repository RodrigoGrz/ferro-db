use std::fmt;

use super::DataType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeError {
    OutOfRange {
        ty: DataType,
    },
    TextOutOfRange {
        ty: DataType,
        input: String,
    },
    InvalidTextRepresentation {
        ty: DataType,
        input: String,
    },
    DivisionByZero,
    /// A stored field whose width does not match its column type (corruption or wrong schema).
    InvalidEncoding {
        ty: DataType,
        expected: usize,
        actual: usize,
    },
    /// Text longer than the `n` of its VARCHAR(n).
    StringTooLong {
        ty: DataType,
        chars: usize,
    },
    /// An operator applied to types it does not accept, such as text + integer. `left` is
    /// `None` for a prefix operator (`-text`).
    UndefinedOperator {
        op: &'static str,
        left: Option<DataType>,
        right: DataType,
    },
    /// A value written to a column of another type family (store assignment).
    DatatypeMismatch {
        value: DataType,
        target: DataType,
    },
    /// A logical operator or boolean test (`AND`, `NOT`, `IS TRUE`...) given a non-boolean.
    NotBoolean {
        op: &'static str,
        actual: DataType,
    },
    /// A stored field of the right width whose bytes are not a value of its type: text that
    /// is not valid UTF-8 or is longer than its column, a boolean byte other than 0 or 1.
    CorruptedValue {
        ty: DataType,
        reason: &'static str,
    },
}

impl TypeError {
    pub fn sqlstate(&self) -> &'static str {
        match self {
            TypeError::OutOfRange { .. } | TypeError::TextOutOfRange { .. } => "22003",
            TypeError::InvalidTextRepresentation { .. } => "22P02",
            TypeError::DivisionByZero => "22012",
            TypeError::StringTooLong { .. } => "22001",
            TypeError::UndefinedOperator { .. } => "42883",
            TypeError::DatatypeMismatch { .. } | TypeError::NotBoolean { .. } => "42804",
            TypeError::InvalidEncoding { .. } | TypeError::CorruptedValue { .. } => "XX001",
        }
    }
}

/// Error messages spell type names in lowercase.
fn type_name(ty: DataType) -> String {
    ty.name().to_ascii_lowercase()
}

/// Like `type_name`, plus the length where the type has one: `varchar(5)`.
fn full_type_name(ty: DataType) -> String {
    ty.to_string().to_ascii_lowercase()
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeError::OutOfRange { ty } => write!(f, "{} out of range", type_name(*ty)),
            TypeError::TextOutOfRange { ty, input } => write!(
                f,
                "value \"{input}\" is out of range for type {}",
                type_name(*ty)
            ),
            TypeError::InvalidTextRepresentation { ty, input } => write!(
                f,
                "invalid input syntax for type {}: \"{input}\"",
                type_name(*ty)
            ),
            TypeError::DivisionByZero => write!(f, "division by zero"),
            TypeError::InvalidEncoding {
                ty,
                expected,
                actual,
            } => write!(
                f,
                "{} field must be {expected} bytes, got {actual}",
                type_name(*ty)
            ),
            TypeError::StringTooLong { ty, .. } => {
                write!(f, "value too long for type {}", full_type_name(*ty))
            }
            TypeError::UndefinedOperator { op, left, right } => {
                write!(f, "operator does not exist: ")?;
                if let Some(left) = left {
                    write!(f, "{} ", type_name(*left))?;
                }
                write!(f, "{op} {}", type_name(*right))
            }
            TypeError::DatatypeMismatch { value, target } => write!(
                f,
                "cannot assign a value of type {} to a column of type {}",
                type_name(*value),
                full_type_name(*target)
            ),
            TypeError::NotBoolean { op, actual } => write!(
                f,
                "argument of {op} must be type boolean, not type {}",
                type_name(*actual)
            ),
            TypeError::CorruptedValue { ty, reason } => write!(
                f,
                "stored {} value is corrupted: {reason}",
                full_type_name(*ty)
            ),
        }
    }
}

impl std::error::Error for TypeError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_error(ty: DataType, input: &str) -> TypeError {
        TypeError::InvalidTextRepresentation {
            ty,
            input: input.to_string(),
        }
    }

    #[test]
    fn errors_should_describe_the_problem() {
        let cases = [
            (
                TypeError::OutOfRange {
                    ty: DataType::SmallInt,
                },
                "smallint out of range",
            ),
            (
                TypeError::OutOfRange {
                    ty: DataType::Boolean,
                },
                "boolean out of range",
            ),
            (
                text_error(DataType::Boolean, "yes"),
                "invalid input syntax for type boolean: \"yes\"",
            ),
            (
                TypeError::CorruptedValue {
                    ty: DataType::Boolean,
                    reason: "boolean byte must be 0 or 1",
                },
                "stored boolean value is corrupted: boolean byte must be 0 or 1",
            ),
            (
                TypeError::TextOutOfRange {
                    ty: DataType::Integer,
                    input: "99999999999".to_string(),
                },
                "value \"99999999999\" is out of range for type integer",
            ),
            (
                text_error(DataType::BigInt, "abc"),
                "invalid input syntax for type bigint: \"abc\"",
            ),
            (TypeError::DivisionByZero, "division by zero"),
            (
                TypeError::InvalidEncoding {
                    ty: DataType::Integer,
                    expected: 4,
                    actual: 2,
                },
                "integer field must be 4 bytes, got 2",
            ),
        ];

        for (err, message) in cases {
            assert_eq!(err.to_string(), message);
        }
    }

    #[test]
    fn errors_should_map_to_sql_standard_sqlstates() {
        let cases = [
            (
                TypeError::OutOfRange {
                    ty: DataType::BigInt,
                },
                "22003",
            ),
            (
                TypeError::TextOutOfRange {
                    ty: DataType::BigInt,
                    input: String::new(),
                },
                "22003",
            ),
            (text_error(DataType::Integer, ""), "22P02"),
            (TypeError::DivisionByZero, "22012"),
            (
                TypeError::InvalidEncoding {
                    ty: DataType::SmallInt,
                    expected: 2,
                    actual: 0,
                },
                "XX001",
            ),
        ];

        for (err, sqlstate) in cases {
            assert_eq!(err.sqlstate(), sqlstate, "{err:?}");
        }
    }

    fn varchar(max_chars: u16) -> DataType {
        DataType::Varchar(std::num::NonZeroU16::new(max_chars).unwrap())
    }

    fn text_errors() -> Vec<(TypeError, &'static str, &'static str)> {
        vec![
            (
                TypeError::StringTooLong {
                    ty: varchar(5),
                    chars: 7,
                },
                "value too long for type varchar(5)",
                "22001",
            ),
            (
                TypeError::UndefinedOperator {
                    op: "+",
                    left: Some(DataType::VARCHAR_MAX),
                    right: DataType::Integer,
                },
                "operator does not exist: varchar + integer",
                "42883",
            ),
            (
                TypeError::UndefinedOperator {
                    op: "-",
                    left: None,
                    right: varchar(8),
                },
                "operator does not exist: - varchar",
                "42883",
            ),
            (
                TypeError::DatatypeMismatch {
                    value: DataType::VARCHAR_MAX,
                    target: DataType::Integer,
                },
                "cannot assign a value of type varchar to a column of type integer",
                "42804",
            ),
            (
                TypeError::DatatypeMismatch {
                    value: DataType::BigInt,
                    target: varchar(10),
                },
                "cannot assign a value of type bigint to a column of type varchar(10)",
                "42804",
            ),
            (
                TypeError::CorruptedValue {
                    ty: varchar(3),
                    reason: "text is not valid UTF-8",
                },
                "stored varchar(3) value is corrupted: text is not valid UTF-8",
                "XX001",
            ),
        ]
    }

    #[test]
    fn not_boolean_should_name_the_operator_and_the_type() {
        let err = TypeError::NotBoolean {
            op: "AND",
            actual: DataType::Integer,
        };

        assert_eq!(
            err.to_string(),
            "argument of AND must be type boolean, not type integer"
        );
        assert_eq!(err.sqlstate(), "42804");
    }

    #[test]
    fn text_errors_should_describe_the_problem() {
        for (err, message, _) in text_errors() {
            assert_eq!(err.to_string(), message);
        }
    }

    #[test]
    fn text_errors_should_map_to_sql_standard_sqlstates() {
        for (err, _, sqlstate) in text_errors() {
            assert_eq!(err.sqlstate(), sqlstate, "{err:?}");
        }
    }
}
