# Storage Engine

This document is the authoritative description of ferroDB's storage layer (`src/storage/` and `src/serialization.rs`): on-disk formats, invariants, error handling and tests. For how the layers fit together, see [architecture.md](architecture.md). For tuples' typed contents, schemas and the catalog, see [data-model.md](data-model.md).

All integers on disk are **little-endian** and there is no padding or alignment.

- [Disk Manager](#disk-manager)
- [Page and Buffer Pool](#page-and-buffer-pool)
- [LRU-K Replacer](#lru-k-replacer)
- [Common Page Header and Checksum](#common-page-header-and-checksum)
- [Serialization Codec](#serialization-codec)
- [Slotted Page](#slotted-page)
- [RID (Record ID)](#rid-record-id)
- [Tuple Storage](#tuple-storage)
- [Heap File](#heap-file)
- [Free Space Management](#free-space-management)
- [Historical Notes](#historical-notes)

---

## Disk Manager

`DiskManager` (`src/storage/disk_manager.rs`) reads and writes fixed-size pages in a single database file. It does not know what a page contains.

- **Addressing:** page `n` lives at byte offset `n × page_size` (computed with `checked_mul`; overflow → `OffsetOverflow`). The default page size is 8192 bytes (`config::PAGE_SIZE`).
- **`new(path, page_size)`:** opens or creates the file without truncating it. A page size of 0 → `InvalidPageSize`, before the file is created.
- **`read_page(id)`:**
  - a page past the end of the file has never been written, so it is returned as **all zeros**;
  - a page that starts inside the file but ends after its end is a **torn page** (`TornPage { page_id, file_len }`), for example a crash while the file was being extended;
  - otherwise reads exactly `page_size` bytes (`read_exact`).
- **`write_page(id, data)`:** `data.len()` must equal the page size (`InvalidPageData`, checked before anything is written). Writes and then calls `sync_data()`, so the call returns only after the operating system reports the data as durable.
- **`num_pages()`:** file length ÷ page size; a length that is not a multiple of the page size → `TornPage`.

Errors are returned as `DiskError` (`Io`, `InvalidPageSize`, `InvalidPageData`, `OffsetOverflow`, `TornPage`); the module never panics on I/O.

**Limits.** A torn page *at the end* of the file is detected by its size. A page that is partially overwritten *in place* keeps the file size intact; that case is detected one layer up by the page checksum (see [below](#common-page-header-and-checksum)).

**Tests** (unit, in the module): write/read round trip, persistence after reopening the file, pages not overwriting each other, reading a never-written page, zero page size without creating a file, wrong data length without writing, torn file in `read_page` and `num_pages`, sparse writes (`num_pages` after writing page 2 only), missing parent directory, offset overflow.

---

## Page and Buffer Pool

### Page (frame)

`Page` (`src/storage/page.rs`) is one **frame** of the buffer pool: the page bytes plus in-memory metadata that never reaches the disk.

```rust
pub struct Page {
    page_id: Option<u32>, // None = free frame
    data: Vec<u8>,        // the page bytes (page_size)
    pin_count: u32,       // > 0 = in use, cannot be evicted
    is_dirty: bool,       // must be written before the frame is reused
}
```

`Page::new` rejects a page size smaller than the common header (16 bytes). `load` replaces the data (same size) and starts with `pin_count = 1`. `try_unpin` returns `false` when the pin count is already 0.

### Buffer Pool Manager

`BufferPoolManager` (`src/storage/buffer_pool.rs`) caches pages in a fixed number of frames and is the only component that talks to the `DiskManager`.

```rust
pub struct BufferPoolManager {
    pages: Vec<Page>,                // the frames
    page_table: HashMap<u32, usize>, // page id → frame id
    free_list: Vec<usize>,           // frames that hold no page
    replacer: LRUKReplacer,          // chooses the victim when no frame is free (K = 2)
    disk_manager: DiskManager,
    page_size: usize,
    next_page_id: u64,
}
```

**Operations**

- **`fetch_page(id)`:**
  - page in the `page_table` → pin it, record the access, return it (no I/O);
  - otherwise read it from disk, **verify the checksum** (`ChecksumMismatch`) and **validate the header** (`Page(..)` for an unknown type or version) *before* touching any frame; then install it in a free frame or in the victim chosen by the replacer (`NoAvailableFrame` if every frame is pinned). A dirty victim is written back first.
- **`unpin_page(id, is_dirty)`:** decrements the pin count and ORs the dirty flag; when the count reaches 0 the frame becomes evictable. Unpinning a page that is not resident, or whose count is already 0, returns `false`.
- **`new_page()`:** see [page allocation](#page-allocation).
- **`flush_page(id)`:** writes the page if it is dirty and marks it clean.
- **Write-back:** every write stamps the checksum first (`checksum::stamp_page`) and then calls `DiskManager::write_page`.
- **`Drop`:** flushes every resident dirty page when the pool is dropped.

**Fail without side effects.** Installing a page only *peeks* at the frame it will use (`free_list.last()`, `replacer.select_victim()`); the free list, the replacer and the page table are updated only after the read, the checksum, the header check, the write-back and the load have all succeeded. A failure at any step leaves the pool unchanged and usable.

**Errors:** `BufferPoolError` (`InvalidPoolSize`, `InvalidPageSize`, `NoAvailableFrame`, `PageIdOverflow`, `ChecksumMismatch { page_id }`, `Disk(..)`, `Page(..)`). `BufferPoolError::sqlstate()` gives the code the upper layers report, the same in every layer: damaged bytes (`ChecksumMismatch`, `Page(..)`, a torn file) are `XX001`, other disk errors `58030`, `NoAvailableFrame` `53000`, `PageIdOverflow` `54000`, an invalid configuration `XX000`. `HeapFileError::sqlstate()` builds on it (`TupleTooLarge` `54000`, a damaged page list or page `XX001`, a `Rid` outside the heap `XX000`).

**Not supported yet:** concurrent access (every operation takes `&mut self`), and RAII pin guards (callers must pair every fetch with an unpin; the heap file does this through helpers that unpin on every path).

**Tests** (unit, in the module): constructor validation without creating a file, fetch miss and hit, pin counting, unpin rules (unknown page, count already 0), pinned pages never evicted, `NoAvailableFrame` with every frame pinned, LRU-K victim choice, dirty pages written on eviction and on drop, an unpin without the dirty flag not persisted, a failed read keeping the free list usable, checksum stamping and every corruption case (data byte, header byte, checksum field, unstamped page), unknown type and other format version rejected without taking a frame, and the page allocation rules (see [below](#page-allocation)). `flush_page` has no test of its own, but several of these tests call it to put a page on disk before inspecting the file. A test-only failure hook (`fail_fetch_after`) lets higher layers inject a failure in the middle of an operation.

---

## LRU-K Replacer

`LRUKReplacer` (`src/storage/replacer.rs`) decides which unpinned frame is evicted. The buffer pool uses **K = 2**.

```rust
pub struct LRUKReplacer {
    k: usize,
    current_timestamp: usize,
    evictable: HashSet<usize>,
    history: HashMap<usize, VecDeque<usize>>, // last K access timestamps per frame
}
```

**Victim selection**

1. Only frames that are evictable (pin count 0) and have an access history are candidates.
2. Frames with **fewer than K accesses** have infinite backward K-distance and are evicted first; among them, the one whose oldest recorded access is earliest.
3. Otherwise, the frame with the largest K-distance (`current_timestamp − K-th most recent access`).
4. Ties are broken by the lower frame id.

`select_victim` only reports the candidate; `forget` removes a frame's history once the buffer pool has actually reused it. With `K = 1` the policy degrades to plain LRU (covered by a test). `new(0)` → `ReplacerError::InvalidK`.

Victim selection scans every candidate frame (O(frames)), which is fine for the small pools used today.

**Tests** (unit, in the module): history capped at K, pinned frames ignored, fewer-than-K first, K-distance ordering, K = 1 behaves as LRU, cold page with older first access, timestamp tie broken by frame id, `select_victim` does not remove, removed frame is not returned again, no evictable frame, unpin without access, pin/unpin state.

---

## Common Page Header and Checksum

Every page starts with a 16-byte header understood by the buffer pool, independently of the page type (`PageHeader`, `src/storage/page.rs`):

| Bytes | Field | Use |
|---|---|---|
| `0..8` | LSN (`u64`) | reserved for the WAL / recovery; preserved, not assigned yet |
| `8..12` | checksum (`u32`) | CRC32 of the page computed with this field set to zero |
| `12` | page_type (`u8`) | `0` Free (never formatted), `1` Slotted, `2` DatabaseHeader (page 0 only, see [catalog](data-model.md#catalog)), `3` BPlusTreeHeader, `4` BPlusTreeNode, `5` BPlusTreeFree (see [indexes](indexes.md#page-formats)) |
| `13` | version (`u8`) | `PAGE_FORMAT_VERSION` (currently `1`) |
| `14..16` | reserved | zero |

```rust
pub enum PageType { Free = 0, Slotted = 1, DatabaseHeader = 2, BPlusTreeHeader = 3, BPlusTreeNode = 4, BPlusTreeFree = 5 }

pub struct PageHeader { pub lsn: u64, pub checksum: u32, pub page_type: PageType, pub version: u8 }
```

- **Write:** before writing to disk, the buffer pool computes the CRC32 (IEEE 802.3, the zlib/PNG variant, `src/storage/checksum.rs`) and stores it in the header (`checksum::stamp_page`).
- **Read:** when fetching from disk, the buffer pool recomputes the CRC32. A mismatch → `BufferPoolError::ChecksumMismatch`. Then the header is validated: unknown type or different version → `BufferPoolError::Page(..)`. In both cases no frame is used.
- An **all-zero** page is accepted without a checksum: it is a new page (`PageType::Free`) that was never written.
- `PageHeader::read(page)`: decodes and validates. Unknown type → `PageError::Serialization(InvalidValue { field: "page_type", .. })`; formatted page with a version ≠ `PAGE_FORMAT_VERSION` → `PageError::UnsupportedVersion`. A `Free` page has no version check.
- `PageHeader::format(page, page_type)`: writes the type, the current version and zeroes the reserved bytes, **keeping the LSN and the checksum**.
- `SlottedPage::init` calls `format(.., Slotted)`. `SlottedPage::new` rejects a `Free` page (`Uninitialized`), an invalid header (`SlottedPageError::InvalidHeader`) and a page of another formatted type (`DatabaseHeader` or a B+Tree page → `WrongPageType`). The `match` on `PageType` is exhaustive, so every new page type forces that decision.

The checksum detects accidental corruption (bad sectors, partial writes). It is not a security mechanism: anyone who can modify the file can recompute it.

**Design decisions:** see [ADR 0002](decisions/0002-page-header-and-checksum.md).

**Tests**

- Unit in `checksum`: empty input, the standard vector (`crc32("123456789") = 0xCBF43926`), single bit flip, checksum field ignored, stamped page verifies, modified page fails, zero page verifies, unstamped non-zero page fails.
- Unit in `page`, `rid`, `slotted_page` and `buffer_pool` for header, types, versions and rejection without occupying a frame.
- Integration in [`tests/storage_page_serialization.rs`](../tests/storage_page_serialization.rs): header and LSN survive eviction and reopening the file, unknown type / future version on disk are rejected, the pool remains usable, and corruption of the type byte is caught first by the checksum.

---

## Serialization Codec

Every struct ⇄ bytes conversion goes through `src/serialization.rs`: fixed layout, **little-endian**, no padding, and no dependence on Rust's in-memory representation (no `transmute` / `repr(C)` over the bytes).

- `ByteReader` / `ByteWriter`: cursors with `read_u8/u16/u32/u64`, `read_bytes`, `read_array::<N>`, `skip`, `write_u8/u16/u32/u64`, `write_bytes`, `write_zeros`.
- **Never panic:** missing bytes → `UnexpectedEof { needed, remaining }`; not enough room → `BufferOverflow { needed, remaining }`.
- **Atomic per call:** an error neither advances the cursor nor writes part of the bytes.
- Traits `Encode` (with `ENCODED_SIZE`) and `Decode` for fixed-size structures, implemented by `Rid` (6 bytes) and `PageHeader` (16 bytes). `decode` reads the whole block at once, so it is atomic too.
- Helpers `get_u16/put_u16/get_u32/put_u32/get_u64/put_u64` (`pub(crate)`) for offset-based access in already validated code (slotted page slots, LSN, checksum), without paying for a `Result` on the hot path.
- No serialization dependencies (`serde`, `bincode`): the format is fully controlled and documented here.

**Tests:** unit tests for every width, boundaries (empty buffer, exact size, one byte short, `usize::MAX`), error atomicity, and two randomized sequences (1,000 cases each) of typed fields written and read back.

---

## Slotted Page

Layout of a slotted page (8 KB by default, `src/storage/slotted_page.rs`):

```
 0        8        12   13   14    16       18        20        24
 ┌────────┬────────┬────┬────┬─────┬────────┬─────────┬─────────┬─────────────────┐
 │  LSN   │checksum│type│ver │rsv  │slot_cnt│free_end │next_pg  │ slot[0] ...     │ → grows →
 │  u64   │  u32   │ u8 │ u8 │ 2 B │  u16   │  u16    │  u32    │(off u16,len u16)│
 ├────────┴────────┴────┴────┴─────┴────────┴─────────┴─────────┴─────────────────┤
 │                     contiguous free space                                       │
 ├─────────────────────────────────────────────────────────────────────────────────┤
 │              ... tuple 1 | tuple 0                                              │ ← grows ←
 └─────────────────────────────────────────────────────────────────────────────────┘
```

- The slot array grows forward; tuples grow backward from the end of the page.
- The end of the slot array is not stored: `24 + slot_count × 4`.
- `offset == 0` marks a **deleted slot** (tombstone); 0 can never be a real tuple offset because the header is there. An empty tuple (`len == 0`) is valid.
- Largest tuple in 8 KB: `8192 − 24 − 4 = 8164` bytes (`max_tuple_size_for`).
- Offsets are `u16`, so a page can be at most 65,535 bytes (`config::MAX_PAGE_SIZE`, checked at compile time for `PAGE_SIZE`).
- `next_pg` (bytes `20..24`) is the next page of the [heap file](#heap-file); `0` = end of the list.
- Opening a page (`SlottedPage::new`) validates the header and every slot against the page bounds; data from disk is never trusted (`Corrupted { reason }`).

**Operations**

- **insert:** reuses the first deleted slot (if any) or creates a new one (+4 bytes). If the contiguous free space is not enough but the total is, it compacts first. If not even the total is enough → `PageFull`. Tuples larger than the page allows → `TupleTooLarge`.
- **delete:** only marks the slot as a tombstone. The bytes become a hole (lazy compaction).
- **update:**
  - same size or smaller → overwrites in place;
  - larger → uses the free space plus the old version's space, compacting if needed, **keeping the slot id**;
  - does not fit → `PageFull` and the page is left intact. Moving the tuple to another page is the heap file's job, which returns a new `Rid` (see [Heap File](#heap-file)).
- **compact:** moves live tuples to the end of the page (highest offset first, so nothing is overwritten before it is moved) and updates only the slot offsets.
- **`largest_insertable()`:** largest tuple an insert would accept right now (see [Free Space Management](#free-space-management)).

`SlottedPage<B>` is a zero-copy view over the page bytes: `SlottedPage<&[u8]>` only reads, `SlottedPage<&mut [u8]>` also writes. Every operation checks space before changing any byte.

**Design decisions:** see [ADR 0001](decisions/0001-slotted-pages-with-stable-slot-ids.md).

**Tests**

- Unit tests in `slotted_page`, plus `random_operations_should_match_model`: 10,000 random operations compared with a `HashMap`, validating the invariants at every step.
- Integration in [`tests/storage_slotted_page.rs`](../tests/storage_slotted_page.rs): tuples survive eviction and reopening the file, and on-disk corruption is detected by the checksum.

---

## RID (Record ID)

`Rid { page_id: u32, slot_id: u16 }` (`src/storage/rid.rs`) is the physical address of a tuple. It is serialized in 6 bytes (little-endian) and is intended to be the value stored in B+Tree leaves.

The most important rule of the design: **a tuple's `slot_id` never changes while the tuple lives on its page**, so the RID stays valid when the bytes are moved by compaction. An update that does not fit in the page moves the tuple and returns a new RID (see [Heap File](#heap-file)).

---

## Tuple Storage

Defines how a row with N columns becomes the bytes stored in a slot and how it is read back (`src/storage/tuple.rs`). The format **does not know SQL types**: each field is `&[u8]` and a column only states its physical width (`Fixed(n)` or `Variable`). The `types` layer converts values ⇄ bytes on top of it, keeping the dependency direction downward (`storage` imports nothing from the upper layers).

Reference: CMU 15-445 ("Database Storage II") and PostgreSQL's `HeapTupleHeaderData`: a header with a NULL bitmap, fixed-width columns inline and variable-width columns pointing into a data area.

**Layout** (little-endian, no alignment)

| Offset | Size | Field |
|---|---|---|
| `0` | 2 | `column_count: u16` (must match the layout) |
| `2` | `ceil(n/8)` | NULL bitmap: bit `i % 8` of byte `i / 8` set → column `i` is `NULL` |
| `2 + ceil(n/8)` | Σ widths | fixed part, in column order: `Fixed(w)` → `w` bytes; `Variable` → `(offset u16, len u16)` |
| `data_start` | rest | bytes of the variable columns, in column order |

Example: layout `[Fixed(4), Variable, Fixed(1)]`, values `(42, "ab", NULL)`:

```
00: 03 00             column_count = 3
02: 04                bitmap: column 2 is NULL (bit 2)
03: 2A 00 00 00       col 0 = 42
07: 0C 00 02 00       col 1 → offset 12, len 2
0B: 00                col 2 = NULL (zeroed)
0C: 61 62             "ab"
```

**Operations**

- **`TupleLayout::new(columns)`:** computes the fixed-part offsets once per table. Errors: more than 1,600 columns (`TooManyColumns`, the same limit as PostgreSQL), `Fixed(0)` (`ZeroWidthColumn`) and a fixed part larger than a page (`TupleTooLarge`).
- **`layout.encode(&[Option<&[u8]>])`:** `None` = `NULL`. Validates everything before allocating: number of fields (`ColumnCountMismatch`), width of fixed fields (`FixedWidthMismatch`) and total size ≤ 65,535 (`TupleTooLarge`). Writes sequentially with the `ByteWriter`.
- **`TupleRef::new(&layout, bytes)`:** zero-copy view. Because the bytes come from disk, it validates **once** and only accepts the canonical form that `encode` produces: correct `column_count`, minimum size, no padding bits in the bitmap, a `NULL` fixed column zeroed, variable fields contiguous and in column order (a `NULL` variable field has `len == 0`), and no trailing bytes. Every row therefore has exactly one byte representation. After that, `field(i)`, `is_null(i)` and `fields()` can only fail with `ColumnOutOfRange`.

**Design decisions**

- **Type-agnostic format in `storage`:** could be built before data types and keeps the layers independent.
- **A `NULL` fixed column still takes space** (PostgreSQL omits it): constant offsets → O(1) access without walking the bitmap.
- **`(offset, len)` per variable column:** direct access to any column and simple validation; 2 bytes more than storing only the end.
- **`column_count` stored in the tuple:** detects reading with the wrong layout and prepares for `ADD COLUMN` without rewriting the table (like PostgreSQL's `t_infomask2`).
- **No alignment:** `from_le_bytes` does not require it; smaller tuples.
- **`Some(&[])` ≠ `None`:** empty string and `NULL` are different, as in the SQL standard.
- **`encode` returns `Vec<u8>`:** the slotted page keeps receiving `&[u8]` and managing space; tuple and page do not know each other.
- **Two size limits:** the format accepts up to 65,535 bytes (`MAX_TUPLE_SIZE`, `u16` offsets), but the page is stricter: `SlottedPage::max_tuple_size` (8,164 bytes in 8 KB) is checked on insert/update, which return `SlottedPageError::TupleTooLarge`.
- ⚠️ **The tuple format has no version of its own:** it is slot content, so `PAGE_FORMAT_VERSION` did not change. If the tuple layout changes, it will need one.

**Tests**

- Unit in `tuple`: layout offsets (0, 1, 8, 9, 1,600, 1,601 columns; `Fixed(0)`; fixed part at and above the limit), exact `encode` bytes, empty vs. `NULL`, every error and every `Corrupted` reason.
- `random_tuples_should_round_trip`: 1,000 random layouts and rows compared with the input.
- `random_corruption_should_never_panic`: 5,000 tuples with flipped bits or truncated; `TupleRef::new` never panics and, if it accepts, every `field` works (mutation-checked by hand during development).
- Integration in [`tests/storage_tuple.rs`](../tests/storage_tuple.rs): tuples in a slotted page survive eviction and reopening the file, an update with a larger field keeps the `slot_id`, a full page → `PageFull` with every row still readable, a tuple larger than the page → `TupleTooLarge`, wrong layout → `ColumnCountMismatch`.

---

## Heap File

`HeapFile` (`src/storage/heap_file.rs`) stores a table's tuples, unordered, in a **linked list of slotted pages**. The whole database is a single file, so a table's pages are scattered and each one points to the next through the `next_page_id` field of the slotted page header.

```
first_page_id
     │
     ▼
 ┌────────┐  next  ┌────────┐  next  ┌────────┐  next = 0
 │ page 1 │ ─────▶ │ page 4 │ ─────▶ │ page 7 │ ─────▶ end
 └────────┘        └────────┘        └────────┘
                                     ▲ new pages are appended here
```

A heap is identified by its first page. The catalog stores the `first_page_id` of each table (see [catalog](data-model.md#catalog)).

### Page allocation

- `BufferPoolManager::new_page()` returns the next free id, with the page zeroed, pinned and **already dirty**: the id reaches the disk even if the caller writes nothing, so it is never handed out twice.
- The next id is `max(counter, pages on disk, highest id in the page table + 1)`, recomputed on every call: it respects pages created with `fetch_page` past the end of the file that have not been written yet.
- **Page 0 is reserved** (`RESERVED_PAGE_ID`) for the database header, which points to the catalog. That is why `0` can mean "no next page".
- Ids exhausted → `BufferPoolError::PageIdOverflow` (the counter is a `u64` so it does not overflow when handing out `u32::MAX`).

### Operations

- **create:** checks that the pool's page size works for slotted pages (`UnsupportedPageSize`, without consuming an id) and allocates and formats the first page. An empty heap always has one page.
- **open:** walks the list on disk and keeps the ids in memory. Rejects a cycle (`CorruptedChain`), a page that is not slotted (`Page { .. }`, for example a pointer past the end of the file) and starting at page 0.
- **insert:** a tuple larger than an empty page → `TupleTooLarge` without reading or allocating anything. Otherwise asks the **free space map** for the first page (in list order) that accepts the tuple and inserts there. If no known page fits, it follows the `next_page_id` of the last page: if another handle of the same heap already appended pages, it adopts them and tries again. Only then does it allocate a new page, write the tuple into it and link the previous page to it. It never overwrites an existing `next_page_id` (that would orphan another handle's page).
- **get:** checks that the `Rid` belongs to a page of this heap (`PageNotInHeap`) and copies the bytes. Missing or deleted slot → `TupleNotFound`.
- **update:** fits in the page → same `Rid`. Does not fit → inserts the new version (may allocate a page), **then** deletes the old one and returns the new `Rid`. If the insert fails, the old version is still there; if deleting the old version fails, the new one is deleted before returning the error, so two live versions are not left behind.
- **delete:** tombstone in the slot; the space returns to the page and the free space map records it, so the next insert that fits reuses the hole.
- **scan:** an `Iterator` of `(Rid, Vec<u8>)` in page → slot order. Each page is pinned once: live tuples are copied and the page is unpinned before the first one is returned.
- **next_after(after):** the first live tuple after a `Rid` (or the first of the heap), in `scan` order: a cursor that holds no borrow and no pin between calls, for executors that read one row at a time. `after` may be a tuple deleted since; a `Rid` of another heap is `PageNotInHeap`.

**Design decisions:** see [ADR 0004](decisions/0004-heap-file-as-linked-list.md). In short:

- **Linked list in a reserved field** (like BusTub's `TableHeap`), without changing the page format.
- **An update that does not fit changes the `Rid`** instead of leaving a forwarding pointer; the signature says so: `update(..) -> Result<Rid, _>`.
- **`Option<NonZeroU32>` for `next_page_id`:** the type states that 0 is not a valid next page.
- **The `HeapFile` does not own the buffer pool:** every method takes `&mut BufferPoolManager`, so several tables share one pool without `Rc<RefCell<_>>`.
- **Pins are always returned:** every access goes through `read_page`/`write_page` helpers that unpin on every path (including errors) and only mark the page dirty if the write succeeded.
- **Page list in memory**, with a `HashMap` from page id to position: validating that a `Rid` belongs to the heap (and detecting cycles on `open`) is O(1). It costs a few bytes per page and an `open` that reads every page.

**Known limitations**

- ⚠️ **Orphan page:** an I/O error between allocating the new page and writing the pointer in the previous one leaves an unreachable page (lost space, no corrupted data). Fixing this is the WAL's job.
- ⚠️ **An update that moves is not atomic:** undo on error is best effort (if the second delete also fails, both versions remain). Real atomicity comes with the WAL.
- ⚠️ **Several handles of the same heap:** `insert` does not lose data, but `get`/`scan` of one handle only see pages appended by another after its own next `insert` or a new `open`. The catalog keeps one handle per table (see [Table Heap](data-model.md#table-heap)).
- ⚠️ **`get`/`scan` copy the bytes:** without a pin guard in the buffer pool, returning `&[u8]` would require keeping the page pinned between calls.

**Tests**

- Unit in `buffer_pool` (`new_page`: page 0 skipped, increasing ids, continues after the disk and after in-memory pages, dirty even after a clean unpin, `NoAvailableFrame` does not consume an id, `PageIdOverflow`) and in `slotted_page` (`next_page_id`).
- Unit in `heap_file`: every operation, boundaries (empty, maximum and maximum + 1 tuple), every error variant, update that moves (including allocating another page), scan with deletes and after moves, scan abandoned midway, list with a cycle / pointing to itself / past the end of the file / to page 0, insert through a stale handle, unsupported page size, insert without a free frame. Every test checks that no page was left pinned.
- Failures in the middle of an operation use the buffer pool test hook (`fail_fetch_after`, `#[cfg(test)]` only): an update whose delete fails does not leave two versions, and the scan stops after a page error.
- `random_operations_should_match_model`: 5,000 operations with a 3-frame pool against a `HashMap<Rid, Vec<u8>>`, checking the scan for duplicates every 500 steps, pins and the free space map every 250, and, by sampling, that no page was allocated while another one could take the tuple; reopens at the end. Mutation-checked by hand during development: forgetting the new page, not marking it dirty or ignoring the map makes it fail.
- Integration in [`tests/storage_heap_file.rs`](../tests/storage_heap_file.rs): eviction, reopening the file, two heaps in the same file, new pages after reopening do not reuse pages of the heap, corruption in a middle page detected by the checksum on `open`.

---

## Free Space Management

The `HeapFile` knows how much fits in **each** of its pages and inserts into the **first page of the list where the tuple fits**. It reuses space from deletes and updates in old pages, including pages that became empty; a new page is only allocated when no known page accepts the tuple.

The free space map (`FreeSpaceMap`, `src/storage/free_space_map.rs`) lives **in memory**: `open` already reads every page to build the list, and the map comes from the same pass. The on-disk format did not change.

### Stored value

For each page, at the same position as in `page_ids`, a `u16` that is an **exclusive bound**: tuples of `0..value` bytes fit.

| Page | `largest_insertable()` | Value in the map |
|---|---|---|
| empty (8 KB) | `Some(8164)` | 8165 |
| only the 4 bytes of a new slot left | `Some(0)` (an empty tuple fits) | 1 |
| full | `None` (not even an empty tuple fits) | 0 |

`SlottedPage::largest_insertable()` subtracts the 4 bytes of a new slot unless a deleted slot can be reused, and never exceeds `max_tuple_size`. It holds that `can_insert(n) ⇔ largest_insertable() >= Some(n)`.

Storing only "the largest tuple that fits" does not work: `0` would mean both "an empty tuple fits" and "nothing fits", and inserting an empty tuple would find the full page again forever. The exclusive bound separates the two cases.

### Max tree

```
                 tree[1] = 80                 root: largest value of all
          ┌──────────┴──────────┐
     tree[2] = 50          tree[3] = 80
      ┌─────┴─────┐         ┌─────┴─────┐
   10 (page 0) 50 (page 1) 20 (page 2) 80 (page 3)    leaves, in list order
```

- `find(len)`: descends from the root going **left** whenever the left child's value is `> len` → returns the first page that fits in O(log n). Example: `find(30)` → page 1.
- `set(i, value)`: changes the leaf and recomputes its ancestors, O(log n).
- `push(value)`: new page at the end; when leaves run out, capacity doubles (O(n) rebuild, amortized).
- Leaves beyond the number of pages are 0 (= full), so they are never chosen.

### How the heap uses the map

- **insert:** tuple > `max_tuple_size` → `TupleTooLarge` without reading a page. Otherwise `find(len)` → try that page. If it does not accept the tuple (stale map), record the real value (which now rejects `len`) and search again; each page fails at most once, so the loop terminates. No page in the map → adopt pages appended by another handle → only then allocate.
- **delete / update / insert:** the write returns the page's new value, read while the page is still pinned, and the map is only updated **if the write succeeded**. An attempt that does not change the page (insert into a full page, update that does not fit in place) does not mark it dirty, so it costs no disk write.
- **open:** every page read while following the list enters the map with its real value.

**Design decisions:** see [ADR 0005](decisions/0005-in-memory-free-space-map.md).

**Known limitations**

- ⚠️ **Several handles:** each handle's map is a **hint**. A value that is too high becomes a failed attempt that corrects the map; a value that is too low only wastes that page until the next `open`. No data is lost.
- ⚠️ **An emptied page does not accept the maximum tuple again:** deleted slots stay in the array (slot ids are stable), and each one beyond the reused slot still takes 4 bytes.
- ⚠️ **Cost per write:** `largest_insertable` is O(slots) (sum of live bytes and search for a free slot), the same order as the slotted page operations.

**Tests**

- Unit in `slotted_page`: `largest_insertable` on an empty page, with tuples, with a reusable slot, with a hole, with room only for the slot (`Some(0)`), full and header-sized (`None`), and 5,000 random states compared with `can_insert`.
- Unit in `free_space_map`: empty map, first page that fits, boundaries (`find(0)` skipping full pages, `u16::MAX`), `set` up and down, index error, padding leaves, growth by powers of two up to 1,000 pages, and 10,000 operations against a linear search.
- Unit in `heap_file`: every test checks pins **and** the exact map (`assert_heap_consistent`); hole reuse, first fit, no allocation while space is left, emptied page, empty tuple in a full page and in a page with room only for the slot, large tuple without reading a page, failed write does not change the map, stale map between two handles (mutation-checked by hand: without correcting the map on failure, the insert does not terminate).
- The heap's `random_operations_should_match_model` checks the map against the pages every 250 steps and, by sampling, that no page was allocated while another accepted the tuple (mutation-checked by hand: ignoring the map fails at step 0).
- Integration in [`tests/storage_heap_file.rs`](../tests/storage_heap_file.rs): space freed before reopening the file is reused, reuse with a 2-frame pool (eviction), an emptied heap refills without growing.

---

## Historical Notes

These notes explain why some details look the way they do. They are not current behavior.

- **First buffer pool (April 2026):** the initial version lived in `src/main.rs` with a plain LRU replacer (`VecDeque`) and `expect` on every I/O call. LRU-K and the error types came later; when I/O errors stopped panicking, the order of operations in `fetch_page` was changed so that a failed read could not leave a frame outside the free list, the replacer and the page table.
- **First disk manager:** used `flush()` (which does nothing for a `std::fs::File`) instead of `sync_data()`, ignored short reads and did not validate the data length. The current version is described [above](#disk-manager).
- **12-byte page header:** the first common header had only the LSN and the checksum. It grew to 16 bytes to add the page type and the format version; files created with the old header no longer open (no migration; delete the old `data.db`).
- **Slotted page byte `20..24`:** was reserved before becoming `next_page_id`. Old pages already had `0` there, so the format version did not change.
- **Old demo files:** a `data.db` from the demo that stored a slotted page in page 0 now fails with `NotADatabase`, and the demo tells the user to delete it.
