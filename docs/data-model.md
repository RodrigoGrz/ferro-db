# Data Model

This document is the authoritative description of what ferroDB stores on top of the storage engine: SQL values and types (`src/types/`), table schemas (`src/table/`) and the persistent catalog (`src/catalog/`). The byte formats of pages and tuples are in [storage-engine.md](storage-engine.md).

Dependencies always point downward: `catalog` → `table` → `types` → `storage`.

- [Data Types: Integers and NULL](#data-types-integers-and-null)
- [Schema](#schema)
- [Catalog](#catalog)
- [Deviations from the SQL Standard](#deviations-from-the-sql-standard)

---

## Data Types: Integers and NULL

The `src/types/` layer turns tuple fields (`Option<&[u8]>`) into SQL values and implements their semantics: text parsing, casts, arithmetic, comparison and `NULL`. Only the SQL standard integers exist so far: `SMALLINT`, `INTEGER` and `BIGINT`. The layer depends only on `storage` (for `ColumnLayout`), and `storage` does not know `types`.

| File | Responsibility |
|---|---|
| `data_type.rs` | `DataType`: SQL name, width, `ColumnLayout`, promotion |
| `value.rs` | `Value`: `NULL`, encode/decode, parse, `Display`, cast, operators, comparison |
| `integer.rs` | integer semantics in `i64` + narrowing to the target type (private) |
| `error.rs` | `TypeError` with message and PostgreSQL-compatible `SQLSTATE` |

### Types and bytes

```rust
pub enum DataType { SmallInt, Integer, BigInt }
pub enum Value { Null, SmallInt(i16), Integer(i32), BigInt(i64) }
```

| Type | Rust | `ColumnLayout` | Field in the tuple | `-2` |
|---|---|---|---|---|
| `SMALLINT` | `i16` | `Fixed(2)` | two's complement, LE | `FE FF` |
| `INTEGER` | `i32` | `Fixed(4)` | two's complement, LE | `FE FF FF FF` |
| `BIGINT` | `i64` | `Fixed(8)` | two's complement, LE | `FE FF FF FF FF FF FF FF` |
| `NULL` | — | the column's | `None`: bit set in the NULL bitmap, fixed part zeroed | — |

`Value::decode(ty, field)` rejects a field whose width differs from `ty.width()` (`InvalidEncoding`).

### Operations

- **Text** (`Value::parse`, like PostgreSQL's `int4in`): surrounding whitespace (including `\t\n\r\v\f`), optional sign and ASCII digits. Empty, sign only, `1.0`, `0x10`, `1_000`, the text `'NULL'` → `22P02`. Does not fit the type → `22003`. Accumulates in the negative range (like `pg_strtoint64`), so `-9223372036854775808` works.
- **`Display`:** decimal, and `NULL` as `"NULL"`.
- **Cast:** widening never fails; narrowing is range-checked → `OutOfRange` (`22003`). `CAST(NULL AS T)` is `NULL`.
- **Arithmetic** (`add`, `sub`, `mul`, `div`, `rem`, `neg`):
  - the result has the **widest** operand type; `SMALLINT + SMALLINT` is `SMALLINT` and overflows at 32767, as in PostgreSQL;
  - overflow → `OutOfRange` (`22003`), never wrap-around; includes `-MIN` and `MIN / -1`;
  - `/` truncates toward zero (`7 / -2 = -3`); `%` has the sign of the dividend (`-7 % 2 = -1`);
  - `x / 0` and `x % 0` → `DivisionByZero` (`22012`); `MIN % -1 = 0`.
- **NULL in operations:** any `NULL` operand → `NULL`, before any other check (`NULL / 0` is `NULL`).
- **Comparison:**
  - `compare` → `Option<Ordering>`; `None` is `UNKNOWN` whenever one side is `NULL` (including `NULL = NULL`). Compares by value regardless of width (`INTEGER 5 = BIGINT 5`).
  - `is_distinct_from` → `IS DISTINCT FROM`: `NULL` equals `NULL`, never `UNKNOWN`.
  - `sort_cmp` → total order for `ORDER BY` and indexes, with `NULL` after every value (PostgreSQL's `NULLS LAST` for `ASC`).

### Errors

| `TypeError` | SQLSTATE | Message |
|---|---|---|
| `OutOfRange` | `22003` | `integer out of range` |
| `TextOutOfRange` | `22003` | `value "99999" is out of range for type smallint` |
| `InvalidTextRepresentation` | `22P02` | `invalid input syntax for type integer: "abc"` |
| `DivisionByZero` | `22012` | `division by zero` |
| `InvalidEncoding` | `XX001` | `integer field must be 4 bytes, got 2` |

### Design decisions

- **Three integers together:** same logic, and promotion and casts only make sense with more than one width.
- **`Value` as an enum with native payloads** (not bytes + type, as in BusTub): no allocation per value, and the exhaustive `match` forces every new type to be handled.
- **Everything in `i64`, then narrow:** `i32 × i32` always fits in `i64`, and for `BIGINT` the `checked_*` operations catch overflow. One implementation covers the three widths, without macros or 3×3 combinations.
- **Untyped `NULL`** (`Value::Null`, not `Null(DataType)`): an expression's type comes from the schema, decided by the planner; the `NULL` literal has no type until the type system resolves it from context.
- **`NULL` = `None` in the field:** reuses the tuple NULL bitmap without changing the on-disk format.
- **`compare` and `sort_cmp` are separate:** comparison needs `UNKNOWN`, ordering needs a total order. Mixing the two is a classic bug.
- ⚠️ **Rust's `PartialEq` is structural:** `Null == Null` and `SmallInt(1) != BigInt(1)`. It is meant for tests; filters and joins must use `compare` / `is_distinct_from`.
- ⚠️ **Little-endian is not byte-orderable:** a B+Tree must compare with `sort_cmp` or use its own encoding (big-endian with the sign bit flipped).

### Tests

- Unit in `data_type`, `error` and `value`: boundaries (`MIN`, `-1`, `0`, `1`, `MAX`, `MIN-1`, `MAX+1`) for each width, every error with its SQLSTATE, and `NULL` in every position.
- `random_operations_should_match_the_i128_model`: 10,000 operations with random values (biased toward the extremes, ~10% `NULL`) compared with an `Option<i128>` reference. Mutation-checked by hand during development: removing the `MIN % -1` case makes it fail.
- `random_values_should_round_trip_through_bytes_and_text` and `sort_cmp_should_be_a_total_order` (reflexive, antisymmetric, transitive).
- Integration in [`tests/types_integer.rs`](../tests/types_integer.rs): `[SMALLINT, INTEGER, BIGINT]` rows with extremes and `NULL` survive eviction and reopening the file; an all-`NULL` row only sets the bitmap; a field read with the wrong type → `InvalidEncoding`.

---

## Schema

The `src/table/` layer starts with `Schema` (`schema.rs`): the ordered list of a table's columns and the conversion **row of `Value` ⇄ tuple bytes**. It is the bridge between `types` (values) and `storage` (bytes).

```rust
pub struct Column { pub name: String, pub data_type: DataType, pub nullable: bool }
Column::new("age", SmallInt)        // accepts NULL
Column::not_null("id", Integer)     // NOT NULL

let schema = Schema::new(vec![Column::not_null("id", Integer), Column::new("age", SmallInt)])?;
let tuple  = schema.encode_row(&[Value::Integer(1), Value::Null])?;   // → HeapFile::insert
let row    = schema.decode_row(&tuple)?;                              // ← HeapFile::get / scan
let age    = schema.decode_column(&tuple, 1)?;                        // a single column
let some   = schema.decode_columns(&tuple, &[1, 0])?;                 // several, validating the tuple once
```

A `Schema` is immutable and keeps the precomputed `TupleLayout` (`DataType::layout()` of each column), so field offsets are computed once per table, not per row.

### Schema validation (`Schema::new`)

- From 1 to 1,600 columns (`TupleLayout::MAX_COLUMNS`).
- Non-empty names, up to 63 bytes of UTF-8 (`MAX_COLUMN_NAME_LEN`), unique within the table.
- Names are compared **byte by byte**: `id` and `ID` are different columns. Case folding and quoting are SQL rules that the parser will resolve before names reach this layer.

### Row ⇄ tuple

- **`encode_row`** applies SQL *assignment* column by column, and only builds the tuple if every column passes (the first error, in column order, is the one returned):
  - number of values ≠ number of columns → `ColumnCountMismatch`;
  - `NULL` in a `NOT NULL` column → `NullViolation`;
  - the value is converted to the column type with `Value::cast`: a `SMALLINT` in a `BIGINT` column is accepted; `70000` in a `SMALLINT` column → `Type { column: "age", OutOfRange }`.
- **`decode_row` / `decode_column` / `decode_columns`** read bytes from disk and do not trust them: truncated or corrupted tuple, or one written with another schema → `Tuple(..)`; `NULL` stored in a `NOT NULL` column → `NullViolation`. Values come back **in the column's type** (a `SmallInt` stored in an `INTEGER` column comes back as `Integer`).
  - Every field is read with `TupleRef::field`, which returns an error: a field the tuple cannot produce is never mistaken for `NULL`.
  - `decode_columns` returns the requested columns in the requested order (repetitions allowed), checks the indexes **before** reading the tuple and validates the tuple only once.
- Each field's width comes from the schema itself, so `Type { .. InvalidEncoding }` does not happen on read; type errors only appear in `encode_row` (a value that does not fit).

### Errors

| `SchemaError` | SQLSTATE | Example message |
|---|---|---|
| `NoColumns` | `42P16` | `a table must have at least one column` |
| `TooManyColumns` | `54011` | `tables can have at most 1600 columns, got 1601` |
| `EmptyColumnName` | `42602` | `column 2 has an empty name` |
| `ColumnNameTooLong` | `42622` | `column name "aaa…" is longer than 63 bytes` (the message shows only the first 63 bytes) |
| `DuplicateColumn` | `42701` | `column "id" specified more than once` |
| `ColumnCountMismatch` | `42601` | `row has 2 values but the table has 3 columns` |
| `NullViolation` | `23502` | `null value in column "id" violates not-null constraint` |
| `ColumnOutOfRange` | `XX000` | `column 5 does not exist (table has 3)` (programming error) |
| `Type` | the `TypeError`'s (e.g. `22003`) | `column "age": smallint out of range` |
| `Tuple` | `XX001` | `stored row does not match the schema: expected 3 columns, got 2` |

### Design decisions

- **`NOT NULL` already in the schema**, as a column flag checked by `encode_row`: one line of code, and it avoids changing the schema format once the catalog stores it. The **NOT NULL** constraint in the [roadmap](../ROADMAP.md#constraints) is only marked when DDL (`CREATE TABLE ... NOT NULL`) exists.
- **Assignment with cast** instead of requiring the exact type: it is the standard semantics for `INSERT`/`UPDATE`, and `Value::cast` already does the range check.
- **A row is a `Vec<Value>`:** a `struct Row` would have no invariant of its own beyond what `Schema` validates.
- **63-byte name limit:** fits comfortably in a 1-byte length prefix in the catalog. The standard's 128 characters can exceed 255 bytes in UTF-8.
- **At least one column**, as in the standard. A table without columns would complicate the catalog and the executor for no benefit here.
- **`decode_column` / `decode_columns`:** `TupleRef` accesses a field in O(1); projections and filters will read few columns per row, and `decode_columns` avoids validating the same tuple once per column.
- **Truncated name in messages:** `ColumnNameTooLong` keeps the whole name, but `Display` shows only the prefix that would fit (cut at a character boundary) followed by `…`, so a huge name does not become a huge message.
- ⚠️ **A row larger than a page:** a valid schema can describe rows that do not fit in a page (e.g. 1,600 `BIGINT` columns ≈ 13 KB > 8 KB). `Schema` does not know the page size; the `HeapFile` rejects such rows with `TupleTooLarge` (covered by an integration test), and `min_row_size()` lets `create_table` reject the schema up front (see [Catalog](#catalog)).
- ⚠️ **`NullViolation` has two meanings:** a user value in `encode_row`, or inconsistent data on disk in `decode_*` (same variant, message with the column name).

### Tests

- Unit in `schema.rs`: `Column`, every `Schema::new` rule with boundaries (1,600/1,601 columns, 63/64-byte names including multibyte, `id` vs. `ID`), every `SchemaError` with message, SQLSTATE and `source()`, `encode_row` (width cast, does not fit, `NULL` allowed and forbidden, arity, first error wins), `decode_row` / `decode_column` / `decode_columns` (round trip with boundaries and `NULL`, column types, requested order with repetition, empty list, out of range before reading the tuple, truncated, written with another schema, `NULL` on disk in a `NOT NULL` column), truncated long-name message (ASCII and multibyte).
- `random_rows_should_follow_the_assignment_rules`: 2,000 random schemas and rows compared with rules computed by hand (without `Value::cast`), read back with `decode_row`, `decode_column` and `decode_columns` in reverse order. Mutation-checked by hand during development: disabling `NOT NULL` or the cast makes it fail.
- `random_corruption_should_never_panic`: 2,000 valid tuples with flipped bits or truncated.
- Integration in [`tests/table_schema.rs`](../tests/table_schema.rs): 1,000 typed rows in a `HeapFile` survive eviction and reopening the file, `decode_column` projects one column of the stored rows, rows rejected by the schema never reach the heap, a row from a schema wider than a page is rejected by the heap with `TupleTooLarge`.

---

## Catalog

The catalog (`src/catalog/`) is the data dictionary: which tables exist, where their data lives and which columns they have. It solves two problems every database has:

- **Bootstrap:** the catalog also lives on disk, and something must say where. **Page 0** (reserved by the heap file) is the **database header** and points to it.
- **Variable size:** as in PostgreSQL (`pg_class` + `pg_attribute`), tables and columns are stored in **two separate heaps**, one entry per table and one per column. A single entry with every column would exceed 100 KB with 1,600 columns and would not fit in a page.

```
page 0 (DatabaseHeader)
  ├── tables_first_page_id  ──▶ tables heap    (TableRecord: id, first_page_id, name)
  └── columns_first_page_id ──▶ columns heap   (ColumnRecord: table, position, type, NOT NULL, name)
                                                        │
                    TableRecord.first_page_id ──────────┴──▶ HeapFile with the table's rows
```

Entries use their **own format** (`ByteWriter` / `ByteReader`): the catalog is not yet a regular table read through `Schema` / `Value` because the String type does not exist. When it does, the catalog can become system tables without changing the API.

```rust
let mut catalog = Catalog::open(&mut bpm)?;                      // creates the database or loads it
let table = catalog.create_table(&mut bpm, "accounts", schema)?; // TableInfo { id, name, schema, first_page_id }
let table = catalog.table("accounts")?;                          // UndefinedTable if it does not exist
let mut heap = HeapFile::open(&mut bpm, table.first_page_id)?;   // the table's data
```

### Layout

**Page 0 (`DatabaseHeader`, `catalog/header.rs`):**

| Bytes | Field |
|---|---|
| `0..16` | common header, `page_type = DatabaseHeader (2)` |
| `16..24` | magic `b"ferroDB\0"` |
| `24..28` | `tables_first_page_id` (`u32` LE) |
| `28..32` | `columns_first_page_id` (`u32` LE) |
| `32..` | zero |

The checksum catches flipped bytes; the magic rejects a file whose page 0 is valid but is not a ferroDB database.

**Entries (`catalog/records.rs`, little-endian):**

| Entry | Fields |
|---|---|
| `TableRecord` | `table_id u32` · `first_page_id u32` · `name_len u8` · name |
| `ColumnRecord` | `table_id u32` · `position u16` · `data_type u8` (`1` SMALLINT, `2` INTEGER, `3` BIGINT) · `flags u8` (bit 0 = nullable) · `name_len u8` · name |

Names have 1 to 63 bytes of UTF-8. Decoding rejects unknown types or flags (`0` is invalid on purpose, so a zeroed entry never decodes), names with an out-of-range length or invalid UTF-8, and missing or trailing bytes. An entry has at most 74 bytes.

### Operations

- **`open`:**
  - page 0 is `Free` → new database: creates both heaps and writes the header **last** (until then, the next `open` starts over);
  - page 0 is `DatabaseHeader` → checks the magic, opens both heaps, decodes every entry and builds each `Schema` in `position` order;
  - page 0 of another type or a wrong magic → `NotADatabase`.
- **Loading and validation:** columns numbered `0..n` without gaps or repetitions, a valid `Schema::new` (`CorruptedSchema`), unique table names and ids (`Corrupted`). Columns of a `table_id` without a `TableRecord` are **orphans** (left over by a failed `create_table`) and are ignored; the next id is `1 + highest id seen in tables and columns`, so an orphan id is never reused.
- **`create_table`**, in this order:
  1. validates the name (empty, longer than 63 bytes, duplicate) and whether a row fits in a page (`schema.min_row_size() <= max_tuple_size_for(page_size)`, otherwise `RowTooWide`);
  2. **consumes** the table id;
  3. creates the data `HeapFile`;
  4. writes one `ColumnRecord` per column;
  5. writes the `TableRecord` **last**: it is what makes the table exist on the next `open`;
  6. only then adds the table to the in-memory catalog.

  If step 4 or 5 fails, the columns already written are deleted (best effort), the table does not appear and the same name can be created again, but **with another id**: the id consumed in step 2 never comes back. If the undo also fails, the remaining columns become orphans with that id, and no new table inherits them. That is why ids can have gaps, like PostgreSQL sequences.
- **`table(name)` / `tables()`:** in-memory lookups, no I/O. `tables()` is in creation order. Names are compared byte by byte (`users` ≠ `USERS`), like columns.

### Errors

| `CatalogError` | SQLSTATE | Example message |
|---|---|---|
| `NotADatabase` | `XX001` | `not a ferroDB database: wrong magic number` |
| `EmptyTableName` | `42602` | `a table name cannot be empty` |
| `TableNameTooLong` | `42622` | `table name "aaa…" is longer than 63 bytes` |
| `DuplicateTable` | `42P07` | `table "users" already exists` |
| `UndefinedTable` | `42P01` | `table "users" does not exist` |
| `RowTooWide` | `54000` | `rows of table "wide" need 9000 bytes, more than the 8164 that fit a page` |
| `TooManyTables` | `54000` | `no table ids left` |
| `Corrupted` | `XX001` | `catalog is corrupted: unknown column type` |
| `CorruptedSchema` | `XX001` | `catalog is corrupted: table "t": a table must have at least one column` |
| `Heap` / `BufferPool` | `XX000` / `58030` | message of the wrapped error |

### Design decisions

See [ADR 0006](decisions/0006-catalog-on-page-zero.md) for the bootstrap and commit-point decisions. Also:

- **The whole catalog in memory**, loaded on `open`: tens of bytes per column, and lookups do no I/O.
- **`TableId(u32)` newtype:** cannot be confused with a page id. The internal counter is a `u64`, so a `u32::MAX` id stored on disk still loads; creating the next table returns `TooManyTables`.
- **The catalog does not keep `HeapFile` handles**, only `first_page_id`: `open` does not read data pages. Handles (one per table, see [Heap File](storage-engine.md#heap-file)) belong to the future table layer / executor.

### Known limitations

- ⚠️ **Crash in the middle of `create_table` or of database creation:** the buffer pool writes pages in any order, so after a real crash "`TableRecord` last" is not guaranteed; orphan columns may remain (ignored) or, in the worst case, a `TableRecord` without columns (`CorruptedSchema` on `open`). The complete fix is the WAL.
- ⚠️ **Orphan pages:** the data heap of a failed `create_table` and the heaps of an interrupted database creation have no owner until a future `VACUUM`.
- ⚠️ **Provisional format:** when the catalog becomes system tables, old databases will not open (no migration; test databases are disposable).

### Demo (`cargo run`)

`src/main.rs` opens or creates `data.db` in the current directory, creates the table `accounts (id INTEGER NOT NULL, age SMALLINT, balance BIGINT)` on the first run, inserts one row per run and lists the tables and the rows. Running it several times shows the data surviving. A `data.db` from the old demo (a slotted page in page 0) fails with `NotADatabase` and a message asking to delete the file. A captured run is in the [README](../README.md#quick-start).

### Tests

- `header.rs`: documented bytes, round trip, LSN preserved and reserved bytes zeroed, never-written page → `None`, slotted page or wrong magic → `NotADatabase`, page too small.
- `records.rs`: documented bytes, round trip (names of 1 and 63 bytes, multibyte, each type, nullable or not), every corruption reason (truncated at every length, trailing bytes, type `0`/`4`, unknown flag, name of 0/64 bytes, invalid UTF-8), 2,000 random round-trip entries and 2,000 corrupted ones without panicking.
- `catalog.rs`: database creation and empty reopen, page 0 that is not a header, `create_table` (lookup, order and ids, invalid or duplicate name, `id` vs. `ID`, `RowTooWide` at the exact limit, data heap that accepts the schema's rows, `TooManyTables`, including with a `u32::MAX` id loaded from disk), loading (every table back, ids continue, 1,000 columns across several pages with eviction, orphans ignored without reusing ids, columns with gaps or repetitions, invalid schema, table without columns, duplicate name or id, corrupted entry), injected failures (in the middle of `create_table`, on the table entry and between columns: no visible table, not even after reopening; **undo that also fails**: the id is not reused and the database still opens; failed header write: the next `open` creates the database from scratch) and every error with message, SQLSTATE and `source()`.
- `random_creations_should_match_model_across_reopens`: 300 random creations (duplicate and invalid names, 1 to 30 columns) with a 3-frame pool, reopening every 50 and comparing with a `Vec`.
- Mutations checked by hand during development: removing the column undo breaks the failure tests; resetting the next id to 1 after reopening breaks the model test.
- Integration in [`tests/catalog_table_metadata.rs`](../tests/catalog_table_metadata.rs): tables and rows of two tables survive reopening the file (2-frame pool), a file whose page 0 is a slotted page → `NotADatabase`, a corrupted page of the columns heap → checksum error on `open`.

---

## Deviations from the SQL Standard

| Subject | Standard (ISO/IEC 9075) | ferroDB |
|---|---|---|
| Integer literals | decimal only | same; PostgreSQL 16 accepts `1_000`, `0x1F`, `0o17`, `0b101`, ferroDB does not yet |
| Integer division | implementation-defined rounding | truncates toward zero (as PostgreSQL) |
| Result type of `a op b` | implementation-defined | the widest operand type (as PostgreSQL) |
| Position of `NULL` in `ORDER BY` | implementation-defined | after the values for `ASC` (as PostgreSQL) |
| `MOD(a, b)` | function | only the semantics for now, as the `%` operator; the function arrives with the parser |
| Display of `NULL` | not defined | `"NULL"` in `Display` (`psql` shows an empty cell) |
| Table without columns | `CREATE TABLE` requires at least one column | same (`NoColumns`); PostgreSQL accepts tables without columns |
| Column name length | implementation-defined, in characters | up to 63 **bytes** of UTF-8; longer names are rejected (`42622`), PostgreSQL truncates silently |
| Equal column names | compared after identifier normalization | the schema compares exact bytes; normalization (case, quotes) is left to the parser |
| Schemas (`CREATE SCHEMA`, `schema.table`) | every table belongs to a schema | a single namespace for tables |
| Queryable catalog | `INFORMATION_SCHEMA.TABLES` / `.COLUMNS` | not exposed through SQL yet; access is through the `Catalog` API |
| Table name length | implementation-defined, in characters | up to 63 **bytes** of UTF-8, rejected above that (`42622`), like columns |
