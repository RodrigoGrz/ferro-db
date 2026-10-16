# 0004. Heap file as a linked list of pages; an update that does not fit changes the RID

- Status: Accepted
- Date: 2026-10-01 (commit `c14a5f0`)

## Context

A table needs more than one page, and the whole database lives in a single file, so a table's pages are scattered among other tables' pages. Rows can also grow on update beyond the free space of their page.

## Decision

- A table's heap is a linked list of slotted pages: each page stores the id of the next one in bytes `20..24` of its header (`next_page_id`, `0` = end). The heap is identified by its first page.
- Page 0 is reserved for the database header, so `0` can mean "no next page" (`Option<NonZeroU32>` in the API).
- An update that does not fit in its page inserts the new version elsewhere, then deletes the old one, and **returns the new `Rid`** (`update(..) -> Result<Rid, _>`).
- `HeapFile` methods take `&mut BufferPoolManager` instead of owning a pool, and all page accesses go through helpers that unpin on every path.

## Alternatives considered

Recorded in the design notes:

- *A page directory:* would anticipate free space management and add another structure to keep in sync. The linked list fits in a previously reserved field (like BusTub's `TableHeap`) without changing the page format.
- *A forwarding pointer for moved rows:* rejected; it requires telling stubs from tuples, keeping scans from returning a moved row twice and handling chains of forwards. With MVCC, every update will produce a new version at a new address anyway, and indexes will update the `Rid` when it changes.

## Consequences

- `open` must read every page of the heap to build the page list (and the free space map, see [ADR 0005](0005-in-memory-free-space-map.md)).
- An update that moves is not atomic: undo on error is best effort. An I/O error between allocating a page and linking it leaves an unreachable page. Both need the WAL to be fixed.
- Several handles to the same heap stay safe for inserts, but each one only sees pages appended by others after its next insert or a new `open`; the table layer should keep one handle per table.
- `get` and `scan` return copies of the tuple bytes, because there is no pin guard to keep a page pinned between calls.

## Implementation and tests

- [`src/storage/heap_file.rs`](../../src/storage/heap_file.rs), `new_page` in [`src/storage/buffer_pool.rs`](../../src/storage/buffer_pool.rs)
- Unit tests in `heap_file.rs`, including `random_operations_should_match_model` and injected failures
- [`tests/storage_heap_file.rs`](../../tests/storage_heap_file.rs)
- Description: [storage-engine.md § Heap File](../storage-engine.md#heap-file)
