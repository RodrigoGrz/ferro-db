# 0001. Slotted pages with stable slot ids and lazy compaction

- Status: Accepted
- Date: 2026-09-30 (commit `61d5285`)

## Context

Rows have variable sizes, but pages have a fixed size (8 KB). Rows must be addressable from outside the page (future indexes will store their addresses), and rows are inserted, deleted and updated in any order.

## Decision

Each page has a header, a slot array that grows forward from the start of the page, and a tuple area that grows backward from the end. A slot stores `(offset u16, len u16)`; `offset == 0` is a tombstone. A row's address is `Rid { page_id, slot_id }`, and **a slot id never changes while its tuple lives on the page**. Deletes only tombstone the slot; bytes are compacted lazily, when an insert or update needs contiguous space. The page is accessed through a zero-copy view (`SlottedPage<&[u8]>` / `SlottedPage<&mut [u8]>`).

## Alternatives considered

- *Rows packed back to back without a slot array:* finding row N requires reading the rows before it, and deleting a row either leaves holes or shifts the others, changing their addresses. This is the motivation for the slot indirection; it is the standard design taught in CMU 15-445.
- *Eager compaction on delete:* rejected in favor of lazy compaction, so deletes stay cheap and the cost of moving bytes appears only when space is actually needed.

## Consequences

- Compaction can move tuples without invalidating any RID.
- Deleted slots stay in the slot array, so an emptied page can no longer hold the maximum-size tuple (each extra slot keeps 4 bytes).
- Inserts reuse deleted slots. Once indexes and MVCC exist, a slot may only be reused after the index entries pointing to it are cleaned up; otherwise an old RID would point to a different tuple.
- Offsets are `u16`, which caps the page size at 65,535 bytes (`MAX_PAGE_SIZE`, checked at compile time).

## Implementation and tests

- [`src/storage/slotted_page.rs`](../../src/storage/slotted_page.rs), [`src/storage/rid.rs`](../../src/storage/rid.rs)
- Unit tests in both modules, including `random_operations_should_match_model` (10,000 random operations against a `HashMap`)
- [`tests/storage_slotted_page.rs`](../../tests/storage_slotted_page.rs)
- Format: [storage-engine.md § Slotted Page](../storage-engine.md#slotted-page)
