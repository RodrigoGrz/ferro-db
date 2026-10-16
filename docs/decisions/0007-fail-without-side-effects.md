# 0007. Operations validate first and fail without side effects; disk data is never trusted

- Status: Accepted
- Date: introduced in commit `60a5e12` (2026-09-24) and applied to every component since

## Context

The first versions of the disk manager and the buffer pool called `expect` on I/O and ignored short reads, so any failure aborted the process. Once failures became `Result`s, partially applied operations became possible: for example, a buffer pool frame removed from the free list before a read that then fails.

## Decision

Across the codebase:

- Public operations return typed errors (`DiskError`, `BufferPoolError`, `SlottedPageError`, `HeapFileError`, `SchemaError`, `CatalogError`, ...) instead of panicking.
- An operation checks everything it can **before** changing any byte or any in-memory structure, and applies changes only when every step has succeeded. Examples: `DiskManager::write_page` checks the length first; `SlottedPage` checks space before writing; the buffer pool only peeks at the frame it will use; `Schema::encode_row` validates every column before building the tuple; the serialization codec neither advances nor writes on error.
- Bytes read from disk are validated before use (checksums, page headers, slot bounds, canonical tuple form, catalog records), and corrupted data becomes an error, never a panic.

## Alternatives considered

Not recorded as an explicit trade-off; the design notes state the rule ("fail without side effects") in each component.

## Consequences

- After an error, the database remains usable and consistent in memory; tests check this explicitly (for example, "rejected page should leave the pool usable").
- Multi-step operations that span several pages (an update that moves a row, `create_table`) cannot be made fully atomic this way; they use best-effort undo, and real atomicity is left to a future WAL.
- Randomized corruption tests (`random_corruption_should_never_panic`) are part of the test suite.

## Implementation and tests

- Every module under [`src/`](../../src/); see the error tables in [storage-engine.md](../storage-engine.md) and [data-model.md](../data-model.md)
- Injected-failure tests through `BufferPoolManager::fail_fetch_after` (test-only) in `heap_file.rs` and `catalog.rs`
