use std::fmt;

/// An error in the text of a statement. `position` is the character where the problem starts,
/// counting from 1, as PostgreSQL reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlError {
    UnterminatedString {
        position: usize,
    },
    UnterminatedQuotedIdentifier {
        position: usize,
    },
    EmptyQuotedIdentifier {
        position: usize,
    },
    UnterminatedComment {
        position: usize,
    },
    UnexpectedCharacter {
        character: char,
        position: usize,
    },
    /// A number with a trailing letter (`12abc`) or an exponent without digits (`1e`).
    InvalidNumber {
        literal: String,
        position: usize,
    },
    /// A token the grammar does not allow where it is; `found` is its source text.
    UnexpectedToken {
        found: String,
        expected: String,
        position: usize,
    },
    UnexpectedEnd {
        expected: String,
    },
    /// A type name ferroDB does not have, such as `text` or `char`.
    UndefinedType {
        name: String,
        position: usize,
    },
    /// `VARCHAR(n)` with `n` outside 1..=65535 or not an integer.
    InvalidVarcharLength {
        length: String,
        position: usize,
    },
    /// Valid SQL that ferroDB does not support yet, such as `SELECT DISTINCT`.
    Unsupported {
        feature: &'static str,
        position: usize,
    },
    /// Expressions or joins nested deeper than the parser allows, so that a statement can
    /// never exhaust the stack.
    TooDeep {
        position: usize,
    },
}

impl SqlError {
    pub fn sqlstate(&self) -> &'static str {
        match self {
            SqlError::UndefinedType { .. } => "42704",
            SqlError::InvalidVarcharLength { .. } => "22023",
            SqlError::Unsupported { .. } => "0A000",
            SqlError::TooDeep { .. } => "54001",
            SqlError::UnterminatedString { .. }
            | SqlError::UnterminatedQuotedIdentifier { .. }
            | SqlError::EmptyQuotedIdentifier { .. }
            | SqlError::UnterminatedComment { .. }
            | SqlError::UnexpectedCharacter { .. }
            | SqlError::InvalidNumber { .. }
            | SqlError::UnexpectedToken { .. }
            | SqlError::UnexpectedEnd { .. } => "42601",
        }
    }
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlError::UnterminatedString { position } => {
                write!(f, "unterminated quoted string at character {position}")
            }
            SqlError::UnterminatedQuotedIdentifier { position } => {
                write!(f, "unterminated quoted identifier at character {position}")
            }
            SqlError::EmptyQuotedIdentifier { position } => {
                write!(
                    f,
                    "zero-length delimited identifier at character {position}"
                )
            }
            SqlError::UnterminatedComment { position } => {
                write!(f, "unterminated /* comment at character {position}")
            }
            SqlError::UnexpectedCharacter {
                character,
                position,
            } => write!(
                f,
                "unexpected character \"{character}\" at character {position}"
            ),
            SqlError::InvalidNumber { literal, position } => {
                write!(
                    f,
                    "invalid numeric literal \"{literal}\" at character {position}"
                )
            }
            SqlError::UnexpectedToken {
                found,
                expected,
                position,
            } => write!(
                f,
                "syntax error at character {position}: expected {expected}, found \"{found}\""
            ),
            SqlError::UnexpectedEnd { expected } => {
                write!(f, "syntax error at end of input: expected {expected}")
            }
            SqlError::UndefinedType { name, position } => {
                write!(f, "type \"{name}\" does not exist at character {position}")
            }
            SqlError::InvalidVarcharLength { length, position } => write!(
                f,
                "length for type varchar must be between 1 and 65535, got {length} at character {position}"
            ),
            SqlError::Unsupported { feature, position } => {
                write!(f, "{feature} is not supported at character {position}")
            }
            SqlError::TooDeep { position } => {
                write!(f, "statement is nested too deeply at character {position}")
            }
        }
    }
}

impl std::error::Error for SqlError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_error() -> Vec<(SqlError, &'static str, &'static str)> {
        vec![
            (
                SqlError::UnterminatedString { position: 8 },
                "unterminated quoted string at character 8",
                "42601",
            ),
            (
                SqlError::UnterminatedQuotedIdentifier { position: 3 },
                "unterminated quoted identifier at character 3",
                "42601",
            ),
            (
                SqlError::EmptyQuotedIdentifier { position: 1 },
                "zero-length delimited identifier at character 1",
                "42601",
            ),
            (
                SqlError::UnterminatedComment { position: 12 },
                "unterminated /* comment at character 12",
                "42601",
            ),
            (
                SqlError::UnexpectedCharacter {
                    character: '!',
                    position: 8,
                },
                "unexpected character \"!\" at character 8",
                "42601",
            ),
            (
                SqlError::InvalidNumber {
                    literal: "12abc".to_string(),
                    position: 5,
                },
                "invalid numeric literal \"12abc\" at character 5",
                "42601",
            ),
            (
                SqlError::UnexpectedToken {
                    found: "FROM".to_string(),
                    expected: "an expression".to_string(),
                    position: 8,
                },
                "syntax error at character 8: expected an expression, found \"FROM\"",
                "42601",
            ),
            (
                SqlError::UnexpectedEnd {
                    expected: "\")\"".to_string(),
                },
                "syntax error at end of input: expected \")\"",
                "42601",
            ),
            (
                SqlError::UndefinedType {
                    name: "text".to_string(),
                    position: 16,
                },
                "type \"text\" does not exist at character 16",
                "42704",
            ),
            (
                SqlError::InvalidVarcharLength {
                    length: "0".to_string(),
                    position: 20,
                },
                "length for type varchar must be between 1 and 65535, got 0 at character 20",
                "22023",
            ),
            (
                SqlError::Unsupported {
                    feature: "SELECT DISTINCT",
                    position: 8,
                },
                "SELECT DISTINCT is not supported at character 8",
                "0A000",
            ),
            (
                SqlError::TooDeep { position: 210 },
                "statement is nested too deeply at character 210",
                "54001",
            ),
        ]
    }

    #[test]
    fn errors_should_describe_the_problem_and_its_position() {
        for (err, message, _) in every_error() {
            assert_eq!(err.to_string(), message);
        }
    }

    #[test]
    fn errors_should_map_to_sqlstates() {
        for (err, _, sqlstate) in every_error() {
            assert_eq!(err.sqlstate(), sqlstate, "{err:?}");
        }
    }
}
