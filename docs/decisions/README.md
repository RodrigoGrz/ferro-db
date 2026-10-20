# Architectural Decision Records

Short records of significant decisions already made in the codebase. Each one links to the code and tests that implement it. The detailed formats stay in [storage-engine.md](../storage-engine.md), [data-model.md](../data-model.md) and [indexes.md](../indexes.md); these records explain *why*.

The rationale comes from the design notes written while each component was built. When a rationale was not recorded, the ADR says so instead of guessing.

| # | Decision | Status |
|---|---|---|
| [0001](0001-slotted-pages-with-stable-slot-ids.md) | Slotted pages with stable slot ids and lazy compaction | Accepted |
| [0002](0002-page-header-and-checksum.md) | Common 16-byte page header with type, version and CRC32 checksum | Accepted |
| [0003](0003-lru-k-replacement.md) | LRU-K (K = 2) as the buffer pool replacement policy | Accepted |
| [0004](0004-heap-file-as-linked-list.md) | Heap file as a linked list of pages; an update that does not fit changes the RID | Accepted |
| [0005](0005-in-memory-free-space-map.md) | In-memory free space map with first-fit placement | Accepted |
| [0006](0006-catalog-on-page-zero.md) | Catalog bootstrapped from page 0, stored in two heaps with its own record format | Accepted |
| [0007](0007-fail-without-side-effects.md) | Operations validate first and fail without side effects; disk data is never trusted | Accepted |
| [0008](0008-varchar-utf8-byte-order-and-assignment.md) | Text is `VARCHAR(n)` in UTF-8, compared by bytes, with store assignment separate from `CAST` | Accepted |
| [0009](0009-floats-finite-exact-comparison-numeric-family.md) | Floats are finite, compare exactly with integers and share one numeric family | Accepted |
| [0010](0010-b-plus-tree-variable-nodes-and-late-writes.md) | B+Tree with variable-size nodes, order-preserving keys and writing last | Accepted |

## Adding a record

Copy the structure below into `NNNN-short-title.md`, add a row to the table and link it from the relevant section of the design notes.

```markdown
# NNNN. Title

- Status: Proposed | Accepted | Superseded by NNNN
- Date: YYYY-MM-DD

## Context
## Decision
## Alternatives considered
## Consequences
## Implementation and tests
```
