# Planner

This document describes ferroDB's planner layer (`src/planner/`): how a parsed statement becomes work. It runs DDL against the catalog and DML through the executors, after the **binder** has turned names into column positions and literals into typed values. A `SELECT` becomes a validated **logical plan**, which `EXPLAIN` shows; running it waits for the physical plan, which will choose the executors. The parser is in [sql.md](sql.md); the executors in [execution.md](execution.md).

Dependencies point downward: `planner` → `sql`, `execution`, `catalog`, `table`, `types`.

| File | Responsibility |
|---|---|
| `run.rs` | `run_sql`: parse the text, then run each statement |
| `scope.rs` | the names an expression may read: the tables of the `FROM`, each at an offset of the row |
| `binder.rs` | AST expressions as executor expressions; the grouped mode above the groups of a query |
| `select.rs` | `plan_select`: a `SELECT` as a `LogicalPlan`, clause by clause |
| `logical_plan.rs` | `LogicalPlan`, `OutputColumn`, `ColumnName` |
| `explain.rs` | the text of `EXPLAIN` |
| `error.rs` | `PlannerError` |

- [Running SQL](#running-sql)
- [Scope](#scope)
- [Binder](#binder)
- [Statements](#statements)
- [Logical plan](#logical-plan)
- [Planning a SELECT](#planning-a-select)
- [EXPLAIN](#explain)
- [Errors](#errors)
- [Design decisions](#design-decisions)
- [Tests](#tests)
- [Limitations](#limitations)

---

## Running SQL

```rust
pub fn run_sql(sql: &str, ctx: &mut ExecutionContext<'_>) -> Result<Vec<StatementOutcome>, PlannerError>;

pub enum StatementOutcome { CreateTable, CreateIndex, Insert(u64), Update(u64), Delete(u64), Explain(String) }

let outcomes = run_sql(
    "CREATE TABLE accounts (id INTEGER NOT NULL, owner VARCHAR(40), balance BIGINT);
     CREATE UNIQUE INDEX accounts_id ON accounts (id);
     INSERT INTO accounts VALUES (1, 'ana', 100), (2, NULL, 0);
     UPDATE accounts SET balance = balance + 1 WHERE id BETWEEN 1 AND 2;
     DELETE FROM accounts WHERE owner IS NULL;
     EXPLAIN SELECT owner FROM accounts WHERE balance > 10",
    &mut ExecutionContext { catalog: &mut catalog, bpm: &mut bpm },
)?;
// [CreateTable, CreateIndex, Insert(2), Update(2), Delete(1), Explain("Project: …")]
```

- **The whole text is parsed first:** a syntax error in any statement runs none of them.
- **Then each statement runs in order**, and the first failure stops the rest. There are no transactions: the statements before the failure stay done. Each statement on its own is atomic: the write executors validate the whole statement before writing, and the catalog checks everything before creating.
- **`StatementOutcome`** is what each statement did, like PostgreSQL's command tags (`INSERT 0 2`); `Explain` carries the text of the plan.

## Scope

`scope.rs` holds the tables an expression may read. Each is visible under one name, its alias or else its own, with its columns at an offset of the row: a join puts the right table's columns after the left's, as the join executors do.

| Operation | Result |
|---|---|
| `Scope::table(name, schema)`, `Scope::empty()` | one table (DML, a `FROM` item), or none (`VALUES`, `LIMIT`, `SELECT` without `FROM`) |
| `join(left, right)` | the tables of both, the right ones shifted by the left's width; a name already there is `DuplicateAlias` (`42712`) |
| `column(Some(t), c)` | `c` in table `t` only; an unknown `t` is `UndefinedTableReference` (`42P01`), an unknown `c` `UndefinedColumn` (`42703`) |
| `column(None, c)` | `c` in every table: in one, its position; in several, `AmbiguousColumn` (`42702`); in none, `42703` |
| `wildcard(None)`, `wildcard(Some(t))` | the positions and names `*` or `t.*` stand for, in the order of the tables; `*` without tables is `WildcardWithoutTables` (`42601`) |
| `qualified_name(position)` | `t.c`, the way errors name a column |

The alias hides the table's own name (ISO/IEC 9075, 7.6): in `FROM users u`, `users.id` is `42P01`.

## Binder

`binder.rs` turns an expression of the AST (names, literal text) into an expression the executors evaluate (column positions, typed values), over a `Scope`. `bind(expr, scope, clause)` names the clause it binds for, which the error of an aggregate there names too.

| AST | Executor |
|---|---|
| a column `name` or `table.name` | `Column(position)` from the scope (`42P01`, `42702`, `42703`) |
| an integer | `INTEGER` if it fits, else `BIGINT`; past `BIGINT`, `22003` |
| a number with `.` or an exponent | `DOUBLE PRECISION` (there is no `NUMERIC`) |
| `-` before a number | the negative literal, typed after the sign, so `-2147483648` is an `INTEGER` and `-9223372036854775808` a valid `BIGINT` |
| a string, `TRUE`, `FALSE`, `NULL` | `VARCHAR` text, `BOOLEAN`, `NULL` |
| `DATE '…'`, `TIMESTAMP '…'` | `Value::parse` (`22007`, `22008`) |
| `NOT`, `-`, `+` | `Not`, `Negate`, the operand itself |
| binary operators, `IS …`, `IS DISTINCT FROM`, `CAST` | the executor's nodes of the same meaning (`%` is `Rem`) |
| `x BETWEEN a AND b` | `x >= a AND x <= b`, the standard's definition (8.3); `NOT BETWEEN` is `NOT (…)` |
| `x IN (a, b, …)` | `x = a OR x = b OR …`, the standard's definition (8.4), with the same three-valued result; `NOT IN` is `NOT (…)`. The `OR`s form a balanced tree, as deep as the logarithm of the list: a chain would be as deep as the list, and evaluation recurses once per level, so `IN` with 100,000 values would exhaust the stack |
| `count`, `sum`, `avg`, `min`, `max` | `AggregateNotAllowed` (`42803`, *aggregate functions are not allowed in WHERE*) outside the grouped mode |
| any other function | `UndefinedFunction` (`42883`): there are no scalar functions yet |

Expressions are not type-checked statically: a `WHERE` that is not boolean fails when it runs (`42804`, from the `Filter`), and a value of the wrong type for its column fails on assignment.

A string is `VARCHAR` and stays so: it is not turned into a date, a timestamp or a number by the column it is assigned to or the value it is compared with. `INSERT INTO t (day) VALUES ('2024-01-31')` into a `DATE` column is `42804`, and `day = '2024-01-31'` is `42883`; write `DATE '2024-01-31'` or `CAST('2024-01-31' AS DATE)`. This is the standard's store assignment, which takes a datetime only from a datetime.

### Grouped mode

Above the groups of a grouped query, `bind_grouped(expr, scope, &mut grouping)` binds over the **grouped row**: the values of the `GROUP BY` expressions, then one per aggregate, the row of the `Aggregation` executor. `Grouping` holds the bound `GROUP BY` and the aggregates found so far. Going down the AST, at each part:

1. **An aggregate** (`count(*)`, or `count`, `sum`, `avg`, `min`, `max` of one argument) binds its argument over the input and becomes `Column(groups + j)`; an aggregate already found is reused, so `count(*)` in the list and in `HAVING` is computed once. An aggregate in the argument is `NestedAggregate` (`42803`); another number of arguments (`sum(*)`, `count(a, b)`, `max()`) is `42883`.
2. **A part without an aggregate equal to a `GROUP BY` expression** becomes `Column(i)`. The comparison is between bound expressions, so `a`, `t.a` and `A` are the same group, and `GROUP BY a + 1` serves `(a + 1) * 2`.
3. **Any other column** is `GroupingError` (`42803`, *column "u.email" must appear in the GROUP BY clause or be used in an aggregate function*).
4. **Anything else** is made of its parts, each bound the same way; a constant stays as it is.

## Statements

| Statement | Runs as |
|---|---|
| `CREATE TABLE name (column type [NOT NULL \| NULL], …)` | `Schema::new` then `Catalog::create_table` (`42701` repeated column, `42P07` existing table, `42622` long name…) |
| `CREATE UNIQUE INDEX name ON table (column)` | `Catalog::create_index`, which indexes the rows already there (`23505` if the column has repeated values, `42P01`, `42703`, `42P07`) |
| `CREATE INDEX` without `UNIQUE` | `Unsupported` (`0A000`): every index is unique, and a plain index must accept repeated keys |
| `INSERT INTO table [(columns)] VALUES (…), …` | `Insert(Values)`: each row in table order, the columns outside the list `NULL` (the default of a column without `DEFAULT`); a repeated column is `42701`, a row of another length `42601`; the values are assigned to the column types by the table (`42804`, `22001`, `22003`, `23502`) and the unique indexes checked (`23505`) |
| `UPDATE table SET column = expr, … [WHERE expr]` | `Update(Filter(SeqScan))`; each assignment reads the row as it was before the statement; a column set twice is `42601` |
| `DELETE FROM table [WHERE expr]` | `Delete(Filter(SeqScan))` |
| `EXPLAIN SELECT …` | `plan_select`, then the text of the plan |
| `SELECT` | `Unsupported` (`0A000`) until the physical plan |

The plans of DML are fixed: choosing an `IndexScan` for a `WHERE` is optimization, still to come.

## Logical plan

`logical_plan.rs` says *what* a `SELECT` computes, as a tree of relational operators. Each node's expressions are the executors' (`execution::Expr`, `Aggregate`, `SortKey`) and read the rows of its input by position, as the executors will.

```rust
pub enum LogicalPlan {
    Scan { table: String, alias: Option<String>, columns: Vec<String> },
    OneRow,                                                    // SELECT without FROM
    Filter { input: Box<LogicalPlan>, predicate: Expr },
    Join { left: Box<LogicalPlan>, right: Box<LogicalPlan>, join_type: JoinType, on: Option<Expr> },
    Aggregate { input: Box<LogicalPlan>, group_by: Vec<Expr>, aggregates: Vec<Aggregate> },
    Sort { input: Box<LogicalPlan>, keys: Vec<SortKey> },
    Limit { input: Box<LogicalPlan>, count: Option<Expr>, offset: Option<Expr> },
    Project { input: Box<LogicalPlan>, columns: Vec<OutputColumn> },
}

pub struct OutputColumn { pub expr: Expr, pub name: String }
pub struct ColumnName { pub table: Option<String>, pub name: String }
```

| Node | Its row (`columns()`) |
|---|---|
| `Scan` | the table's columns, qualified by the alias or the table |
| `OneRow` | no columns |
| `Filter`, `Sort`, `Limit` | the input's |
| `Join` | the left's, then the right's; `CROSS JOIN` is `Inner` without `on`, the `ON` reads the joined row |
| `Aggregate` | each group (a column keeps its name; an expression is named by its text, in parentheses if compound), then each aggregate by its call (`count(*)`, `sum(o.total)`) |
| `Project` | the output names |

The counts of a `Limit` read no columns: they are computed once, before the first row. `plan_select` only reads the catalog.

## Planning a SELECT

`plan_select(select, catalog)` (`select.rs`) plans the clauses in the standard's order of evaluation, each over the rows of the one before, and assembles

```
Scan / Join → Filter (WHERE) → Aggregate → Filter (HAVING) → Sort → Limit → Project
```

1. **`FROM`:** each table is a `Scan` (an unknown one is `42P01`, from the catalog) and its scope; a join is a `Join` whose `ON` binds over the scope of **its two sides only** (ISO/IEC 9075, 7.7): in `a JOIN b ON … JOIN c ON …` the first `ON` cannot see `c`, and in `a, b JOIN c ON …` it cannot see `a`. Without `FROM`, `OneRow` and an empty scope.
2. **`WHERE`:** bound over the scope; an aggregate is `42803`.
3. **Grouped?** A query is grouped when it has `GROUP BY` or `HAVING`, or an aggregate in the select list or `ORDER BY`. The `GROUP BY` expressions bind over the input (an aggregate there is `42803`, and an integer alone is refused with `0A000` rather than grouping everything by a constant where a position was meant); the select list, `HAVING` and `ORDER BY` bind in the grouped mode, and only then is the `Aggregate` built, with every aggregate they call. Without `GROUP BY`, the whole input is one group: `SELECT count(*) FROM t` gives one row, and so does `SELECT 1 FROM t HAVING TRUE`.
4. **`HAVING`:** a `Filter` over the grouped row.
5. **Select list:** `*` and `t.*` expand in the order of the tables (in a grouped query, each column must be a group). An output column without an alias is named after its column, its function, or else `?column?`.
6. **`ORDER BY`:** each item is a `SortKey` over the rows **before** the projection:
   - an unsigned integer is a position in the select list (`ORDER BY 2`); outside `1..=n`, `InvalidPosition` (`42P10`), naming the position without its leading zeros;
   - a name alone that is an output name is that column, which wins over a column of the input (`SELECT email AS name … ORDER BY name` sorts by `email`); several output columns of that name are `AmbiguousOrderBy` (`42702`) only if they are different expressions, so `SELECT *, id … ORDER BY id` sorts by the one column both are;
   - anything else is an expression over the input, so `ORDER BY b` works with `b` outside the list; an output name inside an expression (`ORDER BY x + 1`) is not a column (`42703`);
   - without `NULLS FIRST` or `LAST`, a `NULL` sorts as the greatest value: last ascending, first descending.
7. **`LIMIT`, `OFFSET`:** bound over no columns: a column is `ColumnInRowCount` (`42P10`), an aggregate `42803`. Their values are checked when they run (`2201W`, `2201X`).

The `Sort` reads the rows before the `Project` so that `ORDER BY` reaches columns outside the list without hidden columns; an output column named by position or name is sorted by its expression, evaluated again. The `Limit` sits right above the `Sort`, where the physical plan can turn the pair into a `TopN`.

## EXPLAIN

`EXPLAIN SELECT …` gives the logical plan as text (`explain.rs`, the `Display` of `LogicalPlan`): one node per line, each input indented two spaces under the node that reads it, the columns by name instead of position.

```
EXPLAIN SELECT c.city, count(*) AS orders, sum(o.total)
        FROM customers c LEFT JOIN orders o ON o.customer_id = c.id
        WHERE o.placed >= DATE '2024-01-01'
        GROUP BY c.city HAVING sum(o.total) > 100
        ORDER BY 3 DESC, c.city LIMIT 10

Project: c.city AS city, count(*) AS orders, sum(o.total) AS sum
  Limit: 10
    Sort: sum(o.total) DESC NULLS FIRST, c.city ASC NULLS LAST
      Filter: sum(o.total) > 100
        Aggregate: group by c.city; count(*), sum(o.total)
          Filter: o.placed >= DATE '2024-01-01'
            Join LEFT: o.customer_id = c.id
              Scan: customers AS c
              Scan: orders AS o
```

- A compound expression inside another is in parentheses, so the text never depends on precedence: `(t.a + 1) * 2`. A negative number inside another is too: `-(-5)`, since `--` starts a comment.
- Literals are written as SQL: strings quoted with `''` doubled, `DATE '…'`, `TIMESTAMP '…'`, `NULL`, `TRUE`.
- Names that need quotes have them (`"Users"."select"`); the columns above an aggregate are shown as their text (`count(*)`).
- `BETWEEN` and `IN` show rewritten (`(t.a >= 1) AND (t.a <= 2)`), which is what the binder made of them.
- `Join CROSS`, `Join INNER: …`, `Join LEFT: …`; `Limit: ALL OFFSET 3` without a count; `Aggregate: one group` for a `HAVING` without groups or aggregates.

## Errors

`PlannerError` (`error.rs`) keeps the errors of the layers below with their SQLSTATEs (`Sql`, `Catalog`, `Schema`, `Type`, `Execution`) and adds the binder's and the planner's:

| Variant | SQLSTATE | Message |
|---|---|---|
| `UndefinedColumn { column }` | `42703` | `column "x" does not exist` |
| `UndefinedTableReference { table }` | `42P01` | `missing FROM-clause entry for table "u"` |
| `AmbiguousColumn { column }` | `42702` | `column reference "id" is ambiguous` |
| `DuplicateAlias { table }` | `42712` | `table name "t" specified more than once` |
| `DuplicateColumn { column }` | `42701` | `column "id" specified more than once` |
| `DuplicateAssignment { column }` | `42601` | `multiple assignments to same column "v"` |
| `ColumnCountMismatch { expected, found }` | `42601` | `INSERT has more expressions (3) than target columns (2)`, or fewer |
| `WildcardWithoutTables` | `42601` | `SELECT * with no tables specified is not valid` |
| `AggregateNotAllowed { clause }` | `42803` | `aggregate functions are not allowed in WHERE` (also `JOIN conditions`, `GROUP BY`, `LIMIT`, `OFFSET`, `VALUES`, `SET`) |
| `NestedAggregate` | `42803` | `aggregate function calls cannot be nested` |
| `GroupingError { column }` | `42803` | `column "u.email" must appear in the GROUP BY clause or be used in an aggregate function` |
| `InvalidPosition { position }` | `42P10` | `ORDER BY position 3 is not in select list` |
| `AmbiguousOrderBy { name }` | `42702` | `ORDER BY "x" is ambiguous` |
| `ColumnInRowCount { clause }` | `42P10` | `argument of LIMIT must not contain variables` |
| `UndefinedFunction { name }` | `42883` | `function "f" does not exist`, or the whole call for an aggregate with other arguments (`function "sum(*)" does not exist`) |
| `Unsupported { feature }` | `0A000` | `SELECT is not supported`, `GROUP BY position is not supported` |
| `MissingCount` | `XX000` | `a write statement returned no row count`: a write executor that did not give one non-negative `BIGINT`, a plan built wrong |

## Design decisions

- **A binder apart from the statements**, instead of translating inside each one: DML and `SELECT` share it.
- **One binder with modes** (plain, aggregate argument, grouped), instead of a second binder or a rewrite after binding: the rules for literals, `BETWEEN` and `IN` live in one place.
- **Groups compared bound** (`PartialEq` of `Expr`), not as text: `a`, `t.a` and `A` are the same group.
- **The executors' expressions in the logical plan**, instead of a tree of its own: no second `Expr`, and the physical plan will be close to a one-to-one translation. The cost is that the logical plan depends on `execution`, which keeps the layers pointing downward.
- **`Sort` before `Project`**, instead of hidden columns in the projection: `ORDER BY` by alias or position uses the item's expression again, safe without volatile functions.
- **`Limit` above `Sort` and below `Project`:** both orders give the same rows, and this one keeps `Sort` and `Limit` together for a `TopN`.
- **`CROSS JOIN` as `Inner` without `ON`**, reusing `JoinType`: the way the `HashJoin` already runs a join without keys.
- **Repeated aggregates computed once.**
- **`ORDER BY` by position, output name or expression; `GROUP BY` only by expressions over the input** (decided with the author): positions are common in practice; in `GROUP BY` an output name would compete with an input column of the same name.
- **`EXPLAIN` with names instead of positions** (decided with the author): readable for someone studying the plan; the positions are in its `Debug`.
- **No types in the plan:** inferring them is the Type System item; an expression of the wrong type fails when it runs.
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
- `scope.rs`: a column qualified or not, unknown names, the alias hiding the table's name, a join shifting the right table's columns, a column in two tables (`42702`), a name twice in one `FROM` (`42712`), `*` and `t.*`, an unknown `t.*`, `*` without tables.
- `binder.rs`: columns as positions (qualified, any case), an unknown column, a column of another table and a column in `VALUES`; integers at the limits of `INTEGER` and `BIGINT` with and without a sign, past `BIGINT` (`22003`); decimals and exponents as `DOUBLE PRECISION`, `1e400` (`22003`); strings, booleans, `NULL`, dates and timestamps, invalid dates (`22007`, `22008`); operators, `NOT`, signs, `CAST`; `IS` tests with `NULL`; `BETWEEN` at, inside and outside its bounds, reversed and with `NULL`, `NOT BETWEEN`; `IN` with and without `NULL` in the list or the value, `NOT IN`, a list of 1,000 values as a tree 12 nodes deep, the first failing value's error; aggregates (`42803`, naming the clause) and unknown functions (`42883`).
- `logical_plan.rs`: the columns of a scan (by alias or table), a join, a filter, an aggregate (groups by name or text, aggregates by call), a projection and a limit.
- `explain.rs`: a plan with every node indented under its reader; `OneRow` and names that need quotes; literals of every type; compound expressions in parentheses, negative numbers inside others, a position past the row (`#7`); every aggregate; every join type; limits with and without a count; aggregates with groups, aggregates or neither; sort keys.
- `select.rs`: projections and output names; wildcards in the order of the tables; every join kind left to right, parenthesized joins; `WHERE` over a join; `SELECT` without `FROM`; unknown, ambiguous and duplicate names, `t.*` and `*` errors; an `ON` that reads a later table or one before a comma (`42P01`); aggregates in `WHERE` and `ON`; `GROUP BY` with `HAVING`; aggregates and `HAVING` without `GROUP BY`; group expressions inside others and qualified differently; repeated aggregates; every way a column falls outside the groups (list, `HAVING`, `*`, `t.*`, an expression); nested aggregates, aggregates in `GROUP BY`, wrong arguments; `ORDER BY` by position, alias and expression with explicit and default `NULLS`; the output name winning; ambiguous and out-of-range `ORDER BY`, an alias inside an expression; `ORDER BY` in grouped queries (an aggregate, one group, a column outside the groups); `LIMIT` and `OFFSET` above the sort, `OFFSET` alone; counts with a column or an aggregate; two output columns of the same expression under one name (repeated, qualified differently, through `*`) accepted by `ORDER BY`, two different ones through `*` ambiguous; positions with leading zeros, past `usize` and `0`; `GROUP BY 1` refused, `GROUP BY 1 + 0` and `1.5` as expressions; `HAVING` not seeing output names (`42703`); a group expression matching only with the same literal types (`id + 1.0` against `GROUP BY id + 1` is `42803`); the `EXPLAIN` of 190 nested signs, a 190-operator group expression and 199 joins on a 2 MB stack.
- `random_selects_should_plan_with_every_column_in_range`: 2,000 random `SELECT`s over three tables (aliases, every join kind, aggregates, `GROUP BY`, `HAVING`, `ORDER BY` by position, alias and expression, `LIMIT`, sometimes a wrong name); a plan that comes out must have every column of every node inside the row it reads, and each item written as a column must read the input column of that name and table; an error must have one of the planner's SQLSTATEs; more than 400 of them plan.
- `run.rs`: `CREATE TABLE` (the schema in the catalog), an existing table and a repeated column with nothing created; `CREATE UNIQUE INDEX` over rows already there, a column with repeated values (`23505`), unknown table and column, an existing name, `CREATE INDEX` (`0A000`); a syntax error running nothing; `SELECT` (`0A000`); `EXPLAIN` among other statements, and of an invalid query; `INSERT` with and without a column list; every failing `INSERT` (`23502`, `42804`, `22001`, `23505`, `42601` both ways, `42701`, `42703`, `42P01`, a column in `VALUES`, `42803`, `22012`) leaving the table and the index as they were; aggregates refused naming `VALUES`, `SET` and `WHERE`; `UPDATE` of some rows, all rows and the indexed key; every failing `UPDATE` (`42601`, `42703`, `23502`, `23505`, `42P01`, a non-boolean `WHERE`, an unknown table) changing nothing; `DELETE` with a `WHERE` and without; a `DELETE … IN` of 100,000 values on a 2 MB stack; every column type taking its literals (`BIGINT` at its maximum, `SMALLINT` at its minimum, an `INTEGER` literal widened to `SMALLINT`, `BIGINT` and `DOUBLE PRECISION`, `BOOLEAN`, `DATE`, `TIMESTAMP`) and refusing others (`22003`, `22001`, `42804` for a decimal in `BIGINT` and a string in `DATE`); a write executor without its count (`XX000`); several statements running in order and stopping at the first failure.
- `random_statements_should_keep_the_table_like_a_model`: 400 random statements on `(id INTEGER NOT NULL, v INTEGER, s VARCHAR(10))` with a unique index on `id`: multi-row inserts (some with a repeated or `NULL` id), `UPDATE … SET v = v + k WHERE id BETWEEN a AND b`, `UPDATE … SET id = id + k` over a range, `DELETE … WHERE id IN (…)` or `v IS NULL`; a `BTreeMap` model predicts each outcome or SQLSTATE (`23502`, `23505`), and after every statement the table and the index must match it.
- Mutations checked by hand, each caught: `BETWEEN` with `>` instead of `>=` (by the random test, which showed the unit test lacked the bounds themselves; they were added); `IN` as a chain of `OR`s again (by the depth test); the output name losing to an input column in `ORDER BY`; repeated aggregates not reused; the right side of a join not shifted (by the scope tests and, after it was strengthened to check the names columns read, by the random `SELECT` test, which at first only checked the positions were in range). An `ON` that sees a later table cannot be written as a mutation: the `ON` is bound inside the recursion over the `FROM`, before the later tables exist.
- Integration in [`tests/planner_statements.rs`](../tests/planner_statements.rs), with 8 KB pages and an 8-frame pool: SQL creates a table and a unique index, inserts 3,000 rows in 30 statements, updates half of them and deletes a range; after reopening, a `SeqScan` and the index give the expected rows; keys moved by `UPDATE … SET id = id + 10000` survive reopening in the index; a failure in the middle of a script keeps the statements before it.
- Integration in [`tests/planner_select.rs`](../tests/planner_select.rs): the `EXPLAIN` of a report (a `LEFT JOIN`, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY` by position and column, `LIMIT`) gives the same plan after reopening the database; a script mixes DDL, DML and `EXPLAIN` in order; an invalid query (`42803`, `42702`) stops its script and the next statements still run.

## Limitations

- **`SELECT` is planned but does not run** yet: the physical plan comes next. There is no `INSERT … SELECT`.
- **No `SELECT DISTINCT`, subqueries, set operations, `CASE`, `LIKE`, `JOIN … USING` or `NATURAL JOIN`** (the parser does not read them).
- **No functional dependencies:** with a primary key, `SELECT * … GROUP BY pk` would be valid; here every column must be grouped.
- **`GROUP BY` only by expressions over the input**, not by output name or position (a bare integer is refused).
- **No optimization:** filters stay above joins, joins in the order written.
- **No `PRIMARY KEY`, `UNIQUE` columns, `DEFAULT`, `CHECK` or foreign keys**; no `DROP TABLE` or `ALTER TABLE`.
- **Indexes are unique and of one column**, created only with `CREATE UNIQUE INDEX`.
- **No transactions:** a script that fails midway keeps the statements before the failure.
- **No static types:** type errors in expressions show up when they run, and a string is never converted to the type of its column. The plan has names, not types.
- **DML always scans the table**, even when an index could find the rows.
