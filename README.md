<p align="center">
  <img src="ferrodb.png" alt="ferroDB logo" width="180">
</p>

# ferroDB

[![CI](https://github.com/RodrigoGrz/ferro-db/actions/workflows/ci.yml/badge.svg)](https://github.com/RodrigoGrz/ferro-db/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

An educational relational database engine, built from scratch in Rust and inspired by the [CMU Database Group](https://db.cs.cmu.edu/)'s Database Systems course (15-445/645).

ferroDB exists so I can understand how a relational database works by implementing its internals myself: how pages reach the disk, how a buffer pool decides what stays in memory, how rows of different sizes share a page, and, later, how queries, indexes and transactions are built on top of that. The long-term goal is a small but real SQL database that a simple application can connect to.

It is **not** meant to compete with PostgreSQL, MySQL or SQLite, and it is **not production-ready**.

**Current status:** the storage engine, a typed table layer and a persistent catalog work and are tested (790 tests). Typed rows can be stored in tables that survive restarts, through the Rust API. There is no SQL interface yet. See the [roadmap](ROADMAP.md) for the full checklist.

## What works today

| Component | What it does |
|---|---|
| **Disk Manager** | Reads and writes fixed-size 8 KB pages in a single file, syncs every write to disk (`sync_data`), detects torn files. |
| **Buffer Pool Manager** | Caches pages in a fixed number of frames, with pin counts, dirty tracking, write-back, page allocation, and checksum and header validation on every read from disk. |
| **LRU-K replacer** | Chooses which unpinned page to evict (K = 2). |
| **Page format** | A 16-byte header on every page: page type, format version, CRC32 checksum and a reserved LSN; explicit little-endian serialization (the crate has no `unsafe` code). |
| **Slotted pages and RID** | Variable-size tuples inside a page behind stable slot ids, with tombstones and lazy compaction. A `Rid` (page + slot) addresses any row. |
| **Tuple format** | NULL bitmap, fixed and variable-width fields, strict validation of bytes read from disk. |
| **Heap File + free space map** | A table's rows across a linked list of pages, first-fit placement in O(log n), moves rows that outgrow their page. |
| **Data types** | `SMALLINT`, `INTEGER`, `BIGINT`, `REAL`, `DOUBLE PRECISION`, `VARCHAR(n)`, `BOOLEAN`, `DATE`, `TIMESTAMP` and `NULL`, with SQL semantics: parsing, casts and store assignment, overflow-checked arithmetic, `||`, comparisons as SQL values, three-valued `AND`/`OR`/`NOT` and PostgreSQL-compatible SQLSTATE codes. |
| **Schema** | Column definitions, `NOT NULL`, and typed row ⇄ tuple conversion with SQL assignment rules. |
| **Table Heap** | A table's rows as typed values: insert, get, update, delete and scan, validated by the schema before anything is written. |
| **Catalog** | Tables and columns persisted on disk and bootstrapped from page 0, loaded into memory on open; finds a table's rows by name, keeping one open handle per table. |

**Partially there:**

- Typed rows can be inserted, updated, deleted and scanned through the Rust API (`TableHeap`), but there are no `INSERT`/`SELECT`/`UPDATE`/`DELETE` statements yet.
- `NOT NULL` is enforced per column by the schema, but there is no DDL to declare it.
- Every page reserves an LSN for write-ahead logging, but there is no WAL yet.

**Planned (not implemented):** SQL lexer and parser, query planning and execution, B+Tree indexes, more data types (`NUMERIC`), constraints, transactions and concurrency control, WAL and crash recovery, a client/server interface. The [roadmap](ROADMAP.md) has the complete list in order.

## Architecture

```mermaid
flowchart TD
    app["Rust API / demo binary"]
    catalog["Catalog<br/>tables and columns, page 0"]
    tableheap["Table Heap<br/>typed rows of a table"]
    schema["Schema + types<br/>row ⇄ tuple, SQL semantics"]
    heap["Heap File + free space map"]
    page["Slotted pages + tuple format"]
    bpm["Buffer Pool Manager + LRU-K"]
    disk["Disk Manager"]
    file[("Database file")]

    app --> catalog
    app --> tableheap
    catalog --> tableheap
    catalog --> heap
    tableheap --> schema
    tableheap --> heap
    schema --> page
    heap --> page
    heap --> bpm
    catalog --> bpm
    bpm --> disk
    disk --> file
```

Every component above exists today. Dependencies only point downward (`catalog` → `table` → `types` → `storage`). [docs/architecture.md](docs/architecture.md) has the detailed diagram, including the planned layers, and explains how a row travels from a `Value` to a page on disk.

## Quick start

Requires Rust 1.98.1 (see [`.tool-versions`](.tool-versions)); no other dependencies.

```sh
git clone https://github.com/RodrigoGrz/ferro-db.git
cd ferro-db
cargo build
cargo test
```

`cargo run` runs a small end-to-end demo ([`src/main.rs`](src/main.rs)). It is a fixed program, not an interactive SQL shell:

1. Opens `data.db` in the **current directory**, creating the database if the file does not exist.
2. Creates the table `accounts (id INTEGER NOT NULL, age SMALLINT, balance BIGINT)` on the first run.
3. Inserts **one new row on every run** (every third row has a `NULL` age).
4. Prints the catalog and every row with its `Rid` as `(page, slot)`.

Because the demo adds a row each time and writes to the current directory, run it from an empty directory to reproduce the output below:

```sh
cargo build
mkdir -p /tmp/ferro-demo && cd /tmp/ferro-demo
/path/to/ferro-db/target/debug/ferro-db   # run it three times
```

Output captured from three consecutive runs in an empty directory (each run is a separate process, so the rows come back from disk):

```text
$ ferro-db
tables:
  accounts (id INTEGER NOT NULL, age SMALLINT, balance BIGINT)
rows of accounts:
  (3, 0) -> (1, 19, 1000)

$ ferro-db
tables:
  accounts (id INTEGER NOT NULL, age SMALLINT, balance BIGINT)
rows of accounts:
  (3, 0) -> (1, 19, 1000)
  (3, 1) -> (2, 20, 2000)

$ ferro-db
tables:
  accounts (id INTEGER NOT NULL, age SMALLINT, balance BIGINT)
rows of accounts:
  (3, 0) -> (1, 19, 1000)
  (3, 1) -> (2, 20, 2000)
  (3, 2) -> (3, NULL, 3000)
```

Page 0 holds the database header and pages 1–2 the catalog heaps, so the table's rows start on page 3. If an older `data.db` from a previous on-disk format is found, the demo stops with a message asking you to delete it.

## Testing

```sh
cargo test --all-targets   # 728 unit tests + 62 integration tests
```

The suite is the main way the implementation is verified:

- **Unit tests** in every module, covering boundaries and every error variant.
- **Integration tests** in [`tests/`](tests/), one file per component: data survives eviction with tiny buffer pools and reopening the file.
- **Corruption tests:** bytes changed on disk are detected by checksums and validation, never by a panic.
- **Randomized model tests:** thousands of random operations compared against a simple in-memory model (`HashMap`, `Vec`, `i128` arithmetic), with invariants checked along the way.
- **Injected failures:** a test-only hook makes the buffer pool fail in the middle of a heap or catalog operation, to check that nothing half-done becomes visible.

CI runs `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test` on every push and pull request.

## Documentation

- [ROADMAP.md](ROADMAP.md): implementation checklist, with evidence for what is done
- [docs/architecture.md](docs/architecture.md): components, data flow, testing strategy, limitations
- [docs/storage-engine.md](docs/storage-engine.md): disk manager, buffer pool, LRU-K, page and tuple formats, heap file, free space management
- [docs/data-model.md](docs/data-model.md): integers, floats, VARCHAR, BOOLEAN, DATE, TIMESTAMP and NULL, schema, catalog, deviations from the SQL standard
- [docs/decisions/](docs/decisions/README.md): architectural decision records
- [CONTRIBUTING.md](CONTRIBUTING.md): development workflow and conventions

## Limitations

- **No SQL yet.** Everything is driven through the Rust API.
- **Single-threaded.** No concurrent access, locks or transactions.
- **No crash safety.** Without a WAL, a crash can lose pages that were not yet written, and multi-page operations (moving an updated row, creating a table) are not atomic. Errors *inside* the process return without leaving half-applied changes in a page or in the buffer pool; multi-page operations fall back to a best-effort undo.
- **Few types:** integers, floats, `VARCHAR(n)`, `BOOLEAN`, `DATE`, `TIMESTAMP` and `NULL`; no `NUMERIC` yet. Text compares by bytes (no collations), and a value must fit in one page.
- **Unstable formats.** On-disk formats change between versions without migrations; database files are disposable.

## Acknowledgements

The design follows the material of the [CMU Database Group](https://db.cs.cmu.edu/)'s Database Systems course and its BusTub project. Any mistakes are mine.

## License

[MIT](LICENSE)
