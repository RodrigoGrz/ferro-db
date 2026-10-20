# Indexes

This document is the authoritative description of ferroDB's index layer (`src/index/`): the B+Tree, its key encoding and page formats, operations, error handling and tests. Page headers, the buffer pool and the other storage formats are in [storage-engine.md](storage-engine.md); SQL types are in [data-model.md](data-model.md).

Dependencies point downward: `index` → `types` (keys are `Value`s of a `DataType`) and `storage` (pages). Nothing uses the index yet: linking it to tables (`CREATE INDEX`, index metadata in the catalog, keeping it in step with `TableHeap`) is the next step.

- [B+Tree](#btree)
- [Keys](#keys)
- [Page formats](#page-formats)
- [Operations](#operations)
- [Failing without side effects](#failing-without-side-effects)
- [Errors](#errors)
- [Design decisions](#design-decisions)
- [Tests](#tests)
- [Limitations](#limitations)

---

## B+Tree

`BPlusTree` (`src/index/b_plus_tree.rs`) is a **unique** index from keys of one type to row ids (`Rid`), stored in buffer pool pages. It follows the B+Tree of CMU 15-445 (lectures 8–9, project 2):

- **Internal nodes** only guide the search: *n* children separated by *n − 1* keys.
- **Leaves** hold every key with its `Rid`, in key order, and are linked left to right, so a range is read leaf after leaf.
- Every leaf is at the same depth. Each node is one page, so a few levels hold millions of keys.

```rust
let mut index = BPlusTree::create(&mut bpm, DataType::Integer)?;   // header page + empty root leaf
index.insert(&mut bpm, &Value::Integer(42), rid)?;                  // DuplicateKey (23505) if present
let rid = index.get(&mut bpm, &Value::Integer(42))?;                 // Option<Rid>
for entry in index.range(&mut bpm, Bound::Included(&low), Bound::Excluded(&high))? {
    let (key, rid) = entry?;                                         // keys in order
}
let removed = index.remove(&mut bpm, &Value::Integer(42))?;          // Option<Rid>
let index = BPlusTree::open(&mut bpm, DataType::Integer, index.header_page_id())?;
```

The tree is identified by its **header page**, which points to the root. The root changes on splits and merges; the header never does, so its id is what the catalog will store. The header also records the key type: whoever opens the tree names it, as a `TableHeap` receives its schema, and another type is `KeyTypeMismatch` instead of keys read the wrong way. For an index every `VARCHAR(n)` is the same type: the length belongs to the column.

## Keys

`key.rs` writes each key as bytes whose **`memcmp` order is the SQL order** of the key type, so the tree compares keys as bytes and never needs to know SQL types:

| Type | Bytes |
|---|---|
| `SMALLINT` / `INTEGER` / `BIGINT` | big-endian with the sign bit flipped (2 / 4 / 8 bytes): `i64::MIN` → `00…`, `-1` → `7F FF…`, `0` → `80 00…` |
| `BOOLEAN` | 1 byte: `0` false, `1` true |
| `DATE` | days since 1970-01-01 (`i32`), like `INTEGER` |
| `TIMESTAMP` | microseconds since 1970-01-01 (`i64`), like `BIGINT` |
| `REAL` / `DOUBLE PRECISION` | IEEE bits, big-endian: a positive value gets its sign bit set, a negative one has every bit flipped |
| `VARCHAR(n)` | the UTF-8 bytes; the length comes from the node, so there is no terminator |

- Two's complement sorts negative numbers after positive ones; flipping the sign bit moves them first. IEEE floats are sign + magnitude; flipping every bit of a negative one also reverses its magnitude order.
- Floats are finite and never `-0.0` (`Finite32`/`Finite64` normalize it), so equal values have equal bytes.
- Text compares by bytes without padding (`'a' < 'a '`), the order of the rest of ferroDB ([ADR 0008](decisions/0008-varchar-utf8-byte-order-and-assignment.md)).
- A key must already be of the tree's type (`KeyTypeMismatch` otherwise): converting, for example a `SMALLINT` literal for an `INTEGER` index, is the caller's job. A `VARCHAR(n)` tree accepts any text; *n* belongs to the column.
- `NULL` is never a key (`NullKey`): the index of a table will skip rows whose key is `NULL`.
- Decoding (for range results) never trusts the bytes: a wrong size, a boolean other than `0`/`1`, a date or timestamp out of range, a non-finite float, `-0.0` or invalid UTF-8 is `Corrupted`.

The largest key is `max_key_size = (page_size − 28) / 4 − 10`: **2,031 bytes** with 8 KB pages (111 with the 512-byte pages of the tests). Any node then holds at least four entries of the largest key, so a split always leaves two halves that fit and a separator always fits its parent. A page too small for four `BIGINT` keys (under 100 bytes) is `UnsupportedPageSize`.

## Page formats

`node.rs`, little-endian, after the 16-byte common header. Three page types belong to the tree: `BPlusTreeHeader = 3`, `BPlusTreeNode = 4`, `BPlusTreeFree = 5`.

**Header page** (`BPlusTreeHeader`):

| Bytes | Field |
|---|---|
| `16..20` | `root_page_id u32` (never `0`) |
| `20..24` | `free_list_head u32` (`0` = empty) |
| `24..28` | `key_type u32`: `1` SMALLINT, `2` INTEGER, `3` BIGINT, `4` VARCHAR, `5` BOOLEAN, `6` REAL, `7` DOUBLE PRECISION, `8` DATE, `9` TIMESTAMP (the numbering of the catalog) |
| `28..` | zero |

**Free page** (`BPlusTreeFree`): `16..20` `next_free u32` (`0` = last), then zero.

**Node** (`BPlusTreeNode`), a leaf or an internal node:

| Bytes | Field |
|---|---|
| `16..18` | `level u16`: `0` is a leaf; a child is always one level below its parent |
| `18..20` | `slot_count u16` |
| `20..22` | `data_start u16`: the entries fill `data_start..page_size` |
| `22..24` | reserved (zero) |
| `24..28` | `link u32`: a leaf's next leaf (`0` = none), an internal node's first child |
| `28..` | slots: `slot_count × u16`, the offset of each entry, in key order |

| Entry | Fields | Size with its slot |
|---|---|---|
| leaf | `key_len u16` · key · `Rid` (6 bytes) | `10 + key` |
| internal (slot *i*) | `key_len u16` · key · `child u32`, the child *i + 1*, whose keys are all ≥ key *i* | `8 + key` |

- **Every write rebuilds the whole node** (`Node` in memory → page), so the entries are always packed in slot order from `data_start` to the end of the page, with no holes and no free-space bookkeeping inside the node.
- **A read accepts only that form** (`Node::read`): the page type; `data_start` inside the page; the slot directory before `data_start`; each slot at the next packed offset; every `key_len` ≤ `max_key_size`; entries ending exactly at the end of the page; keys strictly increasing; no child equal to the reserved page `0`. Anything else is `Corrupted { page_id, reason }`, never a panic.
- A read decodes the whole node into a `Node` (a `Vec` of entries), and searches use binary search on that `Vec`.

## Operations

- **Descent:** from the root to the leaf where the key belongs, keeping the path (page, node, child taken). In an internal node the child is the number of separators ≤ the key. Each child must be **one level below its parent** (`Corrupted` otherwise), so a damaged page can never send the descent into a loop.
- **`get`:** descent and binary search in the leaf. A key longer than `max_key_size` cannot be in the tree: `None`.
- **`insert`:**
  1. checks `NULL`, the type and the size (`KeyTooLarge`, `54000`), and that the key is not there (`DuplicateKey`, `23505`), before changing anything;
  2. inserts the entry in order into the leaf;
  3. while a node does not fit its page, **splits it at half of its bytes** (at least one entry on each side): a leaf copies the first key of its right half up as the separator and links left → right → old next; an internal node *moves* its middle key up, and the middle child becomes the right node's first child;
  4. a split of the root creates a new root one level up and updates the header.
- **`remove`:** removes the entry from the leaf. A node other than the root is **underfull** when its entries take less than **a quarter** of the space a node has for them (`(page_size − 28) / 4`). Then, with a sibling (the left one when there is one) and the separator between them:
  1. the two are **joined** (leaves concatenate, keeping the right one's next; internal nodes also take the separator down, pointing to the right one's first child);
  2. if the joined node fits a page, it is a **merge**: the right page is freed and the separator leaves the parent, which may be underfull in turn;
  3. otherwise the joined entries are **split again** at half of their bytes, which is a **redistribution**, and the parent gets the new separator. If that separator does not fit the parent (variable-size keys), the redistribution is skipped and the node stays underfull: it only costs space;
  4. a root internal node left with one child is replaced by that child (the tree loses a level); a root leaf may become empty.
- **`range(from, to)`:** descends to the first leaf for `from` (the leftmost one when unbounded), skips the keys below `from` and then reads leaf after leaf through the links until a key is past `to`. The iterator (`BPlusTreeRange`) copies one leaf at a time and unpins it before yielding, so dropping it midway leaves no page pinned. Each next leaf must start after the last key of the previous one (`Corrupted` otherwise, which also stops a damaged chain that loops back); a linked leaf can never be empty. An error is yielded once and ends the iteration.
- **Free list:** a page freed by a merge or by the root shrinking is rewritten as a `BPlusTreeFree` page at the head of the header's list. A split takes pages from that list first and from `BufferPoolManager::new_page` only when it is empty, so filling and emptying a tree again does not grow the file.

## Failing without side effects

Every operation works in three phases ([ADR 0010](decisions/0010-b-plus-tree-variable-nodes-and-late-writes.md), applying [ADR 0007](decisions/0007-fail-without-side-effects.md)):

1. **Compute:** read what it needs (each page fetched and unpinned at once) and build, in memory, the new image of every page it changes: nodes, freed pages and the header (`PageWrites`, one image per page).
2. **Pin and encode:** pin every page that will change and encode each image into a copy of its page (keeping the LSN). A node that does not fit, a fetch that fails or a full pool (`NoAvailableFrame`) stops here, and nothing has changed.
3. **Commit:** copy the encoded images into the frames. Every page is pinned and resident, so each fetch is a cache hit and the step cannot fail.

An operation needs as many free frames as pages it changes: one for an insert that fits its leaf, up to `2 × height + 2` for a split that climbs to the root (each level, its new sibling, the new root and the header), and a few for a merge (node, freed sibling, parent, header). With fewer, it fails with `NoAvailableFrame` (`53000`) before writing anything.

The one exception is a page taken from `BufferPoolManager::new_page` by an operation that then fails: it stays allocated, zeroed, with no owner, as in the heap file. Pages taken from the free list are not lost, because the list is only changed when the header is committed.

## Errors

| `IndexError` | SQLSTATE | Example message |
|---|---|---|
| `DuplicateKey { key }` | `23505` | `duplicate key value (42) violates the unique index` (the key is cut at 64 characters, with `…`) |
| `KeyTooLarge { size, max }` | `54000` | `index key of 3000 bytes exceeds the maximum of 2031 bytes` |
| `KeyTypeMismatch { expected, found }` | `42804` | `an index key of type integer does not match the index type varchar` (a key of another type, or `open` naming another type) |
| `NullKey` | `XX000` | `NULL cannot be an index key` (programming error: the caller skips `NULL`) |
| `UnsupportedPageSize { size }` | `XX000` | `page size 64 is too small for a B+Tree` |
| `Corrupted { page_id, reason }` | `XX001` | `B+Tree page 7 is corrupted: keys are not in increasing order` |
| `BufferPool(..)` | `BufferPoolError::sqlstate()` | `checksum mismatch on page 7` (`XX001`), `no available frame` (`53000`) |

## Design decisions

See [ADR 0010](decisions/0010-b-plus-tree-variable-nodes-and-late-writes.md) for variable-size nodes, order-preserving keys and writing last. Also:

- **Unique keys only.** A non-unique index can later store `(key, Rid)` as its key; the "Unique Index" item of the roadmap then becomes a check before inserting.
- **No parent pointers on disk:** the descent keeps the path (like the *Context* of the current BusTub), so a split never rewrites the children that change parent.
- **`level` in each node**, instead of one page type for leaves and another for internal nodes: one type, and a cheap structural check on every descent.
- **Half a node in bytes as the minimum, with optional redistribution:** with entries of different sizes, an exact minimum fill cannot always be kept (a new separator may not fit the parent); the tree stays correct and only uses more pages.
- **A free list inside the tree**, instead of leaving pages freed by merges without an owner: inserting and removing in cycles would otherwise grow the tree forever.
- **A quarter of a node as the minimum, not half.** A split leaves two halves of about half a node each; with "half" as the minimum, the smaller half was already underfull, so a removal there merged the halves back and the next insert split them again (the root grew and shrank on every operation at 27 `BIGINT` keys). With a quarter, a node needs several removals before it merges, and redistribution still works: when two siblings do not fit one node, each half of the redistribution is more than half full.
- **The key type is recorded in the header**, with the type codes of the catalog repeated in `key.rs` (the `index` layer does not depend on `catalog`), so opening a tree with another type fails instead of comparing keys of different encodings.
- **`NULL` is not indexed.** PostgreSQL indexes `NULL` so that `IS NULL` can use an index; that waits for a planner.

## Tests

- **`key.rs`:** documented bytes of each type, `NullKey`, `KeyTypeMismatch` for six pairs of types, any text in a `VARCHAR(n)` tree, round trip of the extremes (`MIN`/`MAX`, `-1`/`0`/`1`, the smallest floats of each sign, the first and last date and timestamp, empty text, prefixes, multibyte), **byte order equal to `sort_cmp`** for every pair of extremes and for 2,000 random pairs of each type, and every corruption reason of `decode_key`. Mutations checked by hand: no sign flip on integers, negative doubles ordered like positive ones.
- **`node.rs`:** documented bytes of the header page, the free page, a leaf and an internal node; LSN kept and every other byte rewritten; round trip (empty, one entry, a key of 0 bytes and of `max_key_size`, internal nodes with no entries and several levels); a node exactly the size of the page fits and one byte more does not (and is never written); four entries of the largest key fit pages of 512, 4,096, 8,192 and 65,535 bytes; every corruption reason; 2,000 randomly damaged or truncated nodes without a panic.
- **`b_plus_tree.rs`:** a test-only `check_invariants` walks the whole tree from the header: keys of each subtree within their bounds, levels, the leaf chain in key order ending at `0`, no empty leaf other than the root, no page reached twice, the header pointing to the root, and the free list without repeats and disjoint from the tree. On top of it:
  - `create`/`open` (empty tree, reopening, a page that is not a header, pages too small for either, another key type, an unknown key type code), `get` through hand-built trees (keys of a root leaf, two levels, a child at the wrong level), every key check of `get`, `insert`, `remove` and `range`;
  - `insert`: order, duplicates (nothing changes; a long key shown cut at 64 characters), the key size limit and one byte more, no page left pinned; splits of the root leaf, of internal nodes (2,000 keys in ascending, descending and scattered order, invariants every 250 inserts) and by bytes with text keys of mixed sizes;
  - `range`: inclusive, exclusive and open bounds against a `BTreeSet`, empty and reversed ranges, text keys, bounds longer than the largest key, dropping the iterator midway, a damaged leaf, leaves linked out of key order;
  - `remove`: missing keys, the root leaf emptied and reused; redistribution of leaves with the right and the left sibling and of internal nodes; a redistribution skipped because its separator does not fit the parent; merges of leaves and of internal nodes up to the root shrinking; removing 2,000 keys back to a root leaf; alternating inserts and removes at a split point keep the same root and leaves;
  - the free list: a merged page reused by the next split, and three cycles of filling and emptying the tree taking no new page;
  - **`random_operations_with_bigint_keys_should_match_model`** and **`..._varchar_keys_...`**: 3,000 random inserts (with duplicates), removes, gets and ranges each against a `BTreeMap`, with keys up to the maximum size, `check_invariants` every 50 steps and no page pinned at the end;
  - failures: a split and a merge with too few frames fail before writing (the tree is the same after reopening with a larger pool); a fetch failing during the descent, while pinning, while reading the sibling or the header changes nothing; a damaged node is `XX001` for `get`, `insert` and `remove`; a free list pointing to a live page is detected before anything changes; a new root above the highest possible level is `Corrupted`, not an overflow;
  - every `IndexError` with message, SQLSTATE and `source()`.
  - Mutations checked by hand: the right leaf of a split linking to itself, the middle key copied instead of moved, a redistribution applied although its separator does not fit, a merged leaf keeping the left next, the root never shrinking, and pages written one by one instead of after all are encoded.
- **Integration** in [`tests/index_b_plus_tree.rs`](../tests/index_b_plus_tree.rs), with 8 KB pages and a 12-frame pool: 20,000 keys of about 200 bytes survive eviction and reopening (full scan and gets), half of them removed and the rest still there after another reopening; a damaged leaf and a damaged header on disk are caught by the checksum (`XX001`); three cycles of filling and emptying a tree leave the file the same size.

## Limitations

- ⚠️ **Not linked to tables yet:** no `CREATE INDEX`, no index metadata in the catalog and no maintenance on `TableHeap` changes.
- ⚠️ **Unique keys only**, of a single column. No composite keys, no `NULL`.
- ⚠️ **No crash atomicity:** the commit step copies several pages into the buffer pool, which writes them to disk in any order; a crash in between can leave the tree inconsistent on disk. The WAL will fix it.
- ⚠️ **Single-threaded:** no latches (no *latch crabbing*); every operation takes `&mut BufferPoolManager`.
- ⚠️ **Pages lost on a failed allocation:** pages taken from `new_page` by an operation that fails stay allocated and unowned until a future `VACUUM`.
- ⚠️ **One handle per tree:** the root is cached in the handle; a second handle on the same tree would not see a root change made through the first until it is opened again.
- **Node copies:** reads decode a whole node and writes rebuild it; a search over the slots without copying, suffix truncation of separators and prefix compression are possible optimizations.
