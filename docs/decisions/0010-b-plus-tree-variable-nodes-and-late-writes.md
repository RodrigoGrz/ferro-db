# 0010. B+Tree with variable-size nodes, order-preserving keys and writing last

- Status: Accepted
- Date: 2026-10-20

## Context

ferroDB needed its first index. The textbook B+Tree of CMU 15-445 (BusTub) uses fixed-size keys: every entry has the same size, a node holds a known number of them, a split cuts it in half by count and "half full" is a count. ferroDB already had text keys (`VARCHAR`), dates, timestamps and floats, and the index had to keep the rule of [ADR 0007](0007-fail-without-side-effects.md): an operation that fails changes nothing. A B+Tree split or merge changes several pages, which makes that rule harder than in a single slotted page.

## Decision

- **Variable-size nodes from the start.** A node is a small ordered slotted page: a slot directory points to entries of `key_len` · key · payload, so binary search still works and any key type fits, including `VARCHAR`. Splits, merges and redistribution count bytes; a node is underfull when its entries take less than a quarter of the space, below the halves a split leaves, so a node does not merge back right after it splits. Every write rebuilds the whole node, so a node never has holes.
- **A maximum key size of a quarter of a node** (`(page_size − 28) / 4 − 10`, 2,031 bytes with 8 KB pages): any node holds four entries of the largest key, so a split always leaves two halves that fit and a separator always fits its parent. A larger key is `54000`.
- **Order-preserving key encoding.** Each type is written so that `memcmp` order equals SQL order (big-endian integers with the sign bit flipped, the IEEE bit trick for floats, UTF-8 bytes for text). The tree compares bytes and never decodes a key to search.
- **Compute, pin, then write.** An operation computes every page it changes in memory, then pins all of them and encodes each image into a copy of its page, and only then copies the images into the frames. Until the copy nothing has changed; the copy cannot fail because every page is pinned and resident.
- **Unique keys**, a header page as the tree's fixed identity, and a free list in that header for pages freed by merges.

## Alternatives considered

- **Fixed-size keys, as in BusTub:** simpler nodes and arithmetic, but text keys would have needed a second node format later, with split, merge and redistribution rewritten for bytes. The author chose to pay that cost once.
- **Comparing decoded `Value`s with `sort_cmp`:** no encoding to get right, but every comparison would decode and allocate, and the tree would depend on SQL semantics. The encoding is tested against `sort_cmp` instead, type by type.
- **Writing level by level with best-effort undo** (as `HeapFile::update` and `create_table` do): fewer pinned pages, but a failure in the middle of a split would leave a half-changed tree in memory. Pinning only the pages that change costs one frame for most inserts and about `2 × height + 2` for a split that climbs to the root.
- **Pinning the whole path during the descent** (the single-threaded counterpart of *latch crabbing*): considered in the plan; pinning only the pages that change, after computing them, needs fewer frames and gives the same guarantee.
- **Guaranteeing a minimum fill:** with variable sizes, a new separator may not fit the parent; the redistribution is skipped instead, which costs space but never correctness.

## Consequences

- One code path serves every key type, and text keys work from the first version.
- A failed operation leaves the tree and the buffer pool as they were (checked with too-small pools and injected fetch failures), except for pages already taken from `new_page`, which stay unowned, as in the heap file.
- Real atomicity across a crash still needs the WAL: the pages of one commit reach the disk in any order.
- Nodes are decoded and rebuilt whole on every access, which copies up to a page per node; searching the slots in place is left as an optimization.

## Implementation and tests

- [`src/index/key.rs`](../../src/index/key.rs), [`src/index/node.rs`](../../src/index/node.rs), [`src/index/b_plus_tree.rs`](../../src/index/b_plus_tree.rs)
- Unit tests in those modules (byte order against `sort_cmp`, every corruption reason, `check_invariants`, two randomized tests against a `BTreeMap`, too-small pools and injected failures) and [`tests/index_b_plus_tree.rs`](../../tests/index_b_plus_tree.rs)
- Details in [indexes.md](../indexes.md)
