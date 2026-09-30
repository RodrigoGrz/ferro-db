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
}

impl TypeError {
    pub fn sqlstate(&self) -> &'static str {
        match self {
            TypeError::OutOfRange { .. } | TypeError::TextOutOfRange { .. } => "22003",
            TypeError::InvalidTextRepresentation { .. } => "22P02",
            TypeError::DivisionByZero => "22012",
            TypeError::InvalidEncoding { .. } => "XX001",
        }
    }
}

/// Error messages spell type names in lowercase.
fn type_name(ty: DataType) -> String {
    ty.name().to_ascii_lowercase()
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
}
