# 0003. LRU-K (K = 2) as the buffer pool replacement policy

- Status: Accepted
- Date: 2026-04-20 (commit `b319cc9`)

## Context

When every frame of the buffer pool holds a page, fetching a new page requires evicting an unpinned one. The choice of victim determines how often the database goes back to disk.

## Decision

Use LRU-K with K = 2. Frames with fewer than K recorded accesses are evicted first (the earliest first access wins); otherwise the frame with the largest backward K-distance is evicted; ties go to the lower frame id. Pinned frames are never candidates.

## Alternatives considered

The first version of the buffer pool (commit `59d6b0d`) used plain LRU, implemented with a `VecDeque`. It was replaced by LRU-K the same day. The design notes say only that LRU-K "considers the last K accesses instead of only the most recent one". The specific reason for the switch, and for choosing K = 2, **was not recorded**. LRU-K is the policy used in the CMU 15-445 buffer pool project, on which this component is based.

## Consequences

- With K = 1 the implementation behaves as plain LRU (covered by a test).
- Victim selection scans every candidate frame (O(frames)). This is fine for the small pools used today, but it would need a better structure for large pools.
- `select_victim` only reports a candidate; the frame's history is forgotten only after the buffer pool has actually reused it, which keeps a failed fetch free of side effects.

## Implementation and tests

- [`src/storage/replacer.rs`](../../src/storage/replacer.rs), used by [`src/storage/buffer_pool.rs`](../../src/storage/buffer_pool.rs)
- Unit tests in `replacer.rs` (history capped at K, fewer-than-K first, K-distance, tie by frame id, K = 1 as LRU, pinned frames ignored) and `should_use_lruk_for_eviction` in `buffer_pool.rs`
- Description: [storage-engine.md § LRU-K Replacer](../storage-engine.md#lru-k-replacer)
