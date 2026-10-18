# 0008. Text is VARCHAR(n) in UTF-8, compared by bytes, with store assignment separate from CAST

- Status: Accepted
- Date: 2026-10-18

## Context

ferroDB needed a text type. The SQL standard defines `CHARACTER(n)`, `CHARACTER VARYING(n)` and `CHARACTER LARGE OBJECT`, PostgreSQL adds `TEXT`, and every option brings its own rules for padding, comparison and conversion. Text is also the first type that is not an integer, so mixing types (`'1' + 1`, `'a' < 1`, an integer in a text column) had to stop being impossible by construction and become an error.

The tuple format already supported variable-width fields (`ColumnLayout::Variable`), so no change below `types` was needed.

## Decision

- **Only `VARCHAR(n)`**, `DataType::Varchar(NonZeroU16)`: `1 ≤ n ≤ 65535`, so `VARCHAR(0)` cannot be built. `n` counts characters (Unicode scalar values), not bytes.
- **UTF-8 on disk**, the field holding the raw bytes. Reading validates UTF-8 and the length again (`CorruptedValue`, named `CorruptedText` until BOOLEAN arrived).
- **`Value::Text(String)` does not carry `n`.** The limit belongs to the column. Text that did not come from a column (the result of `||` or of a `CAST` from an integer) has the type `VARCHAR(65535)` (`DataType::VARCHAR_MAX`).
- **Comparison by UTF-8 bytes, without padding** (`'Z' < 'a'`, `'a' < 'a '`), which equals code point order and PostgreSQL's `"C"` collation.
- **Store assignment separate from `CAST`**, as the standard defines them: `Value::cast` converts integer ⇄ text and cuts text to `n`; `Value::assign`, used by `Schema::encode_row`, stays within a family (`42804` otherwise) and raises `22001` for text longer than `n`, unless the excess is only spaces, which are cut.
- **Mixing families is an error:** arithmetic or `||` across families and comparing text with integers → `42883`. `compare` and `is_distinct_from` return `Result`; `sort_cmp` stays total (integers before text, `NULL` last).
- **Catalog code `4`** for `VARCHAR`, followed by `n` as a `u16`, leaving codes `1` to `3` untouched.

## Alternatives considered

- **`CHAR(n)`:** padding on write, ignoring trailing spaces on comparison and its own cast rules, for a type little used today. Left out; it can be added later.
- **`TEXT` (no limit):** a PostgreSQL extension that only removes the limit; a large `VARCHAR(n)` covers the same use. Decided with the author not to have it.
- **`Value::Varchar(String, u16)`:** duplicates what the schema already says and complicates every comparison.
- **`Option` inside `Varchar` for "unlimited":** would bring `TEXT` back under another name.
- **`Text(Arc<str>)`** to keep cloning cheap: premature; `Value` simply stops being `Copy`.
- **Collation by language** (dictionary order, case-insensitive comparison): needs Unicode tables or a new dependency. Postponed as its own item.
- **Accepting an integer in a text column** (PostgreSQL's assignment cast): rejected with the author ("if it is not text, it does not go in"); only an explicit `CAST` converts. The future Type System may make it configurable.
- **A type modifier on every catalog column:** would change every existing entry; extra bytes only for `VARCHAR` keep old databases readable.

## Consequences

- Byte order surprises users (`'é'` after `'z'`, uppercase before lowercase); it is documented as a deviation and pinned by tests.
- The limit is in characters while pages are in bytes: a valid `VARCHAR(8000)` value can still fail with `TupleTooLarge`. Values larger than a page need an overflow mechanism (not planned yet).
- Length checks are O(length) on every assignment and read.
- `CAST` cuts silently: ferroDB has no warnings (completion conditions) yet. It also cuts the decimal text of an integer cast to a short `VARCHAR(n)` (`'-12'` for `-12345`), where the standard raises `22001`; this follows PostgreSQL and is listed as a deviation.
- A row of valid text can exceed the tuple format's 65,535 bytes; `Schema::encode_row` reports it as `RowTooLarge` (`54000`), not as corruption.
- A comparison across families reports `=` even when the query used `<`; the executor, which knows the operator, can rename it.

## Implementation and tests

- [`src/types/data_type.rs`](../../src/types/data_type.rs), [`src/types/value.rs`](../../src/types/value.rs), [`src/types/text.rs`](../../src/types/text.rs), [`src/types/error.rs`](../../src/types/error.rs)
- [`src/table/schema.rs`](../../src/table/schema.rs) (`encode_row` uses `Value::assign`), [`src/catalog/records.rs`](../../src/catalog/records.rs) (code `4`)
- Unit and randomized tests in those modules; integration in [`tests/types_string.rs`](../../tests/types_string.rs)
- Details in [data-model.md](../data-model.md#data-types-varchar)
