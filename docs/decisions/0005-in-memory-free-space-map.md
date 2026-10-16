# 0005. In-memory free space map with first-fit placement

- Status: Accepted
- Date: 2026-10-07 (commit `f1a9764`)

## Context

Without knowing how much room each page has, inserts would either always go to the last page (never reusing space freed by deletes and updates) or would have to read every page to find room.

## Decision

Each `HeapFile` keeps, in memory, one `u16` per page: an **exclusive** bound on the tuple size the page accepts (`0` = full, `1` = only an empty tuple fits). The values live in a max tree, so finding the **first page in list order** that accepts a tuple takes O(log n). The map is rebuilt from the pages on every `open` and updated only after a successful write.

## Alternatives considered

Recorded in the design notes:

- *A persistent map:* rejected for now. `open` already reads every page to follow the list, so building the map is free, there is no format change and no stale map after a crash. A persistent map only pays off once `open` stops walking the list.
- *Coarse 1-byte categories* (instead of exact values): rejected; in memory the extra byte does not matter, and an exact value never promises space a page does not have (for a single handle).
- *Storing "the largest tuple that fits":* does not work, because `0` would mean both "an empty tuple fits" and "nothing fits".
- *Unlinking empty pages:* rejected; the table reuses them through the map, without touching the list or page 0.

## Consequences

- With several handles, each map is only a hint: a value too high becomes a failed attempt that corrects it; a value too low wastes that page until the next `open`. No data is lost.
- Computing a page's value is O(slots), the same order as the slotted page operations.
- First fit fills the start of the heap; the physical order of rows changes, which is fine because a heap has no order.

## Implementation and tests

- [`src/storage/free_space_map.rs`](../../src/storage/free_space_map.rs) (`pub(crate)`), used by [`src/storage/heap_file.rs`](../../src/storage/heap_file.rs); `largest_insertable` in [`src/storage/slotted_page.rs`](../../src/storage/slotted_page.rs)
- Unit tests in all three modules (10,000 operations against a linear search, 5,000 random page states against `can_insert`)
- [`tests/storage_heap_file.rs`](../../tests/storage_heap_file.rs)
- Description: [storage-engine.md § Free Space Management](../storage-engine.md#free-space-management)
