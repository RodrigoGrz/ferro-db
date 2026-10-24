use std::fmt;

use crate::catalog::CatalogError;
use crate::execution::ExecutionError;
use crate::sql::SqlError;
use crate::table::SchemaError;
use crate::types::TypeError;

/// An error while turning a statement into work and doing it. The layers below keep their own
/// errors and SQLSTATEs; the variants of its own are the binder's.
#[derive(Debug)]
pub enum PlannerError {
    Sql(SqlError),
    Catalog(CatalogError),
    Schema(SchemaError),
    Type(TypeError),
    Execution(ExecutionError),
    UndefinedColumn {
        column: String,
    },
    /// A column qualified by a table the statement does not use.
    UndefinedTableReference {
        table: String,
    },
    /// A column named twice in the column list of an `INSERT`.
    DuplicateColumn {
        column: String,
    },
    /// A column set twice by one `UPDATE`.
    DuplicateAssignment {
        column: String,
    },
    /// A row of `VALUES` with another number of values than there are target columns.
    ColumnCountMismatch {
        expected: usize,
        found: usize,
    },
    /// An aggregate where none may be (`WHERE`, `VALUES`, `SET`).
    AggregateNotAllowed {
        function: String,
    },
    UndefinedFunction {
        name: String,
    },
    /// Valid SQL that ferroDB does not run yet.
    Unsupported {
        feature: &'static str,
    },
    /// A write executor that did not report how many rows it changed: a plan built wrong.
    MissingCount,
}

impl PlannerError {
    pub fn sqlstate(&self) -> &'static str {
        match self {
            PlannerError::Sql(err) => err.sqlstate(),
            PlannerError::Catalog(err) => err.sqlstate(),
            PlannerError::Schema(err) => err.sqlstate(),
            PlannerError::Type(err) => err.sqlstate(),
            PlannerError::Execution(err) => err.sqlstate(),
            PlannerError::UndefinedColumn { .. } => "42703",
            PlannerError::UndefinedTableReference { .. } => "42P01",
            PlannerError::DuplicateColumn { .. } => "42701",
            PlannerError::DuplicateAssignment { .. } | PlannerError::ColumnCountMismatch { .. } => {
                "42601"
            }
            PlannerError::AggregateNotAllowed { .. } => "42803",
            PlannerError::UndefinedFunction { .. } => "42883",
            PlannerError::Unsupported { .. } => "0A000",
            PlannerError::MissingCount => "XX000",
        }
    }
}

impl fmt::Display for PlannerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlannerError::Sql(err) => write!(f, "{err}"),
            PlannerError::Catalog(err) => write!(f, "{err}"),
            PlannerError::Schema(err) => write!(f, "{err}"),
            PlannerError::Type(err) => write!(f, "{err}"),
            PlannerError::Execution(err) => write!(f, "{err}"),
            PlannerError::UndefinedColumn { column } => {
                write!(f, "column \"{column}\" does not exist")
            }
            PlannerError::UndefinedTableReference { table } => {
                write!(f, "missing FROM-clause entry for table \"{table}\"")
            }
            PlannerError::DuplicateColumn { column } => {
                write!(f, "column \"{column}\" specified more than once")
            }
            PlannerError::DuplicateAssignment { column } => {
                write!(f, "multiple assignments to same column \"{column}\"")
            }
            PlannerError::ColumnCountMismatch { expected, found } if found > expected => write!(
                f,
                "INSERT has more expressions ({found}) than target columns ({expected})"
            ),
            PlannerError::ColumnCountMismatch { expected, found } => write!(
                f,
                "INSERT has more target columns ({expected}) than expressions ({found})"
            ),
            PlannerError::AggregateNotAllowed { function } => {
                write!(f, "aggregate function \"{function}\" is not allowed here")
            }
            PlannerError::UndefinedFunction { name } => {
                write!(f, "function \"{name}\" does not exist")
            }
            PlannerError::Unsupported { feature } => write!(f, "{feature} is not supported"),
            PlannerError::MissingCount => write!(f, "a write statement returned no row count"),
        }
    }
}

impl std::error::Error for PlannerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PlannerError::Sql(err) => Some(err),
            PlannerError::Catalog(err) => Some(err),
            PlannerError::Schema(err) => Some(err),
            PlannerError::Type(err) => Some(err),
            PlannerError::Execution(err) => Some(err),
            _ => None,
        }
    }
}

impl From<SqlError> for PlannerError {
    fn from(err: SqlError) -> Self {
        PlannerError::Sql(err)
    }
}

impl From<CatalogError> for PlannerError {
    fn from(err: CatalogError) -> Self {
        PlannerError::Catalog(err)
    }
}

impl From<SchemaError> for PlannerError {
    fn from(err: SchemaError) -> Self {
        PlannerError::Schema(err)
    }
}

impl From<TypeError> for PlannerError {
    fn from(err: TypeError) -> Self {
        PlannerError::Type(err)
    }
}

impl From<ExecutionError> for PlannerError {
    fn from(err: ExecutionError) -> Self {
        PlannerError::Execution(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_error() -> Vec<(PlannerError, &'static str, &'static str, bool)> {
        vec![
            (
                PlannerError::Sql(SqlError::UnterminatedString { position: 3 }),
                "unterminated quoted string at character 3",
                "42601",
                true,
            ),
            (
                PlannerError::Catalog(CatalogError::UndefinedTable {
                    name: "t".to_string(),
                }),
                "table \"t\" does not exist",
                "42P01",
                true,
            ),
            (
                PlannerError::Schema(SchemaError::NoColumns),
                "a table must have at least one column",
                "42P16",
                true,
            ),
            (
                PlannerError::Type(TypeError::DivisionByZero),
                "division by zero",
                "22012",
                true,
            ),
            (
                PlannerError::Execution(ExecutionError::MissingRid),
                "a row without a row id reached an executor that changes rows",
                "XX000",
                true,
            ),
            (
                PlannerError::UndefinedColumn {
                    column: "x".to_string(),
                },
                "column \"x\" does not exist",
                "42703",
                false,
            ),
            (
                PlannerError::UndefinedTableReference {
                    table: "u".to_string(),
                },
                "missing FROM-clause entry for table \"u\"",
                "42P01",
                false,
            ),
            (
                PlannerError::DuplicateColumn {
                    column: "id".to_string(),
                },
                "column \"id\" specified more than once",
                "42701",
                false,
            ),
            (
                PlannerError::DuplicateAssignment {
                    column: "v".to_string(),
                },
                "multiple assignments to same column \"v\"",
                "42601",
                false,
            ),
            (
                PlannerError::ColumnCountMismatch {
                    expected: 2,
                    found: 3,
                },
                "INSERT has more expressions (3) than target columns (2)",
                "42601",
                false,
            ),
            (
                PlannerError::ColumnCountMismatch {
                    expected: 2,
                    found: 1,
                },
                "INSERT has more target columns (2) than expressions (1)",
                "42601",
                false,
            ),
            (
                PlannerError::AggregateNotAllowed {
                    function: "count".to_string(),
                },
                "aggregate function \"count\" is not allowed here",
                "42803",
                false,
            ),
            (
                PlannerError::UndefinedFunction {
                    name: "f".to_string(),
                },
                "function \"f\" does not exist",
                "42883",
                false,
            ),
            (
                PlannerError::Unsupported { feature: "SELECT" },
                "SELECT is not supported",
                "0A000",
                false,
            ),
            (
                PlannerError::MissingCount,
                "a write statement returned no row count",
                "XX000",
                false,
            ),
        ]
    }

    #[test]
    fn errors_should_describe_the_problem() {
        for (err, message, _, _) in every_error() {
            assert_eq!(err.to_string(), message);
        }
    }

    #[test]
    fn errors_should_map_to_sqlstates() {
        for (err, _, sqlstate, _) in every_error() {
            assert_eq!(err.sqlstate(), sqlstate, "{err:?}");
        }
    }

    #[test]
    fn wrapped_errors_should_expose_their_source() {
        for (err, _, _, has_source) in every_error() {
            assert_eq!(
                std::error::Error::source(&err).is_some(),
                has_source,
                "{err:?}"
            );
        }
    }
}
