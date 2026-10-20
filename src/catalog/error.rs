use std::fmt;

use crate::index::IndexError;
use crate::storage::buffer_pool::BufferPoolError;
use crate::storage::heap_file::HeapFileError;
use crate::table::schema::prefix_within;
use crate::table::{SchemaError, TableError};

#[derive(Debug)]
pub enum CatalogError {
    /// Page 0 is neither a fresh page nor a ferroDB database header.
    NotADatabase {
        reason: &'static str,
    },
    EmptyTableName,
    TableNameTooLong {
        name: String,
        max: usize,
    },
    DuplicateTable {
        name: String,
    },
    UndefinedTable {
        name: String,
    },
    /// A row of the table could never fit a page.
    RowTooWide {
        table: String,
        size: usize,
        max: usize,
    },
    TooManyTables,
    Corrupted {
        reason: &'static str,
    },
    CorruptedSchema {
        table: String,
        source: SchemaError,
    },
    EmptyIndexName,
    IndexNameTooLong {
        name: String,
        max: usize,
    },
    DuplicateIndex {
        name: String,
    },
    UndefinedIndex {
        name: String,
    },
    UndefinedColumn {
        table: String,
        column: String,
    },
    TooManyIndexes,
    Heap(HeapFileError),
    BufferPool(BufferPoolError),
    Table(TableError),
    Index(IndexError),
}

impl CatalogError {
    pub fn sqlstate(&self) -> &'static str {
        match self {
            CatalogError::EmptyTableName | CatalogError::EmptyIndexName => "42602",
            CatalogError::TableNameTooLong { .. } | CatalogError::IndexNameTooLong { .. } => {
                "42622"
            }
            CatalogError::DuplicateTable { .. } | CatalogError::DuplicateIndex { .. } => "42P07",
            CatalogError::UndefinedTable { .. } => "42P01",
            CatalogError::UndefinedIndex { .. } => "42704",
            CatalogError::UndefinedColumn { .. } => "42703",
            CatalogError::RowTooWide { .. }
            | CatalogError::TooManyTables
            | CatalogError::TooManyIndexes => "54000",
            CatalogError::NotADatabase { .. }
            | CatalogError::Corrupted { .. }
            | CatalogError::CorruptedSchema { .. } => "XX001",
            CatalogError::Heap(err) => err.sqlstate(),
            CatalogError::BufferPool(err) => err.sqlstate(),
            CatalogError::Table(err) => err.sqlstate(),
            CatalogError::Index(err) => err.sqlstate(),
        }
    }
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatalogError::NotADatabase { reason } => {
                write!(f, "not a ferroDB database: {reason}")
            }
            CatalogError::EmptyTableName => write!(f, "a table name cannot be empty"),
            CatalogError::TableNameTooLong { name, max } => write!(
                f,
                "table name \"{}…\" is longer than {max} bytes",
                prefix_within(name, *max)
            ),
            CatalogError::DuplicateTable { name } => write!(f, "table \"{name}\" already exists"),
            CatalogError::UndefinedTable { name } => write!(f, "table \"{name}\" does not exist"),
            CatalogError::RowTooWide { table, size, max } => write!(
                f,
                "rows of table \"{table}\" need {size} bytes, more than the {max} that fit a page"
            ),
            CatalogError::TooManyTables => write!(f, "no table ids left"),
            CatalogError::Corrupted { reason } => write!(f, "catalog is corrupted: {reason}"),
            CatalogError::CorruptedSchema { table, source } => {
                write!(f, "catalog is corrupted: table \"{table}\": {source}")
            }
            CatalogError::Heap(err) => write!(f, "{err}"),
            CatalogError::BufferPool(err) => write!(f, "{err}"),
            CatalogError::EmptyIndexName => write!(f, "an index name cannot be empty"),
            CatalogError::IndexNameTooLong { name, max } => write!(
                f,
                "index name \"{}…\" is longer than {max} bytes",
                prefix_within(name, *max)
            ),
            CatalogError::DuplicateIndex { name } => write!(f, "index \"{name}\" already exists"),
            CatalogError::UndefinedIndex { name } => write!(f, "index \"{name}\" does not exist"),
            CatalogError::UndefinedColumn { table, column } => {
                write!(f, "column \"{column}\" of table \"{table}\" does not exist")
            }
            CatalogError::TooManyIndexes => write!(f, "no index ids left"),
            CatalogError::Table(err) => write!(f, "{err}"),
            CatalogError::Index(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for CatalogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CatalogError::CorruptedSchema { source, .. } => Some(source),
            CatalogError::Heap(err) => Some(err),
            CatalogError::BufferPool(err) => Some(err),
            CatalogError::Table(err) => Some(err),
            CatalogError::Index(err) => Some(err),
            _ => None,
        }
    }
}

impl From<HeapFileError> for CatalogError {
    fn from(err: HeapFileError) -> Self {
        CatalogError::Heap(err)
    }
}

impl From<TableError> for CatalogError {
    fn from(err: TableError) -> Self {
        CatalogError::Table(err)
    }
}

impl From<IndexError> for CatalogError {
    fn from(err: IndexError) -> Self {
        CatalogError::Index(err)
    }
}

impl From<BufferPoolError> for CatalogError {
    fn from(err: BufferPoolError) -> Self {
        CatalogError::BufferPool(err)
    }
}
