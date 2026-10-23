use std::fmt;

use crate::catalog::CatalogError;
use crate::index::IndexError;
use crate::table::TableError;
use crate::types::{DataType, TypeError};

use super::JoinType;

#[derive(Debug)]
pub enum ExecutionError {
    Type(TypeError),
    Catalog(CatalogError),
    Table(TableError),
    Index(IndexError),
    /// An expression reads a column the row does not have: a plan built wrong.
    ColumnOutOfRange {
        column: usize,
        len: usize,
    },
    /// A row without a `Rid` reached Delete or Update: a plan built wrong.
    MissingRid,
    /// An aggregate given a type it does not accept, such as `SUM` of text.
    UndefinedFunction {
        function: &'static str,
        arg: DataType,
    },
    /// A `LIMIT` or `OFFSET` count that is NULL (`None`) or negative.
    InvalidRowCount {
        clause: RowCountClause,
        count: Option<i64>,
    },
    /// A join type the executor cannot run, such as a `RIGHT` index nested loop join.
    UnsupportedJoinType {
        join_type: JoinType,
    },
    /// A row of a join input whose width is not the declared one: a plan built wrong.
    RowWidthMismatch {
        expected: usize,
        actual: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowCountClause {
    Limit,
    Offset,
}

impl RowCountClause {
    fn keyword(self) -> &'static str {
        match self {
            RowCountClause::Limit => "LIMIT",
            RowCountClause::Offset => "OFFSET",
        }
    }
}

impl ExecutionError {
    pub fn sqlstate(&self) -> &'static str {
        match self {
            ExecutionError::Type(err) => err.sqlstate(),
            ExecutionError::Catalog(err) => err.sqlstate(),
            ExecutionError::Table(err) => err.sqlstate(),
            ExecutionError::Index(err) => err.sqlstate(),
            ExecutionError::ColumnOutOfRange { .. }
            | ExecutionError::MissingRid
            | ExecutionError::RowWidthMismatch { .. } => "XX000",
            ExecutionError::UnsupportedJoinType { .. } => "0A000",
            ExecutionError::UndefinedFunction { .. } => "42883",
            // The standard's FETCH FIRST and result OFFSET clauses.
            ExecutionError::InvalidRowCount {
                clause: RowCountClause::Limit,
                ..
            } => "2201W",
            ExecutionError::InvalidRowCount {
                clause: RowCountClause::Offset,
                ..
            } => "2201X",
        }
    }
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecutionError::Type(err) => write!(f, "{err}"),
            ExecutionError::Catalog(err) => write!(f, "{err}"),
            ExecutionError::Table(err) => write!(f, "{err}"),
            ExecutionError::Index(err) => write!(f, "{err}"),
            ExecutionError::ColumnOutOfRange { column, len } => {
                write!(
                    f,
                    "column {column} is out of range for a row of {len} values"
                )
            }
            ExecutionError::MissingRid => write!(
                f,
                "a row without a row id reached an executor that changes rows"
            ),
            ExecutionError::UndefinedFunction { function, arg } => write!(
                f,
                "function {function}({}) does not exist",
                arg.name().to_ascii_lowercase()
            ),
            ExecutionError::InvalidRowCount {
                clause,
                count: Some(count),
            } => write!(f, "{} must not be negative, got {count}", clause.keyword()),
            ExecutionError::InvalidRowCount {
                clause,
                count: None,
            } => write!(f, "{} must not be null", clause.keyword()),
            ExecutionError::UnsupportedJoinType { join_type } => write!(
                f,
                "{} is not supported by an index nested loop join",
                join_type.keyword()
            ),
            ExecutionError::RowWidthMismatch { expected, actual } => write!(
                f,
                "a join input declared {expected} values per row produced a row of {actual}"
            ),
        }
    }
}

impl std::error::Error for ExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ExecutionError::Type(err) => Some(err),
            ExecutionError::Catalog(err) => Some(err),
            ExecutionError::Table(err) => Some(err),
            ExecutionError::Index(err) => Some(err),
            ExecutionError::ColumnOutOfRange { .. }
            | ExecutionError::MissingRid
            | ExecutionError::UndefinedFunction { .. }
            | ExecutionError::InvalidRowCount { .. }
            | ExecutionError::UnsupportedJoinType { .. }
            | ExecutionError::RowWidthMismatch { .. } => None,
        }
    }
}

impl From<TypeError> for ExecutionError {
    fn from(err: TypeError) -> Self {
        ExecutionError::Type(err)
    }
}

impl From<CatalogError> for ExecutionError {
    fn from(err: CatalogError) -> Self {
        ExecutionError::Catalog(err)
    }
}

impl From<TableError> for ExecutionError {
    fn from(err: TableError) -> Self {
        ExecutionError::Table(err)
    }
}

impl From<IndexError> for ExecutionError {
    fn from(err: IndexError) -> Self {
        ExecutionError::Index(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::table::SchemaError;

    fn every_error() -> Vec<(ExecutionError, &'static str, &'static str, bool)> {
        vec![
            (
                ExecutionError::Type(TypeError::DivisionByZero),
                "division by zero",
                "22012",
                true,
            ),
            (
                ExecutionError::Catalog(CatalogError::UndefinedTable {
                    name: "users".to_string(),
                }),
                "table \"users\" does not exist",
                "42P01",
                true,
            ),
            (
                ExecutionError::Table(TableError::Schema(SchemaError::NullViolation {
                    column: "id".to_string(),
                })),
                "null value in column \"id\" violates not-null constraint",
                "23502",
                true,
            ),
            (
                ExecutionError::Index(IndexError::DuplicateKey {
                    key: "7".to_string(),
                }),
                "duplicate key value (7) violates the unique index",
                "23505",
                true,
            ),
            (
                ExecutionError::ColumnOutOfRange { column: 3, len: 2 },
                "column 3 is out of range for a row of 2 values",
                "XX000",
                false,
            ),
            (
                ExecutionError::MissingRid,
                "a row without a row id reached an executor that changes rows",
                "XX000",
                false,
            ),
            (
                ExecutionError::UndefinedFunction {
                    function: "sum",
                    arg: DataType::VARCHAR_MAX,
                },
                "function sum(varchar) does not exist",
                "42883",
                false,
            ),
            (
                ExecutionError::InvalidRowCount {
                    clause: RowCountClause::Limit,
                    count: Some(-3),
                },
                "LIMIT must not be negative, got -3",
                "2201W",
                false,
            ),
            (
                ExecutionError::InvalidRowCount {
                    clause: RowCountClause::Offset,
                    count: None,
                },
                "OFFSET must not be null",
                "2201X",
                false,
            ),
            (
                ExecutionError::UnsupportedJoinType {
                    join_type: JoinType::Right,
                },
                "RIGHT JOIN is not supported by an index nested loop join",
                "0A000",
                false,
            ),
            (
                ExecutionError::RowWidthMismatch {
                    expected: 3,
                    actual: 2,
                },
                "a join input declared 3 values per row produced a row of 2",
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
