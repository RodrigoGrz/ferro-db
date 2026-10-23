use std::fmt;

/// A syntax error in a statement. `position` is the character where the problem starts,
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
}

impl SqlError {
    /// Every error of the layer is a syntax error so far.
    pub fn sqlstate(&self) -> &'static str {
        "42601"
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
        }
    }
}

impl std::error::Error for SqlError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_error() -> Vec<(SqlError, &'static str)> {
        vec![
            (
                SqlError::UnterminatedString { position: 8 },
                "unterminated quoted string at character 8",
            ),
            (
                SqlError::UnterminatedQuotedIdentifier { position: 3 },
                "unterminated quoted identifier at character 3",
            ),
            (
                SqlError::EmptyQuotedIdentifier { position: 1 },
                "zero-length delimited identifier at character 1",
            ),
            (
                SqlError::UnterminatedComment { position: 12 },
                "unterminated /* comment at character 12",
            ),
            (
                SqlError::UnexpectedCharacter {
                    character: '!',
                    position: 8,
                },
                "unexpected character \"!\" at character 8",
            ),
            (
                SqlError::InvalidNumber {
                    literal: "12abc".to_string(),
                    position: 5,
                },
                "invalid numeric literal \"12abc\" at character 5",
            ),
        ]
    }

    #[test]
    fn errors_should_describe_the_problem_and_its_position() {
        for (err, message) in every_error() {
            assert_eq!(err.to_string(), message);
        }
    }

    #[test]
    fn every_error_should_be_a_syntax_error() {
        for (err, _) in every_error() {
            assert_eq!(err.sqlstate(), "42601", "{err:?}");
        }
    }
}
