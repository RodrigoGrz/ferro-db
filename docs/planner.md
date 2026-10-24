# Planner

This document describes ferroDB's planner layer (`src/planner/`): how a parsed statement becomes work. It runs DDL against the catalog and DML through the executors, after the **binder** has turned names into column positions and literals into typed values. `SELECT` is not run yet: the query planner, which will choose scans, joins and aggregation, comes next. The parser is in [sql.md](sql.md); the executors in [execution.md](execution.md).

Dependencies point downward: `planner` → `sql`, `execution`, `catalog`, `table`, `types`.

- [Running SQL](#running-sql)
- [Binder](#binder)
- [Statements](#statements)
- [Errors](#errors)
- [Design decisions](#design-decisions)
- [Tests](#tests)
- [Limitations](#limitations)

---

## Running SQL

```rust
pub fn run_sql(sql: &str, ctx: &mut ExecutionContext<'_>) -> Result<Vec<StatementOutcome>, PlannerError>;

pub enum StatementOutcome { CreateTable, CreateIndex, Insert(u64), Update(u64), Delete(u64) }

let outcomes = run_sql(
    "CREATE TABLE accounts (id INTEGER NOT NULL, owner VARCHAR(40), balance BIGINT);
     CREATE UNIQUE INDEX accounts_id ON accounts (id);
     INSERT INTO accounts VALUES (1, 'ana', 100), (2, NULL, 0);
     UPDATE accounts SET balance = balance + 1 WHERE id BETWEEN 1 AND 2;
     DELETE FROM accounts WHERE owner IS NULL",
    &mut ExecutionContext { catalog: &mut catalog, bpm: &mut bpm },
)?;
// [CreateTable, CreateIndex, Insert(2), Update(2), Delete(1)]
```

- **The whole text is parsed first:** a syntax error in any statement runs none of them.
- **Then each statement runs in order**, and the first failure stops the rest. There are no transactions: the statements before the failure stay done. Each statement on its own is atomic: the write executors validate the whole statement before writing, and the catalog checks everything before creating.
- **`StatementOutcome`** is what each statement did, like PostgreSQL's command tags (`INSERT 0 2`).

## Binder

`binder.rs` turns an expression of the AST (names, literal text) into an expression the executors evaluate (column positions, typed values). Its `Scope` is the statement's table, or none for the rows of `VALUES`.

| AST | Executor |
|---|---|
| a column `name` or `table.name` | `Column(position)`; a qualifier other than the statement's table is `UndefinedTableReference` (`42P01`, *missing FROM-clause entry*); an unknown name, or any column in `VALUES`, is `UndefinedColumn` (`42703`) |
| an integer | `INTEGER` if it fits, else `BIGINT`; past `BIGINT`, `22003` |
| a number with `.` or an exponent | `DOUBLE PRECISION` (there is no `NUMERIC`) |
| `-` before a number | the negative literal, typed after the sign, so `-2147483648` is an `INTEGER` and `-9223372036854775808` a valid `BIGINT` |
| a string, `TRUE`, `FALSE`, `NULL` | `VARCHAR` text, `BOOLEAN`, `NULL` |
| `DATE '…'`, `TIMESTAMP '…'` | `Value::parse` (`22007`, `22008`) |
| `NOT`, `-`, `+` | `Not`, `Negate`, the operand itself |
| binary operators, `IS …`, `IS DISTINCT FROM`, `CAST` | the executor's nodes of the same meaning (`%` is `Rem`) |
| `x BETWEEN a AND b` | `x >= a AND x <= b`, the standard's definition (8.3); `NOT BETWEEN` is `NOT (…)` |
| `x IN (a, b, …)` | `x = a OR x = b OR …`, the standard's definition (8.4), with the same three-valued result; `NOT IN` is `NOT (…)`. The `OR`s form a balanced tree, as deep as the logarithm of the list: a chain would be as deep as the list, and evaluation recurses once per level, so `IN` with 100,000 values would exhaust the stack |
| `count`, `sum`, `avg`, `min`, `max` | `AggregateNotAllowed` (`42803`): aggregates are not allowed in `WHERE`, `VALUES` or `SET` |
| any other function | `UndefinedFunction` (`42883`): there are no scalar functions yet |

Expressions are not type-checked statically: a `WHERE` that is not boolean fails when it runs (`42804`, from the `Filter`), and a value of the wrong type for its column fails on assignment.

A string is `VARCHAR` and stays so: it is not turned into a date, a timestamp or a number by the column it is assigned to or the value it is compared with. `INSERT INTO t (day) VALUES ('2024-01-31')` into a `DATE` column is `42804`, and `day = '2024-01-31'` is `42883`; write `DATE '2024-01-31'` or `CAST('2024-01-31' AS DATE)`. This is the standard's store assignment, which takes a datetime only from a datetime.

## Statements

| Statement | Runs as |
|---|---|
| `CREATE TABLE name (column type [NOT NULL \| NULL], …)` | `Schema::new` then `Catalog::create_table` (`42701` repeated column, `42P07` existing table, `42622` long name…) |
| `CREATE UNIQUE INDEX name ON table (column)` | `Catalog::create_index`, which indexes the rows already there (`23505` if the column has repeated values, `42P01`, `42703`, `42P07`) |
| `CREATE INDEX` without `UNIQUE` | `Unsupported` (`0A000`): every index is unique, and a plain index must accept repeated keys |
| `INSERT INTO table [(columns)] VALUES (…), …` | `Insert(Values)`: each row in table order, the columns outside the list `NULL` (the default of a column without `DEFAULT`); a repeated column is `42701`, a row of another length `42601`; the values are assigned to the column types by the table (`42804`, `22001`, `22003`, `23502`) and the unique indexes checked (`23505`) |
| `UPDATE table SET column = expr, … [WHERE expr]` | `Update(Filter(SeqScan))`; each assignment reads the row as it was before the statement; a column set twice is `42601` |
| `DELETE FROM table [WHERE expr]` | `Delete(Filter(SeqScan))` |
| `SELECT` | `Unsupported` (`0A000`) until the query planner |

The plans of DML are fixed: choosing an `IndexScan` for a `WHERE` is optimization, still to come.

## Errors

`PlannerError` (`error.rs`) keeps the errors of the layers below with their SQLSTATEs (`Sql`, `Catalog`, `Schema`, `Type`, `Execution`) and adds the binder's:

| Variant | SQLSTATE | Message |
|---|---|---|
| `UndefinedColumn { column }` | `42703` | `column "x" does not exist` |
| `UndefinedTableReference { table }` | `42P01` | `missing FROM-clause entry for table "u"` |
| `DuplicateColumn { column }` | `42701` | `column "id" specified more than once` |
| `DuplicateAssignment { column }` | `42601` | `multiple assignments to same column "v"` |
| `ColumnCountMismatch { expected, found }` | `42601` | `INSERT has more expressions (3) than target columns (2)`, or fewer |
| `AggregateNotAllowed { function }` | `42803` | `aggregate function "count" is not allowed here` |
| `UndefinedFunction { name }` | `42883` | `function "f" does not exist` |
| `Unsupported { feature }` | `0A000` | `SELECT is not supported` |
| `MissingCount` | `XX000` | `a write statement returned no row count`: a write executor that did not give one non-negative `BIGINT`, a plan built wrong |

## Design decisions

- **A binder apart from the statements**, instead of translating inside each one: the query planner will reuse it.
- **`BETWEEN` and `IN` rewritten by the binder**, instead of new executor nodes: they are the standard's definitions, and evaluating `x` twice has no side effect (there are no volatile functions).
- **Literal types chosen by the binder**, not the parser: the parser keeps the text, and the sign joins the number where its type is decided.
- **`DOUBLE PRECISION` for decimals** until there is `NUMERIC`: it is the only type that holds `1.5`.
- **`IN` as a balanced tree of `OR`s**, not a chain: the depth of an expression is bounded by the parser's nesting limit, but a list of values is read in a loop and could be as long as the text.
- **`PRIMARY KEY` and `UNIQUE` columns later** (decided with the author), with the Constraints section: index names, the implicit `NOT NULL` of a primary key.
- **Only `CREATE UNIQUE INDEX`** (decided with the author): a `CREATE INDEX` that refused repeated keys would have the wrong meaning.
- **Fixed plans for DML**, no `IndexScan`: choosing an index is Index Selection.
- **The whole text parsed before running**, statements run one by one: a syntax error leaves nothing half done; without transactions, a failure while running keeps the statements before it.
- **Outcomes as command tags**, instead of rows: a future CLI prints them as they are.

## Tests

- `error.rs`: every error with its message, SQLSTATE and source.
- `binder.rs`: columns as positions (qualified, any case), an unknown column, a column of another table and a column in `VALUES`; integers at the limits of `INTEGER` and `BIGINT` with and without a sign, past `BIGINT` (`22003`); decimals and exponents as `DOUBLE PRECISION`, `1e400` (`22003`); strings, booleans, `NULL`, dates and timestamps, invalid dates (`22007`, `22008`); operators, `NOT`, signs, `CAST`; `IS` tests with `NULL`; `BETWEEN` at, inside and outside its bounds, reversed and with `NULL`, `NOT BETWEEN`; `IN` with and without `NULL` in the list or the value, `NOT IN`, a list of 1,000 values as a tree 12 nodes deep, the first failing value's error; aggregates (`42803`) and unknown functions (`42883`).
- `run.rs`: `CREATE TABLE` (the schema in the catalog), an existing table and a repeated column with nothing created; `CREATE UNIQUE INDEX` over rows already there, a column with repeated values (`23505`), unknown table and column, an existing name, `CREATE INDEX` (`0A000`); a syntax error running nothing; `SELECT` (`0A000`); `INSERT` with and without a column list; every failing `INSERT` (`23502`, `42804`, `22001`, `23505`, `42601` both ways, `42701`, `42703`, `42P01`, a column in `VALUES`, `42803`, `22012`) leaving the table and the index as they were; `UPDATE` of some rows, all rows and the indexed key; every failing `UPDATE` (`42601`, `42703`, `23502`, `23505`, `42P01`, a non-boolean `WHERE`, an unknown table) changing nothing; `DELETE` with a `WHERE` and without; a `DELETE … IN` of 100,000 values on a 2 MB stack; every column type taking its literals (`BIGINT` at its maximum, `SMALLINT` at its minimum, an `INTEGER` literal widened to `SMALLINT`, `BIGINT` and `DOUBLE PRECISION`, `BOOLEAN`, `DATE`, `TIMESTAMP`) and refusing others (`22003`, `22001`, `42804` for a decimal in `BIGINT` and a string in `DATE`); a write executor without its count (`XX000`); several statements running in order and stopping at the first failure.
- `random_statements_should_keep_the_table_like_a_model`: 400 random statements on `(id INTEGER NOT NULL, v INTEGER, s VARCHAR(10))` with a unique index on `id`: multi-row inserts (some with a repeated or `NULL` id), `UPDATE … SET v = v + k WHERE id BETWEEN a AND b`, `UPDATE … SET id = id + k` over a range, `DELETE … WHERE id IN (…)` or `v IS NULL`; a `BTreeMap` model predicts each outcome or SQLSTATE (`23502`, `23505`), and after every statement the table and the index must match it.
- Mutations checked by hand, each caught: `BETWEEN` with `>` instead of `>=` (by the random test, which showed the unit test lacked the bounds themselves; they were added); `IN` as a chain of `OR`s again (by the depth test).
- Integration in [`tests/planner_statements.rs`](../tests/planner_statements.rs), with 8 KB pages and an 8-frame pool: SQL creates a table and a unique index, inserts 3,000 rows in 30 statements, updates half of them and deletes a range; after reopening, a `SeqScan` and the index give the expected rows; keys moved by `UPDATE … SET id = id + 10000` survive reopening in the index; a failure in the middle of a script keeps the statements before it.

## Limitations

- **`SELECT` does not run** yet, and there is no `INSERT … SELECT`.
- **No `PRIMARY KEY`, `UNIQUE` columns, `DEFAULT`, `CHECK` or foreign keys**; no `DROP TABLE` or `ALTER TABLE`.
- **Indexes are unique and of one column**, created only with `CREATE UNIQUE INDEX`.
- **No transactions:** a script that fails midway keeps the statements before the failure.
- **No static types:** type errors in expressions show up when they run, and a string is never converted to the type of its column.
- **DML always scans the table**, even when an index could find the rows.
