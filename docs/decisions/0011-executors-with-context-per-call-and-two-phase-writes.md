# 0011. Executors take the database on each call and write in two phases

- Status: Accepted
- Date: 2026-10-21

## Context

ferroDB needed executors to run queries over its tables and indexes. In BusTub, each executor keeps a pointer to an `ExecutorContext` (catalog, buffer pool) and its children, and the INSERT/DELETE/UPDATE executors update the table and its indexes row by row. In Rust, a tree of executors each holding `&mut Catalog` and `&mut BufferPoolManager` does not compile, and the table scan (`TableScan`) borrows the buffer pool for as long as it lives. Writing row by row also has two problems: an update that moves a row to a later page meets it again in its own scan (the Halloween problem), and a statement that fails on its third row has already changed two, which the SQL standard does not allow.

## Decision

- **The database is passed to each `next(ctx)` call** (`ExecutionContext { catalog, bpm }`), never stored in an executor.
- **The sequential scan keeps a cursor** (the last `Rid` read) and asks the heap for the next live tuple after it (`HeapFile::next_after`), so it holds no borrow and no pinned page between calls.
- **Writes happen in two phases:** first every row of the child is read, then the whole statement is validated without writing (the table's rules through `TableHeap::prepare`, every unique index through `BPlusTree::check_key` and `get`, keys repeated among the new rows), and only then is anything written (decided with the author). Index entries an Update moves are all taken out before any is put back.

## Alternatives considered

- **Executors holding references, as in BusTub:** does not compile in safe Rust without `Rc<RefCell<_>>`, which would turn borrow errors into runtime panics.
- **Materializing the table in the scan:** simpler, but memory grows with the table, and the iterator model loses its point.
- **Writing row by row (per-row atomicity):** simpler, but a refused row leaves the earlier ones written, a visible deviation from the standard in a common case.
- **Writing and undoing on failure:** covers some I/O failures too, but needs an inverse for every change (re-inserting deleted rows changes their `Rid`s, and indexes must follow), and the undo can fail as well.

## Consequences

- Executors compose freely: a parent passes the context down to its child on each call.
- A statement that fails on its data or a constraint leaves the table and its indexes unchanged; only an I/O failure while writing can leave it half done, until there is a WAL.
- The Halloween problem cannot happen, and Update can swap unique keys between rows.
- Writes keep all the child's rows in memory, and the scan fetches its page again for every row (almost always a cache hit).

## Implementation and tests

- [`src/execution/`](../../src/execution/), [`HeapFile::next_after`](../../src/storage/heap_file.rs), [`TableHeap::prepare`](../../src/table/table_heap.rs), [`BPlusTree::check_key`](../../src/index/b_plus_tree.rs)
- Unit tests in those modules (every refused statement leaving the table and its indexes unchanged, Halloween cases, swapped keys, a randomized test against a model with reopening) and [`tests/execution_executors.rs`](../../tests/execution_executors.rs)
- Details in [execution.md](../execution.md)
