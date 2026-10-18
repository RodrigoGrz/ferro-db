# 0009. Floats are finite, compare exactly with integers and share one numeric family

- Status: Accepted
- Date: 2026-10-18

## Context

`REAL` and `DOUBLE PRECISION` bring IEEE 754 into a type system that so far had only exact values. IEEE has NaN, ±infinity and `-0.0`, which SQL does not; PostgreSQL accepts the first three as an extension with special rules (`NaN = NaN`, NaN greater than every number). Floats also raise two questions every future numeric type (`NUMERIC`) will share: how exact and approximate numbers mix, and how they compare.

## Decision

- **Only finite values.** `Value::Real(Finite32)` and `Value::Double(Finite64)` wrap newtypes that refuse NaN and infinities and fold `-0.0` into `0.0`. Overflow and underflow are errors (`22003`); reading NaN, infinity or `-0.0` from disk is corruption.
- **One numeric family.** `TypeFamily::Integer` became `Numeric`: integers and floats mix in arithmetic (integer op float → `DOUBLE PRECISION`) and in comparison.
- **Asymmetric assignment.** An integer goes into a float column; a float goes into an integer column only through an explicit `CAST` (half to even). Decided with the author.
- **Exact integer-float comparison**, by algebraic value, without converting the integer to `f64`.
- **No `CAST` between floats and booleans** (`CannotCast`, `42846`). Decided with the author.

## Alternatives considered

- **PostgreSQL's NaN and Infinity:** every comparison, sort, cast and text path would need special cases, and NaN would break `Eq` on `Value`. The standard does not have them.
- **Plain `f32`/`f64` in `Value`:** `Value::Double(f64::NAN)` could be built anywhere and break the order later; `Value` would lose `Eq`.
- **Keeping `-0.0`** (PostgreSQL): `0 = -0` with different bytes, a problem for byte comparison and future indexes.
- **Floats as their own family:** `1 + 1.5` would need a `CAST`.
- **Converting integers to `f64` to compare** (PostgreSQL): simpler, but different integers above 2⁵³ compare equal to the same float.
- **Accepting a float into an integer column by rounding** (standard and PostgreSQL): convenient, but a fraction disappears silently in an `INSERT`.

## Consequences

- The order of floats is total without special cases, and `Value` stays `Eq`.
- `NUMERIC` will join `TypeFamily::Numeric` and needs its own place in the exact comparison.
- There is now one pair of types without a `CAST` (float ⇄ boolean).
- Float output follows PostgreSQL's layout (shortest digits, `%g` exponent rule), so values read the same in both.

## Implementation and tests

- [`src/types/float.rs`](../../src/types/float.rs), [`src/types/value.rs`](../../src/types/value.rs), [`src/types/data_type.rs`](../../src/types/data_type.rs), [`src/types/error.rs`](../../src/types/error.rs), [`src/catalog/records.rs`](../../src/catalog/records.rs) (codes `6` and `7`)
- Unit and randomized tests in those modules; integration in [`tests/types_float.rs`](../../tests/types_float.rs)
- Details in [data-model.md](../data-model.md#data-types-real-and-double-precision)
