//! The state of a pipeline breaker (`Aggregation`, `Sort`, `TopN`): the first call reads
//! the whole child into rows, later calls hand them out one by one.

use super::{ExecutionError, Row};

/// `Start` holds what building the rows needs (the counts of a `TopN`, or nothing).
pub(super) enum Materialized<S> {
    Start(S),
    Emitting(std::vec::IntoIter<Row>),
    Done,
}

impl<S> Materialized<S> {
    /// The next row, building them all with `build` on the first call. The state is `Done`
    /// until the rows are in hand, so an error ends the executor.
    pub(super) fn next(
        &mut self,
        build: impl FnOnce(S) -> Result<Vec<Row>, ExecutionError>,
    ) -> Result<Option<Row>, ExecutionError> {
        let mut rows = match std::mem::replace(self, Materialized::Done) {
            Materialized::Start(start) => build(start)?.into_iter(),
            Materialized::Emitting(rows) => rows,
            Materialized::Done => return Ok(None),
        };
        let row = rows.next();
        *self = Materialized::Emitting(rows);
        Ok(row)
    }
}
