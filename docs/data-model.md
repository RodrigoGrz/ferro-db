# Data Model

This document is the authoritative description of what ferroDB stores on top of the storage engine: SQL values and types (`src/types/`), table schemas and typed rows (`src/table/`) and the persistent catalog (`src/catalog/`). The byte formats of pages and tuples are in [storage-engine.md](storage-engine.md).

Dependencies always point downward: `catalog` → `table` → `types` → `storage`.

- [Data Types: Integers and NULL](#data-types-integers-and-null)
- [Data Types: VARCHAR](#data-types-varchar)
- [Data Types: BOOLEAN](#data-types-boolean)
- [Data Types: REAL and DOUBLE PRECISION](#data-types-real-and-double-precision)
- [Data Types: DATE](#data-types-date)
- [Data Types: TIMESTAMP](#data-types-timestamp)
- [Schema](#schema)
- [Table Heap](#table-heap)
- [Catalog](#catalog)
- [Deviations from the SQL Standard](#deviations-from-the-sql-standard)

---

## Data Types: Integers and NULL

The `src/types/` layer turns tuple fields (`Option<&[u8]>`) into SQL values and implements their semantics: text parsing, casts, arithmetic, comparison and `NULL`. The SQL standard integers `SMALLINT`, `INTEGER` and `BIGINT` are described here; text, booleans, floats, dates and timestamps are in [Data Types: VARCHAR](#data-types-varchar), [Data Types: BOOLEAN](#data-types-boolean), [Data Types: REAL and DOUBLE PRECISION](#data-types-real-and-double-precision) , [Data Types: DATE](#data-types-date) and [Data Types: TIMESTAMP](#data-types-timestamp). The layer depends only on `storage` (for `ColumnLayout`), and `storage` does not know `types`.

| File | Responsibility |
|---|---|
| `data_type.rs` | `DataType`: SQL name, fixed width, `ColumnLayout`, family, integer promotion |
| `value.rs` | `Value`: `NULL`, encode/decode, parse, `Display`, cast and assignment, operators, comparison |
| `integer.rs` | integer semantics in `i64` + narrowing to the target type (private) |
| `text.rs` | `VARCHAR(n)` semantics: length in characters, truncation, trailing-space rule, UTF-8 validation (private) |
| `date.rs` | `DATE`: the `Date` newtype, Hinnant's calendar conversions, ISO text input, day arithmetic (private; `Date` is re-exported) |
| `timestamp.rs` | `TIMESTAMP`: the `Timestamp` newtype, the split into day and time of day, the text input (private; `Timestamp` is re-exported) |
| `float.rs` | `REAL` / `DOUBLE PRECISION`: finite newtypes, text input and output, checked arithmetic, conversions, exact comparison with integers (private) |
| `boolean.rs` | `BOOLEAN` semantics: the 1-byte format, text input, integer conversion, Kleene's `AND`/`OR` (private) |
| `error.rs` | `TypeError` with message and PostgreSQL-compatible `SQLSTATE` |

### Types and bytes

```rust
pub enum DataType { SmallInt, Integer, BigInt, Real, Double, Varchar(NonZeroU16), Boolean, Date, Timestamp }
pub enum Value { Null, SmallInt(i16), Integer(i32), BigInt(i64), Real(Finite32), Double(Finite64), Text(String), Boolean(bool), Date(Date), Timestamp(Timestamp) }
```

| Type | Rust | `ColumnLayout` | Field in the tuple | `-2` |
|---|---|---|---|---|
| `SMALLINT` | `i16` | `Fixed(2)` | two's complement, LE | `FE FF` |
| `INTEGER` | `i32` | `Fixed(4)` | two's complement, LE | `FE FF FF FF` |
| `BIGINT` | `i64` | `Fixed(8)` | two's complement, LE | `FE FF FF FF FF FF FF FF` |
| `NULL` | — | the column's | `None`: bit set in the NULL bitmap, fixed part zeroed | — |

`Value::decode(ty, field)` rejects a field whose width differs from `ty.fixed_width()` (`InvalidEncoding`).

### Operations

- **Text** (`Value::parse`, like PostgreSQL's `int4in`): surrounding whitespace (including `\t\n\r\v\f`), optional sign and ASCII digits. Empty, sign only, `1.0`, `0x10`, `1_000`, the text `'NULL'` → `22P02`. Does not fit the type → `22003`. Accumulates in the negative range (like `pg_strtoint64`), so `-9223372036854775808` works.
- **`Display`:** decimal, and `NULL` as `"NULL"`.
- **Cast and assignment:** widening never fails; narrowing is range-checked → `OutOfRange` (`22003`). `CAST(NULL AS T)` is `NULL`. Between integers, `Value::cast` (explicit `CAST`) and `Value::assign` (store assignment, used by `INSERT`/`UPDATE`) behave the same; they differ for text (see [VARCHAR](#cast-and-assignment)).
- **Arithmetic** (`add`, `sub`, `mul`, `div`, `rem`, `neg`):
  - the result has the **widest** operand type; `SMALLINT + SMALLINT` is `SMALLINT` and overflows at 32767, as in PostgreSQL;
  - overflow → `OutOfRange` (`22003`), never wrap-around; includes `-MIN` and `MIN / -1`;
  - `/` truncates toward zero (`7 / -2 = -3`); `%` has the sign of the dividend (`-7 % 2 = -1`);
  - `x / 0` and `x % 0` → `DivisionByZero` (`22012`); `MIN % -1 = 0`.
- **NULL in operations:** any `NULL` operand → `NULL`, before any other check (`NULL / 0` is `NULL`).
- **Comparison:**
  - `compare` → `Result<Option<Ordering>, TypeError>`; `Ok(None)` is `UNKNOWN` whenever one side is `NULL` (including `NULL = NULL`). Compares by value regardless of width (`INTEGER 5 = BIGINT 5`). Comparing an integer with text → `UndefinedOperator` (`42883`).
  - `sql_eq` / `sql_lt` / ... → the same comparison as a SQL value (`Boolean` or `Null`); see [BOOLEAN](#comparison-as-sql-values).
  - `is_distinct_from` → `Result<bool, TypeError>`, `IS DISTINCT FROM`: `NULL` equals `NULL`, never `UNKNOWN`.
  - `sort_cmp` → total order for `ORDER BY` and indexes, with `NULL` after every value (PostgreSQL's `NULLS LAST` for `ASC`). It never fails: numbers (integers and floats, interleaved by value) sort before text, text before booleans and booleans before datetimes (dates and timestamps interleaved by instant), an order that only exists to make the function total (a column never mixes families).

### Errors

| `TypeError` | SQLSTATE | Message |
|---|---|---|
| `OutOfRange` | `22003` | `integer out of range` |
| `TextOutOfRange` | `22003` | `value "99999" is out of range for type smallint` |
| `InvalidTextRepresentation` | `22P02` | `invalid input syntax for type integer: "abc"` |
| `DivisionByZero` | `22012` | `division by zero` |
| `InvalidEncoding` | `XX001` | `integer field must be 4 bytes, got 2` |

The text errors (`StringTooLong`, `UndefinedOperator`, `DatatypeMismatch`, `CorruptedValue`) are listed in the [VARCHAR section](#errors-1), `NotBoolean` in the [BOOLEAN section](#errors-2), `CannotCast` in the [float section](#errors-3), and the datetime SQLSTATEs (`22007`, `22008`) in the [DATE section](#errors-4), which also apply to `TIMESTAMP`.

### Design decisions

- **Three integers together:** same logic, and promotion and casts only make sense with more than one width.
- **`Value` as an enum with native payloads** (not bytes + type, as in BusTub): no allocation for integers (only `Text` owns a buffer), and the exhaustive `match` forces every new type to be handled.
- **Everything in `i64`, then narrow:** `i32 × i32` always fits in `i64`, and for `BIGINT` the `checked_*` operations catch overflow. One implementation covers the three widths, without macros or 3×3 combinations.
- **Untyped `NULL`** (`Value::Null`, not `Null(DataType)`): an expression's type comes from the schema, decided by the planner; the `NULL` literal has no type until the type system resolves it from context.
- **`NULL` = `None` in the field:** reuses the tuple NULL bitmap without changing the on-disk format.
- **`compare` and `sort_cmp` are separate:** comparison needs `UNKNOWN`, ordering needs a total order. Mixing the two is a classic bug.
- ⚠️ **Rust's `PartialEq` is structural:** `Null == Null` and `SmallInt(1) != BigInt(1)`. It is meant for tests; filters and joins must use `sql_eq` / `compare` / `is_distinct_from`.
- ⚠️ **Little-endian is not byte-orderable:** a B+Tree must compare with `sort_cmp` or use its own encoding (big-endian with the sign bit flipped).

### Tests

- Unit in `data_type`, `error` and `value`: boundaries (`MIN`, `-1`, `0`, `1`, `MAX`, `MIN-1`, `MAX+1`) for each width, every error with its SQLSTATE, and `NULL` in every position.
- `random_operations_should_match_the_i128_model`: 10,000 operations with random values (biased toward the extremes, ~10% `NULL`) compared with an `Option<i128>` reference. Mutation-checked by hand during development: removing the `MIN % -1` case makes it fail.
- `random_values_should_round_trip_through_bytes_and_text` and `sort_cmp_should_be_a_total_order` (reflexive, antisymmetric, transitive).
- Integration in [`tests/types_integer.rs`](../tests/types_integer.rs): `[SMALLINT, INTEGER, BIGINT]` rows with extremes and `NULL` survive eviction and reopening the file; an all-`NULL` row only sets the bitmap; a field read with the wrong type → `InvalidEncoding`.

---

## Data Types: VARCHAR

`VARCHAR(n)` (`CHARACTER VARYING(n)`) is the only text type: at most `n` characters, `1 ≤ n ≤ 65535`. A character is a Unicode scalar value (Rust's `char`), not a byte: `'ação'` fits `VARCHAR(4)` although its UTF-8 takes 6 bytes. `CHAR(n)` and `TEXT` do not exist (see [ADR 0008](decisions/0008-varchar-utf8-byte-order-and-assignment.md)).

```rust
DataType::Varchar(NonZeroU16)        // VARCHAR(0) cannot be built
DataType::VARCHAR_MAX                // VARCHAR(65535): the type of text that did not come from a column
Value::Text(String)                  // the limit n belongs to the column, not to the value
```

`Value` is no longer `Copy`, since `Text` owns its bytes.

### Bytes

| Type | `ColumnLayout` | Field in the tuple |
|---|---|---|
| `VARCHAR(n)` | `Variable` | the UTF-8 bytes, no terminator; the tuple stores (offset, length) in its fixed part |
| empty text `''` | `Variable` | `Some(&[])`: an empty field, **not** `NULL` |
| `NULL` | `Variable` | `None`: bit in the NULL bitmap |

`Value::decode(VARCHAR(n), field)` validates what comes from disk: invalid UTF-8 or more than `n` characters → `CorruptedValue` (`XX001`). Writes go through assignment, so either means corruption or the wrong schema.

### Cast and assignment

The standard has two different rules, and ferroDB keeps them apart: `Value::cast` is the explicit `CAST`, `Value::assign` is store assignment (`INSERT`/`UPDATE`, used by `Schema::encode_row`).

| From → to | integer | `VARCHAR(n)` |
|---|---|---|
| integer | range check (`22003`), same in both | `cast`: decimal text, cut to `n` characters (a deviation: the standard raises `22001`); `assign`: `DatatypeMismatch` (`42804`) |
| text | `cast`: parsed like integer input (`22P02` / `22003`); `assign`: `DatatypeMismatch` (`42804`) | `cast`: cut to `n` characters; `assign`: `StringTooLong` (`22001`) past `n`, unless the excess is only spaces (U+0020), which are cut |
| `NULL` | `NULL` | `NULL` |

- `CAST('abcdef' AS VARCHAR(3))` = `'abc'`; assigning `'abcdef'` to a `VARCHAR(3)` column → `22001`; assigning `'abc   '` stores `'abc'`. Tabs or non-breaking spaces in the excess are data, so they still fail.
- `Value::parse(VARCHAR(n), s)` (text input) follows the assignment rule and keeps the text as is: no trimming, and `'NULL'` is the four letters.
- `Display` prints the text unquoted.

### Operators

- **`concat` (`||`):** text with text. `NULL` on either side → `NULL`. With an integer → `UndefinedOperator` (`42883`). The result has no column, so it must fit `VARCHAR(65535)`, with the same trailing-space rule as assignment; otherwise `StringTooLong` (`22001`).
- **Arithmetic** (`+ - * / %`, unary `-`) with text → `UndefinedOperator` (`42883`); with `NULL` on either side the result is still `NULL` first.
- **Comparison:** text compares by its **UTF-8 bytes**, which is the same as code point order, and **without padding**:
  - `'Z' < 'a'`, `'é' > 'z'`, `'ábaco' > 'banana'`, `'10' < '9'`;
  - `'a' < 'a '` and `'a' IS DISTINCT FROM 'a '`;
  - comparing text with an integer → `UndefinedOperator` (`42883`). The error names `=`, the operator every comparison derives from; `is_distinct_from` names `IS DISTINCT FROM`.
- **`sort_cmp`:** the same byte order within text; integers before text; `NULL` last.

### Errors

| `TypeError` | SQLSTATE | Message |
|---|---|---|
| `StringTooLong` | `22001` | `value too long for type varchar(5)` |
| `UndefinedOperator` | `42883` | `operator does not exist: varchar + integer` / `operator does not exist: - varchar` |
| `DatatypeMismatch` | `42804` | `cannot assign a value of type varchar to a column of type integer` |
| `CorruptedValue` | `XX001` | `stored varchar(3) value is corrupted: text is not valid UTF-8` |

Length messages show the full type (`varchar(5)`); operator and assignment messages show only the type name, so a loose text value never appears as `varchar(65535)`.

### Design decisions

See [ADR 0008](decisions/0008-varchar-utf8-byte-order-and-assignment.md). Also:

- **Length counted once, at the cut:** `text.rs` finds the byte index of character `n + 1` with `char_indices().nth(n)`, so the kept part, the excess and the error all come from one pass. It is O(length) on every assignment and every read, which is fine at today's sizes.
- **`Operand` view inside `value.rs`:** operators see a non-NULL value as an integer widened to `i64` or a borrowed `&str`, so each operator is one `match` over two families instead of a combination per width.
- ⚠️ **Characters vs. bytes:** the limit is in characters but tuples and pages are in bytes. A row above the tuple format's 65,535 bytes (possible with `VARCHAR(65535)` and multibyte text) is rejected by `Schema::encode_row` with `RowTooLarge` (`54000`); a smaller row that still does not fit an 8 KB page is rejected by the `HeapFile` with `TupleTooLarge` (both covered by tests). Values larger than a page wait for an overflow mechanism.
- **`into_encoded`:** `encode_row` hands each assigned `Text` its own buffer for the tuple field instead of copying it again, and `||` cuts the joined string in place.
- ⚠️ **Composed characters count as several:** an emoji with a skin tone modifier is two or more `char`s. Grapheme clusters would need a Unicode dependency.

### Tests

- Unit in `value.rs`, `error.rs` and `data_type.rs`: round trip of empty, ASCII, accented and 4-byte characters and of exactly `n` characters; invalid UTF-8 (lone byte, truncated sequence, surrogate) and text longer than the column on decode; `parse` at `n` and `n + 1`, excess of spaces, tabs and U+00A0; every cell of the cast/assign matrix, including `NULL`; `concat` with empty, `NULL`, integers and at the 65,535-character limit; arithmetic with text; byte-order comparison cases; `IS DISTINCT FROM`; mixed `sort_cmp`; every new error with message and SQLSTATE.
- `random_text_operations_should_match_the_char_model`: 10,000 random strings (ASCII, multibyte, tabs, trailing spaces) with random `n`, checked against a `Vec<char>` model for `assign`, `parse`, `cast`, `concat`, `compare` (code point order) and `is_distinct_from`, plus round trips through bytes and text. `sort_cmp_should_be_a_total_order` now mixes text with integers.
- `schema.rs`: text columns round trip, trailing spaces cut, `22001`, `42804` in both directions, a row of 80,000 bytes of text → `RowTooLarge` (`54000`), invalid UTF-8 and text longer than the reading schema's column → `Type { .., CorruptedValue }`; the random assignment and corruption tests include `VARCHAR` columns. Mutation-checked by hand: making the trailing-space rule look for another character breaks the random schema test.
- Integration in [`tests/types_string.rs`](../tests/types_string.rs): a catalog table `(id INTEGER NOT NULL, name VARCHAR(40) NOT NULL, bio VARCHAR(4000))` with empty text, `NULL`, accents, emoji and 3,000-byte values spread over many pages survives eviction with a 2-frame pool and reopening the file; `VARCHAR(1)` and `VARCHAR(65535)` come back from the catalog; a value that fits `VARCHAR(8000)` but not a page → `TupleTooLarge`.

## Data Types: BOOLEAN

`BOOLEAN` holds `TRUE`, `FALSE` or `NULL`. The standard makes `UNKNOWN` the null value of this type, so there is no separate `Unknown` variant: the result of `1 < NULL` and a `NULL` stored in a boolean column are the same `Value::Null`.

```rust
DataType::Boolean
Value::Boolean(bool)
```

`src/types/boolean.rs` (private) holds the byte format, the text input, the integer conversion and Kleene's logic; `value.rs` exposes them.

### Bytes and text

| | |
|---|---|
| Field in the tuple | `Fixed(1)`: `0x00` = FALSE, `0x01` = TRUE |
| Any other byte on read | `CorruptedValue` (`XX001`, `boolean byte must be 0 or 1`): one value never has two encodings |
| Width other than 1 | `InvalidEncoding` (as integers) |
| `Display` | `TRUE` / `FALSE` |
| `Value::parse` | surrounding whitespace (the integers' set), then **only** `true` / `false` in any case (`TRUE`, `True`, `tRuE`) or `1` / `0`. Anything else (`t`, `yes`, `on`, `01`, `+1`, `2`, `unknown`, `NULL`, empty) → `InvalidTextRepresentation` (`22P02`). Never yields `NULL` |

### Cast and assignment

| From → to | integer | `VARCHAR(n)` | `BOOLEAN` |
|---|---|---|---|
| integer | (integers) | (VARCHAR) | `cast`: `0` → FALSE, `1` → TRUE, any other value → `OutOfRange` (`22003`, `boolean out of range`); `assign`: `42804` |
| text | (VARCHAR) | (VARCHAR) | `cast`: the `parse` rules; `assign`: `42804` |
| boolean | `cast`: FALSE → `0`, TRUE → `1`, in every width; `assign`: `42804` | `cast`: `'TRUE'` / `'FALSE'`, cut to `n` like integer → text; `assign`: `42804` | identity |
| `NULL` | `NULL` | `NULL` | `NULL` |

Every pair of types now has a `CAST`. Accepting only `0` and `1` makes integer ⇄ boolean lossless both ways (`CAST(CAST(b AS INTEGER) AS BOOLEAN) = b`), and `2` is an error instead of a silent TRUE.

### Comparison as SQL values

`sql_eq`, `sql_ne`, `sql_lt`, `sql_le`, `sql_gt` and `sql_ge` (`=`, `<>`, `<`, `<=`, `>`, `>=`) return a `Value`: `Boolean` when both sides are values, `Null` (UNKNOWN) when either side is `NULL`. They are a thin layer over `compare`, and an error across families names the operator actually used (`operator does not exist: integer < varchar`). The `sql_` prefix keeps them apart from Rust's structural `==`.

Booleans order `FALSE < TRUE`. `sort_cmp` stays total: integers, then text, then booleans, then `NULL`.

### Three-valued logic

`and`, `or` and `not` follow Kleene's logic (`N` = `NULL`/UNKNOWN):

| `a` | `b` | `a AND b` | `a OR b` |
|---|---|---|---|
| T | T | T | T |
| T | F | F | T |
| T | N | N | T |
| F | F | F | F |
| F | N | F | N |
| N | N | N | N |

The table is symmetric; `NOT T = F`, `NOT F = T`, `NOT N = N`.

- `AND` and `OR` are the **only non-strict operators**: a side that alone decides the result wins over `NULL` (`FALSE AND NULL = FALSE`, `TRUE OR NULL = TRUE`).
- Both operands are type-checked **before** the table: `FALSE AND 1` fails instead of returning FALSE, so the result does not depend on which side is looked at first. A non-boolean operand → `NotBoolean` (`42804`, `argument of AND must be type boolean, not type integer`). An untyped `NULL` is accepted.
- **Boolean tests** `is_true`, `is_false` and `is_unknown` (`IS TRUE`, `IS FALSE`, `IS UNKNOWN`; `IS NOT X` is `!is_x`) return a Rust `bool` and are never UNKNOWN; exactly one of them holds for each boolean or `NULL`. `is_true` is what a `WHERE` filter will keep. A non-boolean → `NotBoolean`.

### Errors

| `TypeError` | SQLSTATE | Message |
|---|---|---|
| `OutOfRange` | `22003` | `boolean out of range` (`CAST(2 AS BOOLEAN)`) |
| `InvalidTextRepresentation` | `22P02` | `invalid input syntax for type boolean: "yes"` |
| `NotBoolean` | `42804` | `argument of AND must be type boolean, not type integer` |
| `CorruptedValue` | `XX001` | `stored boolean value is corrupted: boolean byte must be 0 or 1` |
| `UndefinedOperator` | `42883` | `operator does not exist: boolean = integer` |

### Design decisions

- **One byte, `0`/`1`, strict on read:** the natural format (also PostgreSQL's). Reading "anything non-zero is TRUE" would give one value two encodings, which breaks byte comparisons and future indexes. Bit-packing several booleans per byte was not worth its complexity.
- **UNKNOWN = `Value::Null`:** two kinds of null would double every case.
- **`TypeFamily`** (`pub(crate)`, `DataType::family()`): with more than two families, "same family" can no longer be `is_text() == is_text()`; assignment and comparison ask `family()`, and its order (numbers, text, booleans, then datetimes) is the cross-family order of `sort_cmp`.
- **`CorruptedValue`** generalizes the former `CorruptedText`: one variant with the type and a reason serves every type whose bytes can be invalid at the right width.
- **`NotBoolean` instead of `UndefinedOperator`** for logical operators: `UndefinedOperator` reads "type op type", which fits neither `NULL AND 'x'` (the `NULL` has no type) nor the postfix `1 IS TRUE`.

### Tests

- Unit in `data_type.rs`, `error.rs` and `value.rs`: name, width, layout and family; every byte and width on decode; `parse` with mixed case, whitespace and a list of rejected inputs; every cell of the cast/assign matrix including every integer width, `-1`, `2`, `MIN` and `MAX`; `FALSE < TRUE`; the six `sql_*` operators over integers, text and booleans, with `NULL` and with the operator named in the error; the complete `AND` / `OR` / `NOT` tables; a non-boolean operand even when the other side decides; the three `IS` tests; every error with message and SQLSTATE.
- `random_logic_should_match_the_kleene_order_model`: 10,000 pairs checked against Kleene's logic written as an order (`FALSE < UNKNOWN < TRUE`; `AND` = minimum, `OR` = maximum), plus commutativity, double negation, De Morgan and "exactly one `IS` holds". Mutation-checked by hand: dropping the `UNKNOWN AND FALSE = FALSE` case makes it fail.
- `random_sql_comparisons_should_agree_with_compare` over mixed families; `booleans_should_round_trip_through_bytes_text_and_integers`; `sort_cmp_should_be_a_total_order` now mixes all three families.
- `schema.rs` and `catalog`: boolean columns (`Fixed(1)`), round trip, `42804` in both directions, the random assignment and corruption tests with boolean columns and values, catalog code `5` and random creations with `BOOLEAN`.
- Integration in [`tests/types_boolean.rs`](../tests/types_boolean.rs): a catalog table `(id INTEGER NOT NULL, active BOOLEAN NOT NULL, verified BOOLEAN)` with 2,000 rows survives eviction with a 2-frame pool and reopening the file; a `WHERE active AND verified` filter written with `and` + `is_true`, and `verified IS UNKNOWN`, return the expected rows; a boolean byte `2` written below the schema → `Type { .., CorruptedValue }` (`XX001`).

## Data Types: REAL and DOUBLE PRECISION

`REAL` is IEEE 754 binary32 (`f32`) and `DOUBLE PRECISION` binary64 (`f64`). They are *approximate* numbers (`0.1 + 0.2 <> 0.3`) and share the **numeric family** with the integers: they mix in arithmetic and comparison without a `CAST`. See [ADR 0009](decisions/0009-floats-finite-exact-comparison-numeric-family.md).

```rust
DataType::Real, DataType::Double
Value::Real(Finite32), Value::Double(Finite64)
Finite64::new(1.5)                    // Option: None for NaN and ±infinity; -0.0 becomes 0.0
```

`src/types/float.rs` (private) holds the finite newtypes, the text input and output, the arithmetic, the conversions and the exact comparison with integers.

### Only finite values

SQL has no NaN or infinity, so ferroDB has none either: `Finite32` / `Finite64` are built only from finite values and fold `-0.0` into `0.0`. A `Value` can never hold NaN, which keeps `Value: Eq`; without NaN and `-0.0`, `total_cmp` is the numeric order and equal values have equal bits, so every value has a single encoding.

### Bytes

| Type | `ColumnLayout` | Field in the tuple |
|---|---|---|
| `REAL` | `Fixed(4)` | IEEE 754 binary32, little-endian |
| `DOUBLE PRECISION` | `Fixed(8)` | IEEE 754 binary64, little-endian |

Reading NaN, ±infinity (`float must be finite`) or `-0.0` (`negative zero is never stored`) → `CorruptedValue` (`XX001`); a wrong width → `InvalidEncoding`.

### Text

- **Input** (`Value::parse`): surrounding whitespace, optional sign, digits with an optional point and an optional exponent (`1`, `-2.5`, `.5`, `5.`, `1e10`, `1.5E-3`).
  - `NaN`, `inf`, `Infinity`, hexadecimal, `1_000`, `1,5`, `.`, `e5`, empty → `InvalidTextRepresentation` (`22P02`). The syntax is checked by hand first, because Rust's `str::parse` (which rounds correctly) would accept `inf` and `NaN`.
  - `REAL` parses straight to `f32`; going through `f64` would round twice (`'16777217'` → `16777216`).
  - Too large for the type (`1e309`, or `3.5e38` for `REAL`), or a non-zero literal that rounds to zero (`1e-400`) → `TextOutOfRange` (`22003`).
- **Output** (`Display`): the **shortest digits that round-trip** (Rust's `{:e}`), laid out like C's `%g`, as PostgreSQL prints floats: positional when the decimal exponent is in `[-4, 15)` for `DOUBLE PRECISION` and `[-4, 6)` for `REAL` (`f64::DIGITS`, `f32::DIGITS`), scientific with a signed two-digit exponent otherwise.

| Value | `DOUBLE PRECISION` | `REAL` |
|---|---|---|
| `1.5`, `3` | `1.5`, `3` | `1.5`, `3` |
| `0.0001`, `0.00001` | `0.0001`, `1e-05` | `0.0001`, `1e-05` |
| `123456`, `1e6` | `123456`, `1000000` | `123456`, `1e+06` |
| `1e15` | `1e+15` | `1e+15` |
| `0.1 + 0.2` | `0.30000000000000004` | — |
| maximum | `1.7976931348623157e+308` | `3.4028235e+38` |

`parse(display(x)) == x` for every value.

### Arithmetic

| Operands | Result type |
|---|---|
| `REAL` op `REAL` | `REAL` |
| any other mix with a float (`REAL` op `DOUBLE`, integer op float) | `DOUBLE PRECISION` |

- Operands are widened to `f64` and the result is narrowed to its type, like the integers in `i64`. For `REAL`, rounding the `f64` result to `f32` equals computing in `f32` (double rounding is harmless for `+ - * /`, since 53 ≥ 2·24 + 2); the randomized test checks it against native `f32`.
- **Overflow** (infinite in the result type) → `OutOfRange` (`22003`).
- **Underflow**: a result that is zero while the exact result is not (`MIN_POSITIVE * MIN_POSITIVE`) → `OutOfRange` (`22003`), as in PostgreSQL. Subnormal results are kept.
- **`x / 0`** → `DivisionByZero` (`22012`), never IEEE's infinity.
- **`%` with a float** → `UndefinedOperator` (`42883`): SQL's `MOD` is defined for exact numbers only.
- `NULL` stays strict; text or booleans → `42883`.

### Comparison

- Float with float: numeric (`total_cmp`).
- Integer with float: **exact**, by algebraic value, as the standard asks. The integer is never converted to `f64`: outside `[-2⁶³, 2⁶³)` the float decides; inside, the integer is compared with the float's integral part and a tie is broken by the fraction. So `9007199254740993 > 9007199254740992.0`, where PostgreSQL (which converts to `float8`) says they are equal.
- `sql_*`, `is_distinct_from` and `sort_cmp` follow: in `sort_cmp`, integers and floats interleave by value (`2` and `2.0` tie).

### Cast and assignment

| From → to | integer | float | `VARCHAR(n)` | `BOOLEAN` |
|---|---|---|---|---|
| integer | (integers) | rounds to the nearest `f32`/`f64` (straight from `i64`) | (integers) | (BOOLEAN) |
| float | `cast`: rounds **half to even** (`2.5` → `2`, `3.5` → `4`), out of range → `22003`; `assign`: `42804` | `DOUBLE` → `REAL` rounds; overflow or underflow → `22003` | `cast`: the `Display` text cut to `n`; `assign`: `42804` | `CannotCast` (`42846`) |
| text | (VARCHAR) | `cast`: `parse`; `assign`: `42804` | (VARCHAR) | (BOOLEAN) |
| boolean | (BOOLEAN) | `CannotCast` (`42846`) | (BOOLEAN) | (BOOLEAN) |
| `NULL` | `NULL` | `NULL` | `NULL` | `NULL` |

- **Assignment is asymmetric** inside the numeric family: an integer goes into a float column, but a float goes into an integer column only through an explicit `CAST`, so a fraction is never rounded away by an `INSERT`.
- Floats and booleans have no `CAST` in either direction (`cannot cast type double precision to boolean`), as in PostgreSQL; this is the only pair of types without one.

### Errors

| `TypeError` | SQLSTATE | Message |
|---|---|---|
| `OutOfRange` | `22003` | `double precision out of range` (overflow, underflow, `DOUBLE` → `REAL`, float → integer) |
| `TextOutOfRange` | `22003` | `value "1e400" is out of range for type double precision` |
| `InvalidTextRepresentation` | `22P02` | `invalid input syntax for type real: "NaN"` |
| `DivisionByZero` | `22012` | `division by zero` |
| `CannotCast` | `42846` | `cannot cast type double precision to boolean` |
| `DatatypeMismatch` | `42804` | `cannot assign a value of type double precision to a column of type integer` |
| `CorruptedValue` | `XX001` | `stored double precision value is corrupted: float must be finite` |

### Design decisions

See [ADR 0009](decisions/0009-floats-finite-exact-comparison-numeric-family.md). Also:

- **`Finite32` / `Finite64` generated by one `macro_rules!`**: the two types differ only in the float type; a macro avoids two copies of the same forty lines.
- **`TypeFamily::Integer` became `Numeric`**: assignment and comparison ask `family()`, so integers and floats mix without new checks; `is_integer` stays for the exact types and `is_numeric` covers both.
- **Underflow is an error** so that operands that are not zero never produce zero silently; subnormals, which still carry the value, are kept.
- **Half to even** for float → integer (`f64::round_ties_even`) avoids a bias in sums of rounded values; it is what PostgreSQL does for floats (`NUMERIC` rounds half away from zero).

### Tests

- Unit in `float.rs`, `value.rs`, `data_type.rs`, `error.rs`: the finite newtypes (limits, subnormals, `±0`, NaN, ±infinity, order); bytes and every corrupt pattern; every accepted and rejected text form, overflow and underflow per type; the output table and the exponent boundaries; result types of every mix; overflow, underflow, subnormals, exact zero, division by zero, `%`; exact comparison at `2⁵³`, `2⁶³`, negatives and fractions; every cell of the cast/assign matrix with half-to-even cases and range limits; every new error with message and SQLSTATE.
- `random_float_arithmetic_should_match_native_ieee_754_arithmetic`: 10,000 operations against native `f32`/`f64` arithmetic plus the SQL rules written apart. Mutation-checked by hand: disabling the underflow rule makes it fail.
- `random_integer_float_comparisons_should_match_an_exact_i128_model`: 10,000 comparisons against an exact model through `i128` and `floor` (a different route from the implementation's `trunc`), with values next to `2⁵³` and `2⁶³`. Mutation-checked by hand: ignoring the fraction makes it fail.
- `random_floats_should_round_trip_through_bytes_and_text`; `sort_cmp_should_be_a_total_order` and the random `sql_*` test now include floats.
- `schema.rs` and `catalog`: `Fixed(4)` / `Fixed(8)` columns, integer into a float column, float into an integer column (`42804`), the random assignment model with `REAL` overflow and underflow, catalog codes `6` and `7` and random creations with both types.
- Integration in [`tests/types_float.rs`](../tests/types_float.rs): a catalog table `(id INTEGER NOT NULL, price DOUBLE PRECISION NOT NULL, ratio REAL)` with 2,000 rows (limits, subnormals, zero, `NULL`) survives eviction with a 2-frame pool and reopening the file; integers assigned to float columns come back as floats; a NaN written below the schema → `Type { .., CorruptedValue }` (`XX001`).

## Data Types: DATE

`DATE` is a calendar day without a time of day, from **0001-01-01 to 9999-12-31** (the range of the SQL standard), in the **proleptic Gregorian calendar**: today's leap rule (every 4 years, except centuries, except every 400) applied to every year. `TIMESTAMP` has its own section.

```rust
DataType::Date
Value::Date(Date)
Date::from_ymd(2024, 2, 29)          // Option: None for a day that does not exist or a year outside 1..=9999
Date::from_days_since_epoch(19_782)  // Option: None outside MIN..=MAX
```

`src/types/date.rs` (private; `Date` is re-exported) holds the calendar, the text input, the day arithmetic and the decoding.

### Representation

A `Date` is the **number of days since 1970-01-01** in an `i32` (`Date::MIN` = -719,162, `Date::MAX` = 2,932,896), like the "integer since an epoch" of real databases (PostgreSQL counts from 2000-01-01). Comparing and adding days are integer operations; the calendar only matters when converting to and from year, month and day.

The conversion uses Howard Hinnant's `days_from_civil` / `civil_from_days` ([date algorithms](http://howardhinnant.github.io/date_algorithms.html)), closed formulas that count years from March (so February and its leap day close the year) and repeat every 400-year era of 146,097 days. No tables, no loops, no dependency.

| Type | `ColumnLayout` | Field in the tuple |
|---|---|---|
| `DATE` | `Fixed(4)` | `i32` LE, days since 1970-01-01 |

A day number outside `MIN..=MAX` read from disk → `CorruptedValue` (`XX001`, `date out of range`); a wrong width → `InvalidEncoding`.

### Text

- **Input** (`Value::parse`): surrounding whitespace, then **exactly** `YYYY-MM-DD` (4 + 2 + 2 ASCII digits and two hyphens), the standard's date literal.
  - Any other shape (`2024-1-5`, `2024/01/05`, `20240105`, `24-01-05`, `2024-01-05T00:00`, `10000-01-01`, `today`) → `InvalidTextRepresentation`, SQLSTATE `22007` (*invalid datetime format*).
  - The right shape for a day that does not exist or is outside the range (`2023-02-29`, `1900-02-29`, `2024-04-31`, `2024-13-01`, `0000-01-01`) → `TextOutOfRange`, SQLSTATE `22008` (*datetime field overflow*).
- **Output** (`Display`): `YYYY-MM-DD` with a 4-digit, zero-padded year (`0001-01-01`). `parse(display(d)) == d`.

### Arithmetic in days

The standard does date arithmetic through an interval type, which ferroDB does not have. Like PostgreSQL, ferroDB accepts a number of days instead (decided with the author):

| Operation | Result |
|---|---|
| `DATE + integer`, `integer + DATE` | `DATE`, that many days later |
| `DATE - integer` | `DATE` |
| `DATE - DATE` | `INTEGER`, the days between them (the largest difference, about 3.65 million, always fits) |
| anything else with a `DATE` (`*`, `/`, `%`, unary `-`, `DATE + DATE`, `integer - DATE`, floats, text, booleans) | `UndefinedOperator` (`42883`) |

- The sum is computed in `i64`, so even `DATE + BIGINT::MAX` is an error, never a wrap; a result outside 0001..=9999 → `OutOfRange` (`date out of range`, SQLSTATE `22008`).
- `NULL` stays strict. Adding months or years (`+ 1 month`) is not supported.

### Comparison

A `DATE` compares with a `DATE` by day number, and with a `TIMESTAMP` as midnight of its day (see [TIMESTAMP](#comparison-2)); with any other family → `42883`, naming the operator used. `sql_*`, `is_distinct_from` and `sort_cmp` follow; in `sort_cmp`, dates come after booleans, and `NULL` stays last.

### Cast and assignment

| From → to | `DATE` | `VARCHAR(n)` | integer, float, boolean |
|---|---|---|---|
| `DATE` | identity | `cast`: `YYYY-MM-DD` cut to `n`; `assign`: `42804` | `CannotCast` (`42846`) |
| text | `cast`: the `parse` rules; `assign`: `42804` | — | — |
| integer, float, boolean | `CannotCast` (`42846`) | — | — |
| `NULL` | `NULL` | `NULL` | `NULL` |

### Errors

There is no new `TypeError` variant: the standard gives datetimes their own SQLSTATE class, so the existing variants report a different code for a `DATE`, with the same message:

| `TypeError` | Numbers | `DATE` | `DATE` message |
|---|---|---|---|
| `InvalidTextRepresentation` | `22P02` | `22007` | `invalid input syntax for type date: "2024/01/05"` |
| `TextOutOfRange` | `22003` | `22008` | `value "2023-02-29" is out of range for type date` |
| `OutOfRange` | `22003` | `22008` | `date out of range` |

`CannotCast` (`42846`), `UndefinedOperator` (`42883`), `DatatypeMismatch` (`42804`) and `CorruptedValue` (`XX001`) are reused as they are.

### Design decisions

- **Days since 1970-01-01 in `i32`**: comparison and arithmetic are integer operations, and 4 bytes cover 0001–9999 with room to spare. Packing year, month and day would complicate both; the Unix epoch is the better known one.
- **The standard's range and only the ISO format** (decided with the author): always a 4-digit year, no `BC` suffix, no ambiguity between `01/02` as January or February, no `DateStyle` setting.
- **Newtype `Date`** with validating constructors, like `Finite64`: an invalid `Value::Date` cannot exist.
- **`TypeFamily::Datetime`**, which `TIMESTAMP` joined without new family rules; in the family order it comes last, after booleans.
- **SQLSTATE by type** instead of date-specific variants: the message is the same, only the standard's class differs.

### Tests

- Unit in `date.rs`: day 0 is 1970-01-01; the range limits; February 29 in leap and non-leap years (4, 1600, 2000, 2024 vs. 1900, 2023, 2100); every month's last day and the day after; fields out of range; consecutive days across month and year ends; `Display`.
- `every_day_of_the_range_should_follow_the_previous_one`: **all 3,652,059 days**, checking that each converts to the calendar successor of the previous one and back. Random samples were not enough: a wrong century term in Hinnant's formulas (`36_525` instead of `36_524`) only changes a few days every 400 years and survived 2,000 random days; the exhaustive walk catches it, as well as a broken leap rule (both mutation-checked by hand).
- `random_days_should_match_a_naive_calendar`: 2,000 random days against a year-by-year, month-by-month walk from 0001-01-01.
- `value.rs`: bytes and every corrupt pattern; every accepted and rejected text, with the right SQLSTATE; comparison and the `sql_*` operators; day arithmetic at the range ends and with `i64` extremes; every refused operator; every cast/assign cell. `random_date_arithmetic_should_match_an_i64_model` and `random_dates_should_round_trip_through_bytes_and_text` (10,000 each); `sort_cmp_should_be_a_total_order` now mixes all five families.
- `schema.rs` and `catalog`: `Fixed(4)` columns, round trip, `42804` both ways, the random assignment and corruption tests with dates, catalog code `8` and random creations with `DATE`.
- Integration in [`tests/types_date.rs`](../tests/types_date.rs): a catalog table `(id INTEGER NOT NULL, born DATE NOT NULL, died DATE)` with 2,000 rows (range limits, leap days, the epoch, `NULL`) survives eviction with a 2-frame pool and reopening the file; `died - born` and `born > DATE '2000-01-01'` on the stored rows give the expected results; a day number past 9999-12-31 written below the schema → `Type { .., CorruptedValue }` (`XX001`).

## Data Types: TIMESTAMP

`TIMESTAMP` is a date and a time of day to the microsecond (`2024-03-15 14:30:00.25`), in the same range as `DATE`: **0001-01-01 00:00:00 to 9999-12-31 23:59:59.999999**. It shares the `Datetime` family with `DATE`.

```rust
DataType::Timestamp
Value::Timestamp(Timestamp)
Timestamp::new(date, 14, 30, 0, 250_000)  // Option: None for hour > 23, minute or second > 59, micro > 999,999
Timestamp::at_midnight(date)
Timestamp::from_micros_since_epoch(n)     // Option: None outside MIN..=MAX
```

`src/types/timestamp.rs` (private; `Timestamp` is re-exported) holds the newtype, the split into day and time of day, the text input and the decoding. The day part is a `Date`, so the calendar stays in `date.rs`, checked day by day.

### Representation

A `Timestamp` is the number of **microseconds since 1970-01-01 00:00:00** in an `i64` (`Timestamp::MIN` = -62,135,596,800,000,000, `Timestamp::MAX` = 253,402,300,799,999,999), the precision the SQL standard sets as default and the one PostgreSQL uses. The epoch is the one of `DATE`, so a day is exactly 86,400,000,000 µs.

The day and the time of day are split with **floor division** (`div_euclid` / `rem_euclid`): the microsecond before 1970 is `1969-12-31 23:59:59.999999`, not a negative time on 1970-01-01.

| Type | `ColumnLayout` | Field in the tuple |
|---|---|---|
| `TIMESTAMP` | `Fixed(8)` | `i64` LE, microseconds since 1970-01-01 00:00:00 |

A value outside `MIN..=MAX` read from disk → `CorruptedValue` (`XX001`, `timestamp out of range`); a wrong width → `InvalidEncoding`.

### Text

- **Input** (`Value::parse`): surrounding whitespace, then one of three forms (decided with the author):
  - `YYYY-MM-DD HH:MM:SS`, with an optional fraction of 1 to 6 digits (`.5`, `.123456`), the standard's literal;
  - the same with an uppercase `T` instead of the space, as in ISO 8601: `2024-03-15T14:30:00`;
  - the date alone, `YYYY-MM-DD`, which is midnight.

  The date follows the rules of `DATE`; hours `00`–`23`, minutes and seconds `00`–`59`, always two digits. The whole shape is checked before any range:
  - any other shape (`2024-03-15 14:30` without seconds, `2:30:00`, lowercase `t`, two spaces, `2024-03-15T`, more than 6 fraction digits, a suffix such as `Z` or `+01`, a time alone) → `InvalidTextRepresentation`, SQLSTATE `22007`, even if the date is also invalid (`2023-02-29 25:00` is a syntax error);
  - the right shape for a value that does not exist (`2023-02-29 10:00:00`, `24:00:00`, minute or second `60`, year `0000`) → `TextOutOfRange`, SQLSTATE `22008`.
- **Output** (`Display`): always `YYYY-MM-DD HH:MM:SS`, with the fraction only when it is not zero and without trailing zeros (`14:30:00.25`, `09:05:07.000001`), as PostgreSQL. `parse(display(t)) == t`.

`date.rs` and `timestamp.rs` share the reading of `YYYY-MM-DD` and a small `DatetimeTextError` (`Syntax` / `OutOfRange`) that each type turns into its own `TypeError`.

### Comparison

- `TIMESTAMP` with `TIMESTAMP`: by microsecond.
- `DATE` with `TIMESTAMP`: the `DATE` is **midnight of its day**, as the standard extends a date with a zero time. `DATE '2024-03-15' = TIMESTAMP '2024-03-15 00:00:00'` and `DATE '2024-03-15' < TIMESTAMP '2024-03-15 00:00:00.000001'`. The comparison is exact: days times 86,400,000,000 fit an `i64` over the whole range.
- Any other family → `42883`, naming the operator. `sql_*`, `is_distinct_from` and `sort_cmp` follow; in `sort_cmp`, dates and timestamps interleave by instant.

### Arithmetic

None (decided with the author): every arithmetic operator with a `TIMESTAMP` (`+`, `-`, `*`, `/`, `%`, unary `-`, with integers, dates or other timestamps) → `UndefinedOperator` (`42883`). Unlike `DATE`, there is no natural unit: days would drop the time, and in PostgreSQL `TIMESTAMP - TIMESTAMP` gives an interval, a type ferroDB does not have. `NULL` stays strict.

### Cast and assignment

| From → to | `TIMESTAMP` | `DATE` | `VARCHAR(n)` | integer, float, boolean |
|---|---|---|---|---|
| `TIMESTAMP` | identity | `cast`: its day (the time is dropped, floor division before 1970); `assign`: `42804` | `cast`: the text cut to `n`; `assign`: `42804` | `CannotCast` (`42846`) |
| `DATE` | `cast` and `assign`: midnight | (`DATE`) | (`DATE`) | (`DATE`) |
| text | `cast`: the `parse` rules; `assign`: `42804` | (`DATE`) | — | — |
| integer, float, boolean | `CannotCast` (`42846`) | (`DATE`) | — | — |
| `NULL` | `NULL` | `NULL` | `NULL` | `NULL` |

**Assignment is asymmetric** inside the family, as integers and floats are: a `DATE` goes into a `TIMESTAMP` column (midnight, nothing lost), but a `TIMESTAMP` goes into a `DATE` column only through an explicit `CAST`, since it would drop the time silently.

### Errors

No new variant; the datetime SQLSTATEs of `DATE` apply to the whole family:

| `TypeError` | SQLSTATE | Message |
|---|---|---|
| `InvalidTextRepresentation` | `22007` | `invalid input syntax for type timestamp: "2024-03-15 14:30"` |
| `TextOutOfRange` | `22008` | `value "2024-03-15 24:00:00" is out of range for type timestamp` |
| `CorruptedValue` | `XX001` | `stored timestamp value is corrupted: timestamp out of range` |
| `UndefinedOperator` | `42883` | `operator does not exist: timestamp + integer` |
| `CannotCast` | `42846` | `cannot cast type timestamp to bigint` |
| `DatatypeMismatch` | `42804` | `cannot assign a value of type timestamp to a column of type date` |

### Design decisions

- **Microseconds since 1970 in `i64`** (decided with the author): the standard's default precision; the range uses about 58 of the 63 bits. Nanoseconds would not fit 0001–9999 in an `i64`; a separate date and time (12 bytes) would complicate comparing and sorting.
- **Reusing `Date`** for the day and for `YYYY-MM-DD`: one calendar, checked on all 3,652,059 days.
- **Three input forms, one output form**: the `T` of ISO 8601 (JSON, APIs) and a bare date are accepted to ease pasting values from elsewhere; the output stays single (with a space), so the round trip is exact.
- **No leap seconds**: the time of day stops at `23:59:59.999999`. PostgreSQL reads `:60` as the next minute, which would give one instant two texts.
- **No new error variant**: the SQLSTATE by family already covers `TIMESTAMP`.

### Tests

- Unit in `timestamp.rs`: microsecond 0 is 1970-01-01 00:00:00; the microsecond before 1970; the range limits; every time field one past its limit; `date`/`time_of_day` inverting `new`; `at_midnight`; `Display` with and without a fraction.
- `random_instants_should_split_into_day_and_time_like_the_model`: 10,000 instants (anywhere, or next to a midnight, the epoch or the range ends) against a split with truncating `/` and `%` corrected for negatives. Mutation-checked by hand: replacing `div_euclid` with `/`, or `rem_euclid` with `%`, makes it fail.
- `value.rs`: bytes and every corrupt pattern; the three accepted forms and a list of rejected ones with their SQLSTATE, including syntax over range; comparison with timestamps and dates; every refused operator; every cast/assign cell. `random_timestamps_should_round_trip_through_bytes_and_text` and `random_datetime_comparisons_should_match_the_instant_model` (10,000 each; comparing a date without converting it to midnight fails the latter, mutation-checked by hand); `sort_cmp_should_be_a_total_order` includes timestamps.
- `schema.rs` and `catalog`: `Fixed(8)` columns, round trip, a date into a timestamp column, a timestamp into a date column (`42804`), the random assignment and corruption tests with timestamps, catalog code `9` and random creations with `TIMESTAMP`.
- Integration in [`tests/types_timestamp.rs`](../tests/types_timestamp.rs): a catalog table `(id INTEGER NOT NULL, created TIMESTAMP NOT NULL, closed TIMESTAMP, due DATE)` with 2,000 rows (range ends, instants before 1970, fractions, `NULL`) survives eviction with a 2-frame pool and reopening the file; `created < due` and `due = CAST(due AS TIMESTAMP)` hold on the stored rows; an instant past the range written below the schema → `Type { .., CorruptedValue }` (`XX001`).

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
  - the value is converted to the column type with `Value::assign`: a `SMALLINT` in a `BIGINT` column is accepted; `70000` in a `SMALLINT` column → `Type { column: "age", OutOfRange }`; an integer in a float column is accepted, a float in an integer column → `Type { .., DatatypeMismatch }` (only a `CAST` converts); text longer than a `VARCHAR(n)` column → `Type { .., StringTooLong }` unless the excess is only spaces; a value of another family (text in an integer column, a boolean in a text column, ...) → `Type { .., DatatypeMismatch }`;
  - valid values whose tuple exceeds the tuple format's limit (65,535 bytes, reachable only with long text) → `RowTooLarge`, not the `Tuple` error, which means corruption.
- **`decode_row` / `decode_column` / `decode_columns`** read bytes from disk and do not trust them: truncated or corrupted tuple, or one written with another schema → `Tuple(..)`; `NULL` stored in a `NOT NULL` column → `NullViolation`. Values come back **in the column's type** (a `SmallInt` stored in an `INTEGER` column comes back as `Integer`).
  - Every field is read with `TupleRef::field`, which returns an error: a field the tuple cannot produce is never mistaken for `NULL`.
  - `decode_columns` returns the requested columns in the requested order (repetitions allowed), checks the indexes **before** reading the tuple and validates the tuple only once.
- Each field's width comes from the schema itself, so `Type { .. InvalidEncoding }` does not happen on read; on read, the only type error is `Type { .. CorruptedValue }` for a damaged text field.

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
| `RowTooLarge` | `54000` | `row of 80007 bytes exceeds the maximum of 65535 bytes` |

### Design decisions

- **`NOT NULL` already in the schema**, as a column flag checked by `encode_row`: one line of code, and it avoids changing the schema format once the catalog stores it. The **NOT NULL** constraint in the [roadmap](../ROADMAP.md#constraints) is only marked when DDL (`CREATE TABLE ... NOT NULL`) exists.
- **Store assignment** (`Value::assign`) instead of requiring the exact type: it is the standard semantics for `INSERT`/`UPDATE`. It accepts any width within a family and differs from `CAST` for text (no silent cut, no integer ⇄ text).
- **A row is a `Vec<Value>`:** a `struct Row` would have no invariant of its own beyond what `Schema` validates.
- **63-byte name limit:** fits comfortably in a 1-byte length prefix in the catalog. The standard's 128 characters can exceed 255 bytes in UTF-8.
- **At least one column**, as in the standard. A table without columns would complicate the catalog and the executor for no benefit here.
- **`decode_column` / `decode_columns`:** `TupleRef` accesses a field in O(1); projections and filters will read few columns per row, and `decode_columns` avoids validating the same tuple once per column.
- **Truncated name in messages:** `ColumnNameTooLong` keeps the whole name, but `Display` shows only the prefix that would fit (cut at a character boundary) followed by `…`, so a huge name does not become a huge message.
- ⚠️ **A row larger than a page:** a valid schema can describe rows that do not fit in a page (e.g. 1,600 `BIGINT` columns ≈ 13 KB > 8 KB). `Schema` does not know the page size; the `HeapFile` rejects such rows with `TupleTooLarge` (covered by an integration test), and `min_row_size()` lets `create_table` reject the schema up front (see [Catalog](#catalog)).
- ⚠️ **`NullViolation` has two meanings:** a user value in `encode_row`, or inconsistent data on disk in `decode_*` (same variant, message with the column name).

### Tests

- Unit in `schema.rs`: `Column`, every `Schema::new` rule with boundaries (1,600/1,601 columns, 63/64-byte names including multibyte, `id` vs. `ID`), every `SchemaError` with message, SQLSTATE and `source()`, `encode_row` (width cast, does not fit, `NULL` allowed and forbidden, arity, first error wins), `decode_row` / `decode_column` / `decode_columns` (round trip with boundaries and `NULL`, column types, requested order with repetition, empty list, out of range before reading the tuple, truncated, written with another schema, `NULL` on disk in a `NOT NULL` column), truncated long-name message (ASCII and multibyte).
- `random_rows_should_follow_the_assignment_rules`: 2,000 random schemas (integers and short `VARCHAR`s) and rows (integers and text) compared with rules computed by hand (without `Value::assign`), read back with `decode_row`, `decode_column` and `decode_columns` in reverse order. Mutation-checked by hand during development: disabling `NOT NULL` or the cast makes it fail.
- `random_corruption_should_never_panic`: 2,000 valid tuples with flipped bits or truncated.
- Integration in [`tests/table_schema.rs`](../tests/table_schema.rs): 1,000 typed rows in a `HeapFile` survive eviction and reopening the file, `decode_column` projects one column of the stored rows, rows rejected by the schema never reach the heap, a row from a schema wider than a page is rejected by the heap with `TupleTooLarge`.

---

## Table Heap

`TableHeap` (`src/table/table_heap.rs`) is a table's rows as typed values. It joins the two pieces below it: `Schema` validates and encodes a row, and `HeapFile` stores the bytes. The executor's operators (`INSERT`, `SELECT`, `UPDATE`, `DELETE`, sequential scan) will talk only to it.

```rust
let accounts = catalog.table_heap(&mut bpm, "accounts")?;            // opened on first use, then cached
let rid  = accounts.insert(&mut bpm, &[Value::Integer(1), Value::Null, text])?;
let row  = accounts.get(&mut bpm, rid)?;                            // Vec<Value>, in the column types
let rid  = accounts.update(&mut bpm, rid, &new_row)?;               // may return a different Rid
accounts.delete(&mut bpm, rid)?;
for entry in accounts.scan(&mut bpm) {                              // Result<(Rid, Vec<Value>), TableError>
    let (rid, row) = entry?;
}
```

`TableHeap::create(bpm, schema)` and `TableHeap::open(bpm, schema, first_page_id)` build one directly; the catalog uses them, and nothing above the catalog needs to.

### Operations

- **`insert` / `update`** run `Schema::encode_row` **before** touching the heap: arity, `NOT NULL` and store assignment (`22001`, `22003`, `42804`, ...) are checked first, so a rejected row writes nothing, and a rejected `update` leaves the old row as it was.
- **`update` can move the row.** When the new row no longer fits its page, the heap stores it on another page and deletes the old one: the returned `Rid` is the one to use from then on, and the old `Rid` gives `TupleNotFound`.
- **`get` / `scan`** decode through the schema and never trust the disk. Values come back in the column's type.
- **`scan`** yields the live rows in heap order (page by page, slot by slot). A row that does not decode is an `Err` for that row (`XX001`), never skipped, and the scan goes on to the next one.
- A `Rid` of another table, or of a deleted row, is an error, never another row.

### Errors

`TableError` wraps the error of the layer that failed. `Display` and `source()` show the wrapped error.

| `TableError` | SQLSTATE | When |
|---|---|---|
| `Schema(..)` | the `SchemaError`'s (`23502`, `42601`, `22001`, `22003`, `42804`, `54000`, `XX001`, ...) | the row is rejected by the schema, or a stored row does not decode |
| `Heap(TupleTooLarge)` | `54000` | the row fits its types but not a page: a limit of the system, not of the data |
| `Heap(BufferPool(ChecksumMismatch \| Page \| Disk(TornPage)))` | `XX001` | bytes damaged on disk |
| `Heap(BufferPool(Disk(..)))` (other) | `58030` | the disk failed (I/O error) |
| `Heap(BufferPool(NoAvailableFrame))` | `53000` | every frame of the pool is pinned |
| `Heap(BufferPool(PageIdOverflow))` | `54000` | no page ids left |
| `Heap(CorruptedChain \| Page)` | `XX001` | damaged heap pages |
| `Heap(TupleNotFound \| PageNotInHeap \| UnsupportedPageSize)` | `XX000` | a `Rid` of another table or of a deleted row: a mistake of the caller |

### Design decisions

- **A thin layer.** Validation lives in `Schema` and storage in `HeapFile`; `TableHeap` only orders the calls (encode, then write) and converts the errors. Each piece stays testable on its own.
- **No table names here.** `TableHeap` does not know the catalog, which would be a dependency pointing up (`catalog` → `table`). Finding a table by name belongs to the catalog (see [Catalog](#catalog)).
- **One handle per table, kept by the catalog.** Each `HeapFile` keeps its page list and free space map in memory. Two handles on the same heap work, but each only finds out late what the other wrote (see [Heap File](storage-engine.md#heap-file)). The catalog opens a table's `TableHeap` the first time it is asked for and hands out the same one after that, so whoever asks by name does not have to keep anything.
- **The `BufferPoolManager` is a parameter of every call**, never stored, as in `HeapFile`: it keeps the borrows simple (`&mut Catalog` and `&mut BufferPoolManager` are separate).
- **One SQLSTATE per storage failure.** `heap_sqlstate` / `buffer_pool_sqlstate` (in `table_heap.rs`) map storage errors to codes, and `CatalogError` uses the same functions, so a checksum mismatch is `XX001` whether a table or the catalog hit it. Damaged bytes are data corruption (`XX001`), not an I/O error (`58030`); running out of frames is a lack of resources (`53000`).
- **Corrupt rows stop nobody.** `scan` returns an error for the damaged row instead of ending the iteration, so a caller can decide whether to stop or to skip it.

### Tests

- Unit in `table_heap.rs`: create and reopen (schema kept, the reserved page rejected), insert/get round trip, `get` of a row that does not decode (`XX001`), the assigned row stored (`SMALLINT` in an `INTEGER` column, trailing spaces cut), every rejection (`23502`, `42601`, `42804`, `22001`, `22003`) leaving the heap empty, a row that fits its types but not a page (`54000`), `get`/`update`/`delete` of a `Rid` outside the table, `update` in place and `update` that moves the row (the old `Rid` is gone), a rejected `update` keeping the old row, `delete` only once, scan of an empty table, scan across pages after deletes and updates, a row written below the schema reported as `XX001` while the scan goes on, and every error with SQLSTATE (including each storage failure: checksum, page header, torn file, I/O, no frame, no page ids) and `source()`.
- `random_operations_should_match_model`: 2,000 random inserts, updates (including ones that move the row), deletes and gets, with rows the schema sometimes rejects, on a 2-frame pool, compared with a `HashMap<Rid, Vec<Value>>` and with a full scan every 50 steps. Mutation-checked by hand: returning the old `Rid` from `update` makes it fail.
- `catalog.rs`: `table_heap` returns the same handle on every call, reads and writes the table's own heap, fails with `UndefinedTable` (`42P01`) for an unknown name, opens a table on first use after reopening, and keeps nothing when opening fails (the next call retries); `create_table` keeps the heap it created, and a failed `create_table` keeps none, including when the data heap itself cannot be created.
- Integration in [`tests/table_table_heap.rs`](../tests/table_table_heap.rs): 2,000 rows with every type (`INTEGER`, `VARCHAR`, `BOOLEAN`, `DATE`, `TIMESTAMP`, `DOUBLE PRECISION`, `REAL`, with `NULL`s) across many pages on a 2-frame pool, updates that move rows, deletes, then reopening the file and finding the table by name: scan and `get` match what was left. Rejected rows (`22001`, `23502`, `42804`, `42601`) leave no trace, not even after reopening. A data page damaged on disk is reported as `XX001`, by the scan and, after reopening, when the table is opened.

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

Entries use their **own format** (`ByteWriter` / `ByteReader`): the catalog was designed before text existed, so it could not be a regular table read through `Schema` / `Value`. Now that `VARCHAR` exists, it can become system tables without changing the API (not done yet).

```rust
let mut catalog = Catalog::open(&mut bpm)?;                      // creates the database or loads it
let table = catalog.create_table(&mut bpm, "accounts", schema)?; // TableInfo { id, name, schema, first_page_id }
let table = catalog.table("accounts")?;                          // UndefinedTable if it does not exist
let rows  = catalog.table_heap(&mut bpm, "accounts")?;           // the table's rows (see Table Heap)
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
| `ColumnRecord` | `table_id u32` · `position u16` · `data_type u8` (`1` SMALLINT, `2` INTEGER, `3` BIGINT, `4` VARCHAR, `5` BOOLEAN, `6` REAL, `7` DOUBLE PRECISION, `8` DATE, `9` TIMESTAMP) · [`max_chars u16`, only for VARCHAR] · `flags u8` (bit 0 = nullable) · `name_len u8` · name |

Names have 1 to 63 bytes of UTF-8. Decoding rejects unknown types or flags (`0` is invalid on purpose, so a zeroed entry never decodes), a `VARCHAR` length of `0`, names with an out-of-range length or invalid UTF-8, and missing or trailing bytes. An entry has at most 74 bytes (a `VARCHAR` column with a 63-byte name). Existing codes never change when a type is added (`4` VARCHAR, `5` BOOLEAN, `6` REAL, `7` DOUBLE PRECISION, `8` DATE, then `9` TIMESTAMP), so older databases still open.

### Operations

- **`open`:**
  - page 0 is `Free` → new database: creates both heaps and writes the header **last** (until then, the next `open` starts over);
  - page 0 is `DatabaseHeader` → checks the magic, opens both heaps, decodes every entry and builds each `Schema` in `position` order;
  - page 0 of another type or a wrong magic → `NotADatabase`.
- **Loading and validation:** columns numbered `0..n` without gaps or repetitions, a valid `Schema::new` (`CorruptedSchema`), unique table names and ids (`Corrupted`). Columns of a `table_id` without a `TableRecord` are **orphans** (left over by a failed `create_table`) and are ignored; the next id is `1 + highest id seen in tables and columns`, so an orphan id is never reused.
- **`create_table`**, in this order:
  1. validates the name (empty, longer than 63 bytes, duplicate) and whether a row fits in a page (`schema.min_row_size() <= max_tuple_size_for(page_size)`, otherwise `RowTooWide`);
  2. **consumes** the table id;
  3. creates the table's `TableHeap` (its data heap);
  4. writes one `ColumnRecord` per column;
  5. writes the `TableRecord` **last**: it is what makes the table exist on the next `open`;
  6. only then adds the table to the in-memory catalog, and keeps the `TableHeap` from step 3 open.

  If step 4 or 5 fails, the columns already written are deleted (best effort), the table does not appear and the same name can be created again, but **with another id**: the id consumed in step 2 never comes back. If the undo also fails, the remaining columns become orphans with that id, and no new table inherits them. That is why ids can have gaps, like PostgreSQL sequences.
- **`table(name)` / `tables()`:** in-memory lookups, no I/O. `tables()` is in creation order. Names are compared byte by byte (`users` ≠ `USERS`), like columns.
- **`table_heap(bpm, name)`:** the table's [`TableHeap`](#table-heap). The first call opens it (`HeapFile::open` walks its pages); every later call returns the same handle, without I/O. An unknown name → `UndefinedTable`.

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
| `Heap` / `BufferPool` | the storage error's, as in [Table Heap](#table-heap) (e.g. `XX001` for a checksum mismatch) | message of the wrapped error |
| `Table` | the `TableError`'s | message of the wrapped error (opening or creating a table's heap) |

### Design decisions

See [ADR 0006](decisions/0006-catalog-on-page-zero.md) for the bootstrap and commit-point decisions. Also:

- **The whole catalog in memory**, loaded on `open`: tens of bytes per column, and lookups do no I/O.
- **`TableId(u32)` newtype:** cannot be confused with a page id. The internal counter is a `u64`, so a `u32::MAX` id stored on disk still loads; creating the next table returns `TooManyTables`.
- **Open tables are cached** (`HashMap<TableId, TableHeap>`): one handle per table, so two callers never hold heaps that disagree about which pages exist (see [Table Heap](#table-heap)). A table is opened on first use, not on `open`, so opening the database still reads no data pages. A handle costs a few bytes per page of its heap, only for the tables used. Handles are never dropped while the catalog lives: the future `DROP TABLE` must remove the table from `open_tables` too.

### Known limitations

- ⚠️ **Crash in the middle of `create_table` or of database creation:** the buffer pool writes pages in any order, so after a real crash "`TableRecord` last" is not guaranteed; orphan columns may remain (ignored) or, in the worst case, a `TableRecord` without columns (`CorruptedSchema` on `open`). The complete fix is the WAL.
- ⚠️ **Orphan pages:** the data heap of a failed `create_table` and the heaps of an interrupted database creation have no owner until a future `VACUUM`.
- ⚠️ **Provisional format:** when the catalog becomes system tables, old databases will not open (no migration; test databases are disposable).

### Demo (`cargo run`)

`src/main.rs` opens or creates `data.db` in the current directory, creates the table `accounts (id INTEGER NOT NULL, age SMALLINT, balance BIGINT)` on the first run, inserts one row per run and lists the tables and the rows. Running it several times shows the data surviving. A `data.db` from the old demo (a slotted page in page 0) fails with `NotADatabase` and a message asking to delete the file. A captured run is in the [README](../README.md#quick-start).

### Tests

- `header.rs`: documented bytes, round trip, LSN preserved and reserved bytes zeroed, never-written page → `None`, slotted page or wrong magic → `NotADatabase`, page too small.
- `records.rs`: documented bytes (including a `VARCHAR` length), round trip (names of 1 and 63 bytes, multibyte, each type including `VARCHAR(1)` and `VARCHAR(65535)`, nullable or not), the largest entry at 74 bytes, every corruption reason (truncated at every length, integer and `VARCHAR` entries, trailing bytes, type `0`/`10`, `VARCHAR` length `0`, unknown flag, name of 0/64 bytes, invalid UTF-8), 2,000 random round-trip entries and 2,000 corrupted ones without panicking.
- `catalog.rs`: database creation and empty reopen, page 0 that is not a header, `create_table` (lookup, order and ids, invalid or duplicate name, `id` vs. `ID`, `RowTooWide` at the exact limit, the open `TableHeap` kept, `TooManyTables`, including with a `u32::MAX` id loaded from disk), loading (every table back, ids continue, 1,000 columns across several pages with eviction, orphans ignored without reusing ids, columns with gaps or repetitions, invalid schema, table without columns, duplicate name or id, corrupted entry), injected failures (in the middle of `create_table`, on the table entry and between columns: no visible table, not even after reopening; **undo that also fails**: the id is not reused and the database still opens; failed header write: the next `open` creates the database from scratch; no frame for the data heap: no table, the id consumed), `table_heap` (same handle on every call, `UndefinedTable`, opened on first use after reopening) and every error with message, SQLSTATE and `source()`.
- `random_creations_should_match_model_across_reopens`: 300 random creations (duplicate and invalid names, 1 to 30 columns of every type, including `VARCHAR(1)` and `VARCHAR(65535)`) with a 3-frame pool, reopening every 50 and comparing with a `Vec`.
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
| Character string types | `CHARACTER(n)`, `CHARACTER VARYING(n)`, `CHARACTER LARGE OBJECT` | only `VARCHAR(n)`, `1 ≤ n ≤ 65535`; no `CHAR(n)`, `CLOB` or PostgreSQL's `TEXT` |
| Text comparison | depends on the collation; `PAD SPACE` collations make `'a' = 'a '` | `NO PAD`, by UTF-8 bytes (code point order), like PostgreSQL's `"C"` collation; no `COLLATE` |
| `CAST` of text to a shorter `VARCHAR(n)` | cuts and raises a *string data, right truncation* warning | cuts silently (no warnings exist yet) |
| `CAST` of an integer to a `VARCHAR(n)` too short for its digits | error `22001` | cuts the decimal text (`CAST(-12345 AS VARCHAR(3))` = `'-12'`), as PostgreSQL does; kept on purpose, decided with the author |
| Integer assigned to a text column | not allowed in store assignment | rejected (`42804`); PostgreSQL accepts it through an assignment cast. Only an explicit `CAST` converts |
| Comparing text with a number | not allowed | `42883`; the error names `=` whatever the comparison operator |
| Empty string | different from `NULL` | same (Oracle treats `''` as `NULL`; ferroDB does not) |
| Text to `BOOLEAN` | trim spaces, then the literals `TRUE`, `FALSE` (any case) or `UNKNOWN` (→ `NULL`); otherwise `22018` | `true` / `false` in any case **and `1` / `0`** (decided with the author); `'unknown'` is rejected (`22P02`), since text input never yields `NULL` |
| `CAST` between integers and `BOOLEAN` | not defined | `0` ⇄ FALSE and `1` ⇄ TRUE in every width (decided with the author); other integers → `22003`. PostgreSQL takes any non-zero `INTEGER` as TRUE |
| `NaN` and `Infinity` | not in SQL | not accepted (PostgreSQL accepts `'NaN'`, `'Infinity'`, `'-Infinity'`); overflow is an error |
| Negative zero | not in SQL | folded into `0` (PostgreSQL keeps and prints `-0`) |
| Float underflow | implementation-defined | `22003` when a non-zero result becomes zero, as PostgreSQL |
| Approximate to exact in assignment | allowed, with the `CAST` rules | rejected (`42804`); only an explicit `CAST` rounds (decided with the author; PostgreSQL accepts, rounding) |
| Exact vs. approximate comparison | by algebraic value | exact, as the standard (PostgreSQL converts the integer to `float8`) |
| `MOD` with approximate numbers | not defined | `%` with a float → `42883` |
| `CAST` of a float to a short `VARCHAR(n)` | `22001` when the text does not fit | cut to `n`, like integers |
| Arithmetic on dates | through an interval type (`DATE + INTERVAL '30' DAY`) | `DATE ± integer` and `DATE - DATE` in days, as PostgreSQL (decided with the author); no interval type |
| Date text | the literal `DATE 'YYYY-MM-DD'` | only `YYYY-MM-DD`, same as the standard (PostgreSQL also takes other styles set by `DateStyle`) |
| Timestamp text | the literal `TIMESTAMP 'YYYY-MM-DD HH:MM:SS[.f]'` | that form, plus `T` instead of the space and a bare date as midnight (decided with the author; PostgreSQL accepts both) |
| Seconds of a `TIMESTAMP` | `00` to `61.9…`, for leap seconds | `00` to `59.999999` (PostgreSQL reads `:60` as the next minute) |
| `TIMESTAMP` into a `DATE` column | allowed, dropping the time | only through an explicit `CAST` (decided with the author, as float to integer) |
| Arithmetic on timestamps | through an interval type | none |
| `CAST` of a `DATE` to a short `VARCHAR(n)` | `22001` when the text does not fit | cut to `n`, like the other types |
| `CAST` of a `BOOLEAN` to a short `VARCHAR(n)` | `22001` when `'TRUE'`/`'FALSE'` does not fit | cut to `n`, like integers (`CAST(TRUE AS VARCHAR(1))` = `'T'`) |
