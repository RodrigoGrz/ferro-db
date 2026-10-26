<p align="center">
  <img src="ferrodb.png" alt="ferroDB logo" width="180">
</p>

# ferroDB

[![CI](https://github.com/RodrigoGrz/ferro-db/actions/workflows/ci.yml/badge.svg)](https://github.com/RodrigoGrz/ferro-db/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

An educational relational database engine, built from scratch in Rust and inspired by the [CMU Database Group](https://db.cs.cmu.edu/)'s Database Systems course (15-445/645).

ferroDB exists so I can understand how a relational database works by implementing its internals myself: how pages reach the disk, how a buffer pool decides what stays in memory, how rows of different sizes share a page, and, later, how queries, indexes and transactions are built on top of that. The long-term goal is a small but real SQL database that a simple application can connect to.

It is **not** meant to compete with PostgreSQL, MySQL or SQLite, and it is **not production-ready**.

**Current status:** the storage engine, a typed table layer, a persistent catalog, a B+Tree index, query executors and an SQL layer work and are tested (1,391 tests). Tables and unique indexes are created, rows inserted, updated and deleted, and queries run, all with SQL, from the `ferro-db` command-line client or `run_sql`: a `SELECT` is validated, planned and run by executors chosen by fixed rules, and `EXPLAIN` shows the plan. See the [roadmap](ROADMAP.md) for the full checklist.

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
| **Catalog** | Tables, columns and indexes persisted on disk and bootstrapped from page 0, loaded into memory on open; finds a table's rows and an index's tree by name, keeping one open handle each; creates and fills indexes. |
| **B+Tree index** | A unique index from keys of any type (text included) to row ids, with range scans; nodes of variable-size entries split, merge and redistribute by bytes, and a failed operation changes nothing. |
| **Executors** | Iterator-model executors over expressions with SQL semantics: sequential scan, index scan (points and ranges, in key order), values, filter (`WHERE` keeps TRUE only), projection, hash aggregation (`GROUP BY` with `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`; `HAVING` as a filter), sort (several keys, `NULLS FIRST`/`LAST`), limit and offset, top-N with a bounded heap, index nested loop join (`INNER` and `LEFT`), hash join (`INNER`, `LEFT`, `RIGHT`, `FULL`), insert, update and delete. Writes keep the indexes in step and validate a whole statement before writing (a refused statement leaves no trace). |
| **SQL lexer and parser** | Text to an AST: `SELECT` (joins, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`/`OFFSET`/`FETCH`), `CREATE TABLE`, `CREATE UNIQUE INDEX`, `INSERT`, `UPDATE`, `DELETE` and `EXPLAIN`; expressions by precedence climbing, syntax errors at a character position, a nesting limit that keeps any statement from exhausting the stack. |
| **Planner** | `run_sql` binds names to column positions and literals to types, runs DDL against the catalog and DML through the executors; a `SELECT` becomes a validated logical plan (the scope of its `FROM`, the grouping rules, `ORDER BY` by position, name or expression), then executors chosen by fixed rules (hash joins on the equalities of the `ON`, index joins and index scans where an index serves, `TopN` for `ORDER BY … LIMIT`) that run it; `EXPLAIN` shows the physical plan, `EXPLAIN LOGICAL` the logical one. |
| **CLI** | `ferro-db [FILE]` opens a database and reads SQL from a terminal (with prompts) or a script: multi-line commands ending in `;`, rows printed as aligned tables, command tags, errors with their SQLSTATE, and the meta-commands `\d`, `\d TABLE`, `\i FILE`, `\?`, `\q`. |

**Partially there:**

- Plans are chosen by fixed rules, without statistics or costs: an index serves only an equality with a literal on one table and the right table of an index join, filters stay above joins, and joins run in the order written.
- A query's rows are collected in memory and returned all at once; there is no client to stream them to yet.
- Every page reserves an LSN for write-ahead logging, but there is no WAL yet.

**Planned (not implemented):** query optimization (statistics, costs, join order), more joins (sort-merge join, the plain nested loop for any condition), more data types (`NUMERIC`), constraints, transactions and concurrency control, WAL and crash recovery, a client/server interface. The [roadmap](ROADMAP.md) has the complete list in order.

## Architecture

```mermaid
flowchart TD
    app["ferro-db CLI / Rust API"]
    catalog["Catalog<br/>tables and columns, page 0"]
    tableheap["Table Heap<br/>typed rows of a table"]
    btree["B+Tree index<br/>keys → row ids"]
    exec["Executors<br/>seq and index scan, filter, projection, aggregation, sort, limit, top-n, index join, hash join, insert, update, delete"]
    schema["Schema + types<br/>row ⇄ tuple, SQL semantics"]
    heap["Heap File + free space map"]
    page["Slotted pages + tuple format"]
    bpm["Buffer Pool Manager + LRU-K"]
    disk["Disk Manager"]
    file[("Database file")]

    app --> exec
    exec --> catalog
    exec --> tableheap
    exec --> btree
    app --> catalog
    btree --> bpm
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

`cargo run -- shop.db` opens the database in `shop.db` (creating it if needed; `data.db` without an argument) and starts an interactive SQL prompt ([docs/connectivity.md](docs/connectivity.md)). A command may span lines and ends with `;`:

```text
$ cargo run -- shop.db
ferro-db: type \? for help, \q to quit
ferro=> CREATE TABLE customers (id INTEGER NOT NULL, name VARCHAR(20), city VARCHAR(20));
CREATE TABLE
ferro=> CREATE UNIQUE INDEX customers_id ON customers (id);
CREATE INDEX
ferro=> CREATE TABLE orders (id INTEGER NOT NULL, customer_id INTEGER, total BIGINT);
CREATE TABLE
ferro=> INSERT INTO customers VALUES (1, 'Ana', 'Lisboa'), (2, 'Rui', 'Porto'), (3, 'Inês', NULL);
INSERT 0 3
ferro=> INSERT INTO orders VALUES (10, 1, 50), (11, 1, 70), (12, 2, 20);
INSERT 0 3
ferro=> SELECT c.name, count(*) AS orders, sum(o.total) AS spent
ferro->   FROM orders o JOIN customers c ON c.id = o.customer_id
ferro->   GROUP BY c.name ORDER BY spent DESC;
 name | orders | spent
------+--------+-------
 Ana  |      2 |   120
 Rui  |      1 |    20
(2 rows)

ferro=> EXPLAIN SELECT c.name FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.total > 30;
Projection: c.name AS name
  Filter: o.total > 30
    IndexNestedLoopJoin INNER: c.id = o.customer_id (index customers_id ON customers AS c)
      SeqScan: orders AS o

ferro=> \d customers
Table "customers"
 column |    type     | nullable
--------+-------------+----------
 id     | INTEGER     | not null
 name   | VARCHAR(20) |
 city   | VARCHAR(20) |
(3 rows)
Indexes:
    "customers_id" UNIQUE (id)

ferro=> SELECT nome FROM customers;
ERROR: column "nome" does not exist (SQLSTATE 42703)
ferro=> \q
```

Without a terminal it reads a script instead, without prompts: `cargo run -- shop.db < setup.sql`. `\i FILE` runs a file from the prompt, `\d` lists the tables and `\?` shows the meta-commands. Every typed command's pages are written to the file before the next prompt (a script's, at its end), so the data is there the next time you open it. `ferro-db --help` prints the usage.

## Testing

```sh
cargo test --all-targets   # 1,283 unit tests + 108 integration tests
```

The suite is the main way the implementation is verified:

- **Unit tests** in every module, covering boundaries and every error variant.
- **Integration tests** in [`tests/`](tests/), one file per component: data survives eviction with tiny buffer pools and reopening the file.
- **Corruption tests:** bytes changed on disk are detected by checksums and validation, never by a panic.
- **Randomized model tests:** thousands of random operations compared against a simple in-memory model (`HashMap`, `Vec`, `i128` arithmetic), with invariants checked along the way.
- **Injected failures:** a test-only hook makes the buffer pool fail in the middle of a heap, catalog or index operation, to check that nothing half-done becomes visible.

CI runs `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test` on every push and pull request.

## Documentation

- [ROADMAP.md](ROADMAP.md): implementation checklist, with evidence for what is done
- [docs/architecture.md](docs/architecture.md): components, data flow, testing strategy, limitations
- [docs/storage-engine.md](docs/storage-engine.md): disk manager, buffer pool, LRU-K, page and tuple formats, heap file, free space management
- [docs/data-model.md](docs/data-model.md): integers, floats, VARCHAR, BOOLEAN, DATE, TIMESTAMP and NULL, schema, catalog, deviations from the SQL standard
- [docs/indexes.md](docs/indexes.md): the B+Tree, key encoding, node format, splits and merges
- [docs/execution.md](docs/execution.md): expressions, executors, two-phase writes and index maintenance
- [docs/sql.md](docs/sql.md): the SQL layer: the lexer (tokens, keywords, identifiers, literals, comments) and the parser and AST for `SELECT` (grammar, precedence, reserved words, errors)
- [docs/connectivity.md](docs/connectivity.md): the command-line client: usage, when a command is complete, meta-commands, output format
- [docs/planner.md](docs/planner.md): the planner layer: `run_sql`, the scope and the binder (names, literal types, `BETWEEN`/`IN`, grouping), how each statement runs, the logical and physical plans of a `SELECT` and `EXPLAIN`, errors
- [docs/decisions/](docs/decisions/README.md): architectural decision records
- [CONTRIBUTING.md](CONTRIBUTING.md): development workflow and conventions

## Limitations

- **One embedded client.** The CLI opens the database file itself; there is no server, network protocol or concurrent access.
- **Single-threaded.** No concurrent access, locks or transactions.
- **No crash safety.** Without a WAL, a crash can lose pages that were not yet written, and multi-page operations (moving an updated row, creating a table) are not atomic. Errors *inside* the process return without leaving half-applied changes in a page or in the buffer pool; multi-page operations fall back to a best-effort undo.
- **Few types:** integers, floats, `VARCHAR(n)`, `BOOLEAN`, `DATE`, `TIMESTAMP` and `NULL`; no `NUMERIC` yet. Text compares by bytes (no collations), and a value must fit in one page.
- **Unstable formats.** On-disk formats change between versions without migrations; database files are disposable.

## Acknowledgements

The design follows the material of the [CMU Database Group](https://db.cs.cmu.edu/)'s Database Systems course and its BusTub project. Any mistakes are mine.

## License

[MIT](LICENSE)
