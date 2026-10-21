//! The iterator (Volcano) model: every executor hands out one row per `next` call, pulling
//! rows from its children as it needs them, and a query is a tree of executors whose root is
//! pulled until it ends.
//!
//! The database (`ExecutionContext`) is passed to each call instead of being stored in the
//! executors, so a tree of them never holds two mutable borrows of the catalog or the buffer
//! pool, and no executor keeps a page pinned between calls.

use crate::catalog::Catalog;
use crate::storage::buffer_pool::BufferPoolManager;
use crate::storage::rid::Rid;
use crate::types::Value;

use super::ExecutionError;

/// A row flowing between executors. Rows read from a table carry their `Rid`, which Delete
/// and Update need; computed rows have none.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub values: Vec<Value>,
    pub rid: Option<Rid>,
}

pub struct ExecutionContext<'a> {
    pub catalog: &'a mut Catalog,
    pub bpm: &'a mut BufferPoolManager,
}

pub trait Executor {
    /// The next row, or `None` when there are no more (and on every call after that). After an
    /// `Err` the executor is finished: its children may have been consumed halfway, so it must
    /// not be called again (an Insert, Update or Delete would answer `None`).
    fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError>;
}

/// Pulls `root` until it ends.
pub fn execute(
    root: &mut dyn Executor,
    ctx: &mut ExecutionContext<'_>,
) -> Result<Vec<Row>, ExecutionError> {
    let mut rows = Vec::new();
    while let Some(row) = root.next(ctx)? {
        rows.push(row);
    }
    Ok(rows)
}
