# ferroDB Roadmap

This is the authoritative implementation checklist for ferroDB. The README only shows a summary.

**How to read it**

- `[x]` means the feature exists **and** is covered by tests (the links point to both).
- `[ ]` means it is not done. A note says when part of it already exists.
- Sections keep the order in which the project plans to build them. Items marked *(next)* come from the "next steps" recorded in the design notes and are the closest planned work.
- Status was last verified against the code on 2026-10-21 (`cargo test --all-targets`: 979 tests passing).

When an item is finished, mark it here in the same change that adds the code, and update the design notes (see the [documentation index](doc.md)).

## Storage

- [x] Disk Manager ([`disk_manager.rs`](src/storage/disk_manager.rs), unit tests in the module)
- [x] Page ([`page.rs`](src/storage/page.rs), [`tests/storage_page_serialization.rs`](tests/storage_page_serialization.rs))
- [x] Buffer Pool Manager ([`buffer_pool.rs`](src/storage/buffer_pool.rs), unit tests in the module)
- [x] LRU-K Replacement ([`replacer.rs`](src/storage/replacer.rs), unit tests in the module)
- [x] Page Serialization: little-endian codec, 16-byte page header, CRC32 checksum ([`serialization.rs`](src/serialization.rs), [`page.rs`](src/storage/page.rs), [`checksum.rs`](src/storage/checksum.rs), [`tests/storage_page_serialization.rs`](tests/storage_page_serialization.rs))
- [x] Slotted Pages ([`slotted_page.rs`](src/storage/slotted_page.rs), [`tests/storage_slotted_page.rs`](tests/storage_slotted_page.rs))
- [x] Tuple Storage ([`tuple.rs`](src/storage/tuple.rs), [`tests/storage_tuple.rs`](tests/storage_tuple.rs))
- [x] RID ([`rid.rs`](src/storage/rid.rs), unit tests in the module)
- [x] Heap File ([`heap_file.rs`](src/storage/heap_file.rs), [`tests/storage_heap_file.rs`](tests/storage_heap_file.rs))
- [x] Free Space Management ([`free_space_map.rs`](src/storage/free_space_map.rs), [`tests/storage_heap_file.rs`](tests/storage_heap_file.rs))
- [ ] `VACUUM`: compact the heap and return empty pages ([storage notes](docs/storage-engine.md#free-space-management)).
- [ ] Persistent free space map. Only worth it once `HeapFile::open` stops reading every page of the heap.

## Data Types

- [x] Integer: `SMALLINT`, `INTEGER` and `BIGINT`, with parsing, casts, checked arithmetic and comparison ([`src/types/`](src/types/), [`tests/types_integer.rs`](tests/types_integer.rs))
- [x] Float: `REAL` and `DOUBLE PRECISION`, finite only, with checked arithmetic, exact comparison with integers and half-to-even `CAST` to integers ([`float.rs`](src/types/float.rs), [`value.rs`](src/types/value.rs), [`tests/types_float.rs`](tests/types_float.rs))
- [x] Boolean: `sql_eq`/`sql_lt`/... returning `Value::Boolean`, the three-valued logic of `AND`/`OR`/`NOT` and `IS TRUE`/`FALSE`/`UNKNOWN` ([`boolean.rs`](src/types/boolean.rs), [`value.rs`](src/types/value.rs), [`tests/types_boolean.rs`](tests/types_boolean.rs))
- [x] String: `VARCHAR(n)` in UTF-8, with CAST vs. store assignment, `||` and byte-order comparison ([`text.rs`](src/types/text.rs), [`value.rs`](src/types/value.rs), [`tests/types_string.rs`](tests/types_string.rs)). Not included: `CHAR(n)`, `TEXT`, collations and text functions.
- [x] NULL: `Value::Null`, the tuple NULL bitmap, SQL `UNKNOWN` comparison and `IS DISTINCT FROM` ([`value.rs`](src/types/value.rs), [`tuple.rs`](src/storage/tuple.rs))
- [x] Date: `DATE` (years 0001 to 9999, ISO `YYYY-MM-DD` text), with day arithmetic (`DATE ± integer`, `DATE - DATE`) ([`date.rs`](src/types/date.rs), [`value.rs`](src/types/value.rs), [`tests/types_date.rs`](tests/types_date.rs))
- [x] Timestamp: `TIMESTAMP` to the microsecond, in the range of `DATE`, with ISO text (space or `T`, or a bare date as midnight) and comparison with `DATE` ([`timestamp.rs`](src/types/timestamp.rs), [`value.rs`](src/types/value.rs), [`tests/types_timestamp.rs`](tests/types_timestamp.rs))
- [ ] Type System: implicit vs. explicit coercion and operator resolution across type families

## Tables

- [x] Schema: column definitions and typed row ⇄ tuple conversion ([`schema.rs`](src/table/schema.rs), [`tests/table_schema.rs`](tests/table_schema.rs))
- [x] Table Metadata: tables persisted in the catalog ([`src/catalog/`](src/catalog/), [`tests/catalog_table_metadata.rs`](tests/catalog_table_metadata.rs))
- [x] Table Heap: typed `insert` / `get` / `update` / `delete` / `scan`, combining `Schema` and `HeapFile`, found by table name through the catalog, which keeps one handle per table ([`table_heap.rs`](src/table/table_heap.rs), [`catalog.rs`](src/catalog/catalog.rs), [`tests/table_table_heap.rs`](tests/table_table_heap.rs))
- [x] INSERT: `INSERT INTO t [(columns)] VALUES …` through `run_sql`, the columns outside the list `NULL` ([`run.rs`](src/planner/run.rs), [`tests/planner_statements.rs`](tests/planner_statements.rs), [docs](docs/planner.md#statements))
- [ ] SELECT. Parsed and planned (`EXPLAIN SELECT` shows the logical plan); it runs with the physical plan.
- [x] UPDATE: `UPDATE t SET … [WHERE …]` through `run_sql` ([`run.rs`](src/planner/run.rs), [`tests/planner_statements.rs`](tests/planner_statements.rs))
- [x] DELETE: `DELETE FROM t [WHERE …]` through `run_sql` ([`run.rs`](src/planner/run.rs), [`tests/planner_statements.rs`](tests/planner_statements.rs))
- [x] Sequential Scan: typed table scan in heap order, `TableHeap::scan` ([`table_heap.rs`](src/table/table_heap.rs), [`tests/table_table_heap.rs`](tests/table_table_heap.rs)). The scan operator of the executor is under Query Execution.
- [ ] `DROP TABLE`

## Indexes

- [x] B+Tree: unique keys of any type (text included) to `Rid`, order-preserving key encoding, variable-size nodes with split, merge and redistribution by bytes, a free list for freed pages ([`src/index/`](src/index/), [`tests/index_b_plus_tree.rs`](tests/index_b_plus_tree.rs), [docs](docs/indexes.md))
- [x] Index Scan: the `IndexScan` executor reads a table through a B+Tree, fetching each row by `Rid` ([`scan.rs`](src/execution/scan.rs), [`tests/execution_index_scan.rs`](tests/execution_index_scan.rs), [docs](docs/execution.md#index-scan)). The operator is under Query Execution.
- [x] Range Scan: `BPlusTree::range` and `IndexScan` over inclusive, exclusive and open bounds, converted to the key type ([`key_bounds.rs`](src/execution/key_bounds.rs), [`b_plus_tree.rs`](src/index/b_plus_tree.rs), [docs](docs/execution.md#bounds-in-the-keys-type))
- [x] Unique Index: `CREATE UNIQUE INDEX name ON t (column)`, indexing the rows already there; the Insert and Update executors refuse a repeated key (`23505`) ([`run.rs`](src/planner/run.rs), [docs](docs/planner.md#statements))
- [ ] Composite Index

## SQL

- [x] Lexer: words folded to lowercase with their keywords, quoted identifiers, strings, numbers as written, operators (`!=` as `<>`), nested comments, syntax errors (`42601`) at a character position ([`lexer.rs`](src/sql/lexer.rs), [`tests/sql_lexer.rs`](tests/sql_lexer.rs), [docs](docs/sql.md))
- [x] SQL Parser: recursive descent for `SELECT` (projection, joins, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`/`OFFSET`/`FETCH`) and expressions by precedence climbing, with `42601`, `42704`, `22023` and `0A000` errors ([`parser.rs`](src/sql/parser.rs), [`tests/sql_parser.rs`](tests/sql_parser.rs), [docs](docs/sql.md#parser)). Also `CREATE TABLE`, `CREATE [UNIQUE] INDEX`, `INSERT … VALUES`, `UPDATE` and `DELETE`; `DROP TABLE` and `ALTER TABLE` not yet.
- [x] AST: statements with names and literal text, printed back as canonical SQL ([`ast.rs`](src/sql/ast.rs), [docs](docs/sql.md#ast)).
- [x] DDL: `CREATE TABLE` (types, `NOT NULL`) and `CREATE UNIQUE INDEX` run against the catalog ([`run.rs`](src/planner/run.rs), [`tests/planner_statements.rs`](tests/planner_statements.rs), [docs](docs/planner.md)). `PRIMARY KEY`, `UNIQUE` columns and `DROP TABLE` not yet.
- [x] DML: `INSERT`, `UPDATE` and `DELETE` bound and run through the executors ([`binder.rs`](src/planner/binder.rs), [`run.rs`](src/planner/run.rs), [docs](docs/planner.md))
- [x] Query Validation: names resolved against the tables of the `FROM` (`42P01`, `42702`, `42703`, `42712`), the `ON` of a join limited to its two sides, grouping rules (`42803`: columns outside the groups, aggregates nested or in `WHERE`/`ON`/`GROUP BY`), `ORDER BY` positions and names (`42P10`, `42702`), columns in `LIMIT` (`42P10`) ([`scope.rs`](src/planner/scope.rs), [`binder.rs`](src/planner/binder.rs), [`select.rs`](src/planner/select.rs), [`tests/planner_select.rs`](tests/planner_select.rs), [docs](docs/planner.md#planning-a-select))

## Query Planning

- [x] Logical Query Plan: a `SELECT` as `Scan/Join → Filter → Aggregate → Filter → Sort → Limit → Project` over the executors' expressions, shown by `EXPLAIN` with the columns by name ([`logical_plan.rs`](src/planner/logical_plan.rs), [`select.rs`](src/planner/select.rs), [`explain.rs`](src/planner/explain.rs), [`tests/planner_select.rs`](tests/planner_select.rs), [docs](docs/planner.md#logical-plan))
- [ ] Physical Query Plan

## Query Execution

- [x] Sequential Scan: iterator-model `SeqScan` with a heap cursor ([`scan.rs`](src/execution/scan.rs), [`tests/execution_executors.rs`](tests/execution_executors.rs), [docs](docs/execution.md))
- [x] Index Scan: iterator-model `IndexScan` by point or range, in key order, with the last key as cursor ([`scan.rs`](src/execution/scan.rs), [`tests/execution_index_scan.rs`](tests/execution_index_scan.rs), [docs](docs/execution.md#index-scan))
- [x] Filter: keeps the rows whose predicate is TRUE, over expressions with SQL semantics ([`filter.rs`](src/execution/filter.rs), [`expression.rs`](src/execution/expression.rs))
- [x] Projection ([`projection.rs`](src/execution/projection.rs)). Insert, Update and Delete executors also exist ([`modify.rs`](src/execution/modify.rs))
- [x] Sort: in memory, by several keys with `ASC`/`DESC` and `NULLS FIRST`/`LAST`, ties in input order; `TopN` keeps the first rows of the order in a bounded heap ([`sort.rs`](src/execution/sort.rs), [`tests/execution_sort.rs`](tests/execution_sort.rs), [docs](docs/execution.md#sort-limit-and-topn))
- [x] Limit: `LIMIT`/`OFFSET` counts validated as the standard says (`2201W`, `2201X`), never pulling the child past the count ([`limit.rs`](src/execution/limit.rs), [`tests/execution_sort.rs`](tests/execution_sort.rs), [docs](docs/execution.md#counts))
- [x] Aggregation: hash aggregation in memory with `COUNT(*)`, `COUNT`, `SUM`, `AVG`, `MIN` and `MAX`, skipping `NULL` as the standard does ([`aggregation.rs`](src/execution/aggregation.rs), [`tests/execution_aggregation.rs`](tests/execution_aggregation.rs), [docs](docs/execution.md#aggregation))
- [x] GROUP BY: groups by any expressions, `NULL` keys in one group, in order of first appearance; `HAVING` is a `Filter` over the groups ([`aggregation.rs`](src/execution/aggregation.rs), [`tests/execution_aggregation.rs`](tests/execution_aggregation.rs))
- [ ] JOIN. `INNER`, `LEFT`, `RIGHT`, `FULL` and `CROSS` exist through the `IndexNestedLoopJoin` and the `HashJoin`, and the syntax is parsed and planned (`Join` in the logical plan); running a join from SQL waits for the physical plan.
- [x] Hash Join: in-memory build of the right input and probe by normalized keys (`1 = 1.0`), `INNER`, `LEFT`, `RIGHT` and `FULL` with a residual predicate, several keys or none (`CROSS`) ([`hash_join.rs`](src/execution/hash_join.rs), [`tests/execution_hash_join.rs`](tests/execution_hash_join.rs), [docs](docs/execution.md#hash-join))
- [ ] Nested Loop Join. Not implemented on purpose: the plain loop scans the inner table for every outer row; the index nested loop join below replaces it.
- [x] Index Nested Loop Join: one B+Tree descent per outer row, `INNER` and `LEFT` with a residual predicate, the key converted like an `IndexScan` point ([`join.rs`](src/execution/join.rs), [`tests/execution_join.rs`](tests/execution_join.rs), [docs](docs/execution.md#index-nested-loop-join))

## Transactions

- [ ] Transaction Manager
- [ ] BEGIN
- [ ] COMMIT
- [ ] ROLLBACK
- [ ] Isolation Levels
- [ ] Lock Manager
- [ ] Concurrency Control
- [ ] Deadlock Detection

## Recovery

- [ ] WAL. Several components document failure cases that only a WAL can make atomic (see [Limitations](docs/architecture.md#limitations)).
- [ ] LSN. The field exists in every page header (bytes `0..8`) and is preserved, but nothing assigns it yet.
- [ ] Checkpoints
- [ ] Redo
- [ ] Undo
- [ ] Crash Recovery

## Query Optimization

Logical and physical plans are tracked under [Query Planning](#query-planning).

- [ ] Cost Model
- [ ] Statistics
- [ ] Cardinality Estimation
- [ ] Index Selection
- [ ] Join Ordering

## Constraints

- [ ] PRIMARY KEY
- [ ] UNIQUE
- [x] NOT NULL: `CREATE TABLE … NOT NULL`, enforced by the schema on every insert and update (`23502`) ([`run.rs`](src/planner/run.rs), [`schema.rs`](src/table/schema.rs))
- [ ] FOREIGN KEY
- [ ] CHECK
- [ ] DEFAULT

## Connectivity

- [ ] CLI. `cargo run` is a fixed demo, not an interactive client.
- [ ] Client Protocol
- [ ] TCP Server
- [ ] Prepared Statements
- [ ] Connection Pooling

## Security

- [ ] Authentication
- [ ] Users
- [ ] Roles
- [ ] Permissions

## Catalog

- [x] Database Catalog: page 0 database header, persistent catalog loaded on open ([`src/catalog/`](src/catalog/), [`tests/catalog_table_metadata.rs`](tests/catalog_table_metadata.rs))
- [x] Table Metadata ([`records.rs`](src/catalog/records.rs))
- [x] Column Metadata ([`records.rs`](src/catalog/records.rs))
- [x] Index Metadata: unique single-column indexes created and filled from a table's rows, persisted in their own catalog heap, found by name or by table, with the tree opened on demand ([`catalog.rs`](src/catalog/catalog.rs), [`records.rs`](src/catalog/records.rs), [`tests/catalog_index_metadata.rs`](tests/catalog_index_metadata.rs)). Indexes are not updated when the table changes until the executors exist.
- [ ] Constraint Metadata
- [ ] *(next)* String/VARCHAR-backed system tables, so the catalog can be stored as regular tables (`INFORMATION_SCHEMA`)

## Reliability

- [x] Unit Tests: 907 tests, in every module
- [x] Integration Tests: 72 tests in [`tests/`](tests/), including persistence across reopen and on-disk corruption detection
- [ ] Concurrency Tests
- [ ] Crash Recovery Tests
- [ ] Fuzz Tests. Randomized model-based and corruption tests exist (for example `random_corruption_should_never_panic`), but there is no coverage-guided fuzzing harness.
- [ ] Benchmarks

## Goal

Build a fully working educational database and connect real applications to it.
