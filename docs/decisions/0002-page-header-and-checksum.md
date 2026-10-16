# 0002. Common 16-byte page header with type, version and CRC32 checksum

- Status: Accepted
- Date: 2026-09-30 (commit `61d5285`)

## Context

The disk manager can detect a torn page at the end of the file (its size is wrong), but not a page that was partially overwritten in place. The buffer pool also needs to know, before handing a page to anyone, whether it is a page it understands. The first header had only the LSN and a checksum (12 bytes), with no way to tell page types or format versions apart.

## Decision

Every page, of every type, starts with the same 16 bytes: LSN (`u64`, reserved for the WAL), CRC32 checksum (`u32`), page type (`u8`), format version (`u8`) and 2 reserved bytes. The buffer pool stamps the checksum before every write and verifies the checksum and the header on every read from disk; a page that fails never enters the pool. An all-zero page is accepted as a new, never-written page. Encoding goes through a hand-written little-endian codec, with no serialization dependencies.

## Alternatives considered

Recorded in the design notes:

- *Type and version in the slotted page's reserved bytes:* rejected; a common header lets every future page type (B+Tree, free space map) inherit type and version checks.
- *An `UnknownPageType` error variant:* rejected; an unknown type is reported as the codec's `InvalidValue`, because `Decode` has a single error type.
- *`serde` / `bincode`:* rejected to keep the on-disk format fully controlled and documented.

## Consequences

- Corruption and partial overwrites are detected on read, before any frame is used.
- A single global `PAGE_FORMAT_VERSION`: if the layout of any page type changes, the version goes up.
- The `match` on `PageType` is exhaustive, so adding a page type forces every page reader to handle it.
- Growing the header from 12 to 16 bytes made older database files unreadable; there is no migration (database files are disposable during development).
- The checksum catches accidents, not tampering.

## Implementation and tests

- [`src/storage/page.rs`](../../src/storage/page.rs), [`src/storage/checksum.rs`](../../src/storage/checksum.rs), [`src/serialization.rs`](../../src/serialization.rs), checks in [`src/storage/buffer_pool.rs`](../../src/storage/buffer_pool.rs)
- [`tests/storage_page_serialization.rs`](../../tests/storage_page_serialization.rs)
- Format: [storage-engine.md § Common Page Header and Checksum](../storage-engine.md#common-page-header-and-checksum)
