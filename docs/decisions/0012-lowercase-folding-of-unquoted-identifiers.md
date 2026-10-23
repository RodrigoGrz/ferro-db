# 0012. Unquoted identifiers fold to lowercase

- Status: Accepted
- Date: 2026-10-22

## Context

The lexer has to decide what an unquoted identifier stands for. ISO/IEC 9075 (5.2) says a regular identifier is equivalent to the delimited identifier written in **uppercase**: `users` and `Users` both mean `"USERS"`. PostgreSQL, the project's reference where the standard is silent or impractical, folds to **lowercase** instead: they mean `"users"`. The choice is not internal: it decides the names stored in the catalog from now on, and whether `"users"` (quoted, lowercase) names the same table as an unquoted `users`.

## Decision

- **Unquoted identifiers fold to lowercase** (decided with the author): `users`, `Users` and `USERS` are all `"users"`; a quoted `"Users"` is another name.
- **Only `A`–`Z` change**, as in PostgreSQL; other letters keep their case (`AÇÃO` → `aÇÃo`).
- **Keywords are matched on the folded text**, so `SELECT`, `Select` and `select` are the same keyword.
- The deviation is recorded in [data-model.md](../data-model.md#deviations-from-the-sql-standard).

## Alternatives considered

- **Uppercase, as the standard says:** faithful, but the catalog would hold names like `USERS`, every quoted lowercase name would differ from its unquoted spelling, and results would differ from PostgreSQL's, against which behavior is checked.
- **Case-insensitive names everywhere (keeping the spelling):** avoids folding, but needs case-insensitive comparison in the catalog for unquoted names only, which is neither the standard nor PostgreSQL.
- **Full Unicode folding (`str::to_lowercase`):** folds `Ç` too, but can change a name's length (`İ` becomes two characters) and differs from PostgreSQL.

## Consequences

- Names in the catalog are lowercase unless they were quoted; the catalog keeps comparing exact bytes.
- Statements behave as in PostgreSQL: `CREATE TABLE Users` then `SELECT * FROM users` works; `SELECT * FROM "Users"` does not.
- Porting a query written for a database that follows the standard can differ only for quoted uppercase names.

## Implementation and tests

- [`src/sql/lexer.rs`](../../src/sql/lexer.rs) (`Lexer::word`)
- Unit tests in that module (`words_should_be_folded_to_lowercase_and_marked_as_keywords`, `only_ascii_letters_should_be_folded`, the random round-trip test with keywords in random case) and [`tests/sql_lexer.rs`](../../tests/sql_lexer.rs)
- Details in [sql.md](../sql.md)
