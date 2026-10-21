# Query Execution

This document is the authoritative description of ferroDB's execution layer (`src/execution/`): expressions, the executors, how they read and write tables and keep indexes in step, their errors and their tests. The tables, the catalog and SQL types are in [data-model.md](data-model.md); the B+Tree is in [indexes.md](indexes.md).

Dependencies point downward: `execution` → `catalog` → `table` / `index` → `types` / `storage`. Nothing uses `execution` yet: plans are built by hand in Rust (as BusTub's tests do) until the SQL parser and the planner exist.

- [Iterator model](#iterator-model)
- [Expressions](#expressions)
- [Executors](#executors)
- [Index Scan](#index-scan)
- [Writing: two phases](#writing-two-phases)
- [Errors](#errors)
- [Design decisions](#design-decisions)
- [Tests](#tests)
- [Limitations](#limitations)

---

## Iterator model

Every executor hands out **one row per `next` call**, pulling rows from its children only as it needs them; a query is a tree of executors whose root is pulled until it ends (CMU 15-445 lectures 12–13 and project 3; Graefe's *Volcano*).

```rust
pub struct Row { pub values: Vec<Value>, pub rid: Option<Rid> }
pub struct ExecutionContext<'a> { pub catalog: &'a mut Catalog, pub bpm: &'a mut BufferPoolManager }

pub trait Executor {
    /// The next row, or `None` when there are no more (and on every call after that).
    fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>, ExecutionError>;
}

pub fn execute(root: &mut dyn Executor, ctx: &mut ExecutionContext<'_>) -> Result<Vec<Row>, ExecutionError>;
```

```rust
// SELECT name || '!', id FROM users WHERE id >= 50 AND name IS NOT NULL
let filter = Filter::new(
    Box::new(SeqScan::new("users")),
    Expr::binary(BinaryOp::And,
        Expr::binary(BinaryOp::Ge, Expr::Column(0), Expr::Literal(Value::Integer(50))),
        Expr::IsNull { expr: Box::new(Expr::Column(1)), negated: true }),
);
let mut query = Projection::new(Box::new(filter), vec![
    Expr::binary(BinaryOp::Concat, Expr::Column(1), Expr::Literal(Value::Text("!".into()))),
    Expr::Column(0),
]);
let rows = execute(&mut query, &mut ExecutionContext { catalog: &mut catalog, bpm: &mut bpm })?;
```

- Rows read from a table carry their **`Rid`**, which Delete and Update need; rows computed by `Values` have none. `Filter` and `Projection` keep their child's `Rid`.
- The **database is passed to each call** (`ExecutionContext`) instead of being stored in the executors, so a tree of them never holds two mutable borrows of the catalog or the buffer pool, and **no executor keeps a page pinned between calls**.
- **After an `Err`, an executor is finished:** its children may have been consumed halfway, so it must not be called again.
- **A `SeqScan` does not shield itself from changes made between two of its `next` calls** by other code: it simply goes on after the last `Rid` it returned. The writing executors never interleave with their own scan, because they read their child completely first; a plan built by hand that interleaves a scan with writes to the same table sees those writes, or not, depending on where they land.

## Expressions

`Expr` (`expression.rs`) is evaluated over one row (`eval(&[Value])`). Columns are referred to by **position**; resolving names is the planner's job.

| `Expr` | Meaning | Implemented by |
|---|---|---|
| `Column(i)` | the `i`-th value of the row (`ColumnOutOfRange`, `XX000`, past the end) | — |
| `Literal(value)` | the value | — |
| `Binary { op, left, right }` | `=` `<>` `<` `<=` `>` `>=`, `AND` `OR`, `+` `-` `*` `/` `%`, `\|\|` | `sql_eq`…`sql_ge`, `and`/`or`, `add`…`rem`, `concat` |
| `Not(e)`, `Negate(e)` | `NOT e`, `-e` | `not`, `neg` |
| `IsNull { e, negated }` | `e IS [NOT] NULL` | `is_null` |
| `Test { e, test, negated }` | `e IS [NOT] TRUE / FALSE / UNKNOWN` | `is_true`, `is_false`, `is_unknown` |
| `IsDistinctFrom { l, r, negated }` | `l IS [NOT] DISTINCT FROM r` | `is_distinct_from` |
| `Cast { e, to }` | `CAST(e AS to)` | `cast` |

Every operator is the `Value` operation of the same name, so `NULL`, three-valued logic, type rules and SQLSTATEs are exactly those of the [types](data-model.md): comparisons with `NULL` are UNKNOWN, `AND`/`OR`/`NOT` follow Kleene, the tests and `IS DISTINCT FROM` are never UNKNOWN, a non-boolean operand of `AND` is `42804`, division by zero `22012`, overflow `22003`, a comparison across families `42883`. Both sides of `AND` and `OR` are always evaluated: the standard does not promise short-circuiting, and `Value::and` type-checks both operands anyway.

## Executors

| Executor | Behavior |
|---|---|
| `SeqScan::new(table)` | the rows of the table in heap order, each with its `Rid`. It keeps a cursor (the last `Rid` read) and asks `TableHeap::next_after` for the next row on every call, so no page stays pinned between calls. An unknown table is `42P01` on the first call |
| `IndexScan::new(index, from, to)`, `IndexScan::point(index, key)` | the rows of the index's table whose key is within the bounds, **in key order**, each with its `Rid` ([below](#index-scan)). An unknown index is `42704` on the first call |
| `Values::new(rows)` | rows of expressions with no columns to read (`VALUES (1, 'a'), (2, 'b')`), without `Rid` |
| `Filter::new(child, predicate)` | the rows whose predicate is **TRUE** (`is_true`): a `WHERE` drops FALSE and UNKNOWN alike; a predicate that is not boolean is `42804` |
| `Projection::new(child, exprs)` | a new row with the value of each expression over the child's row; keeps the `Rid` |
| `Insert::new(table, child)` | inserts the child's rows |
| `Delete::new(table, child)` | deletes the rows the child produces, by `Rid` |
| `Update::new(table, child, assignments)` | sets `(column, expr)` assignments on the rows the child produces; every expression sees the stored row **before** the update, all at once, so `SET a = b, b = a` swaps |

Insert, Delete and Update return **one row with the number of rows they changed** (a `BIGINT`), as BusTub's do, and `None` afterwards. A row without a `Rid` reaching Delete or Update is `MissingRid` (`XX000`); a `Rid` given twice counts once.

## Index Scan

`IndexScan` (`scan.rs`) reads a table through one of its B+Tree indexes (CMU 15-445 lecture 13 and project 3): it goes down the tree to the first key of the range, follows the leaves to the end of it, and **fetches each row** from the table by the entry's `Rid`.

```rust
// SELECT * FROM users WHERE id >= 10 AND id < 20, through users_id
let mut scan = IndexScan::new("users_id",
    Bound::Included(Expr::Literal(Value::Integer(10))),
    Bound::Excluded(Expr::Literal(Value::Integer(20))));
// SELECT * FROM users WHERE email = 'ana@x'
let mut point = IndexScan::point("users_email", Expr::Literal(Value::Text("ana@x".into())));
```

- **The bounds are expressions without columns**, evaluated once, on the first call: the planner will pass literals and parameters. A bound that reads a column is `ColumnOutOfRange` (`XX000`); one that fails is its own error (`22012`…).
- **The cursor is the last key returned.** Each call asks `BPlusTree::range(bpm, Excluded(last key), upper)` for its first entry and drops the iterator, so no page stays pinned between calls, and no leaf position is kept that a merge could free in between. Going down the tree again costs O(log n) pages per row, almost always cache hits.
- **A `Rid` the table does not have** (an index out of step with its table, written through `TableHeap` directly) is `XX000` instead of a wrong row; the scan ends there, like every executor after an `Err`.
- **Rows with a `NULL` key never come out:** the index does not hold them, as no comparison with `NULL` is TRUE. `IS NULL` needs a `SeqScan`.
- **Halloween:** `UPDATE … SET id = id + 10 WHERE id >= 5` read through the index of `id` would meet every updated key again further on. The writing executors read their child completely before writing ([below](#writing-two-phases)), so each row is updated once.

### Bounds in the key's type

Before the scan starts, each bound is converted to the type of the index's key (`key_bounds.rs`); the tree compares encoded keys of one type only.

| Bound | Becomes |
|---|---|
| `NULL` | an empty range (`key < NULL` is UNKNOWN) |
| text, on a `VARCHAR(n)` key | the text as it is, even longer than `n`: it only compares |
| the same family (an integer of another width, an integer or a `DOUBLE PRECISION` for a float key, a `DATE` for a `TIMESTAMP` key) | the value assigned to the key type (`Value::assign`; a `DATE` is its midnight) |
| a number a float key rounds (`0.1` for a `REAL` key, an integer past 2^24 for `REAL` or 2^53 for `DOUBLE PRECISION`) | the rounded value, made inclusive or exclusive so that it admits the same keys: no key lies strictly between the bound and its rounding, so a bound rounded up admits the keys from it on the lower side and below it on the upper side, and a bound rounded down the other way round. `real_col <= 0.1` does not admit the REAL `0.1`, which is above the DOUBLE PRECISION `0.1` |
| a number the key type cannot hold (`70000` for a `SMALLINT` key, `1e39` for a `REAL` key) | the range narrows instead of failing: above the maximum, a lower bound empties the range and an upper bound is no bound; below the minimum, the other way round. A `DOUBLE PRECISION` too close to zero for a `REAL` (`1e-50`) is zero, rounded as above |
| another family, or a conversion that would lose information (a `REAL` for an `INTEGER` key, a `TIMESTAMP` for a `DATE` key, text for an integer key) | the assignment's error, `42804` |

The last row is a [deviation](data-model.md#deviations-from-the-sql-standard): `WHERE int_col < 2.5` is valid SQL. The planner will use a `Filter` for such predicates.

## Writing: two phases

1. **Read** every row the child produces. This solves the **Halloween problem**: an update that moves a row to a later page (its `Rid` changes) would meet it again in the scan and update it twice. It also lets the whole statement be validated.
2. **Validate the whole statement, writing nothing:**
   - Insert and Update build each new row and pass it through `TableHeap::prepare`, which applies the table's rules exactly as `insert` would (arity `42601`, NOT NULL `23502`, store assignment `42804`/`22001`/`22003`, the size of a tuple `54000`) and returns the values **in the column types**, which is what the indexes need as keys;
   - Delete and Update read each stored row by `Rid` (a row that is gone fails here), and use its values, not the child's, as the keys to take out;
   - for **every index** of the table, the new keys (`NULL` keys are never indexed) must fit the tree (`BPlusTree::check_key`: type, size `54000`), must not repeat among the new rows, and must not hit an entry of a row the statement does not replace (`BPlusTree::get`); otherwise `IndexError::DuplicateKey` (`23505`). An Update may give a row a key another updated row is giving up (swapping two keys is valid).
3. **Write:**
   - Insert: each row into the heap, then its keys into each index;
   - Delete: the keys out of each index, then the row out of the heap;
   - Update: each row into the heap (its `Rid` may change), then, index by index, every entry that changed (a new key or a new `Rid`) is first taken out for all rows and only then put back, so keys swapped between rows of the statement never meet in the tree. An index whose key and `Rid` did not change is not touched.

A statement that fails on its data or a constraint therefore leaves the table and its indexes as they were. Only an I/O failure in phase 3 can leave a statement half done, until there is a WAL.

## Errors

`ExecutionError` (`error.rs`) wraps the error of the layer that failed and keeps its SQLSTATE:

| Variant | SQLSTATE |
|---|---|
| `Type(TypeError)` | the `TypeError`'s (`22012`, `22003`, `42804`, `42883`, `22P02`…) |
| `Catalog(CatalogError)` | the `CatalogError`'s (`42P01`, `42704`…) |
| `Table(TableError)` | the `TableError`'s (`23502`, `22001`, `42601`, `54000`…) |
| `Index(IndexError)` | the `IndexError`'s (`23505`, `54000`…) |
| `ColumnOutOfRange { column, len }` | `XX000` (a plan built wrong) |
| `MissingRid` | `XX000` (a plan built wrong) |

## Design decisions

See [ADR 0011](decisions/0011-executors-with-context-per-call-and-two-phase-writes.md) for the context passed to each call and the two-phase writes. Also:

- **A trait for executors:** there are eight real implementations now and more to come, and a tree of `Box<dyn Executor>` is the Volcano model itself.
- **A cursor on the heap** (`HeapFile::next_after`) instead of holding a `TableScan` (which borrows the buffer pool) or reading the whole table: the scan stays lazy. The cost is fetching the page again for every row, almost always a cache hit.
- **The Index Scan's cursor is the last key**, not a leaf and a position in it: a leaf kept between calls could be merged away and freed by a write. It reuses `BPlusTree::range` (create, take one entry, drop), which already copies the leaf and unpins it before handing out entries; the cost is copying a leaf per row, which a leaf cursor can remove later.
- **Bounds that do not fit the key type narrow the range** instead of failing: `small_col < 70000` is a valid query and must return every row.
- **Columns by position** in expressions: names belong to the planner; there is no output schema to maintain yet.
- **The count as an output row**, as in BusTub, instead of a different return type: every executor has the same interface.
- **Keys checked with `get` before writing**, instead of letting the tree's `insert` fail and undoing: "fail without side effects" ([ADR 0007](decisions/0007-fail-without-side-effects.md)).

## Tests

- `expression.rs`: columns and literals, a column past the row, the six comparisons with `NULL` on each side, the whole Kleene table for `AND`/`OR`/`NOT`, logic on a non-boolean (`42804`), arithmetic with `NULL` and width promotion, `22012`/`22003`/`42883`, `||`, `IS [NOT] NULL`, every boolean test negated or not (and on an integer), `IS [NOT] DISTINCT FROM` (and across types, `42883`), `CAST` with `22P02` and of `NULL`, a nested expression.
- `scan.rs`: `Values` (rows, no rows, a failing expression, a column with no row to read, `None` after the end); `SeqScan` (empty table, 80 rows over several pages with every third one deleted and each `Rid` right, a 2-frame pool, reading after reopening, an unknown table, `None` after the end).
- `key_bounds.rs`: an open bound, `NULL` on each side, assignment within a family (`SMALLINT` to `INTEGER`, `INTEGER` to `DOUBLE PRECISION`, `DATE` to `TIMESTAMP`), text longer than the `VARCHAR`, the four corners of an integer outside the key type (and `BIGINT` past `INTEGER`), a float key that rounds the bound up or down on each side (`0.1`, `16777217` and `2^53 + 1`), a float bound past `REAL` or too close to zero for it, and every refused conversion (`42804`).
- `scan.rs`, `IndexScan`: a point that exists, that does not and `NULL`; an unknown index (`42704`), a bound with a column (`XX000`), a failing bound (`22012`) and one of another family (`42804`); a `Rid` the table no longer has (`XX000`, then `None`); `None` after the end; inclusive, exclusive, open, empty and reversed ranges; 300 rows inserted in scattered order read back in key order, equal to the stored rows with their `Rid`s, through a 2-frame pool; no row with a `NULL` key; bounds converted to the key type; an empty table; rows removed by a Delete between two calls, merging the leaves under the cursor (the scan goes on after its last key); a `TIMESTAMP` index read with `DATE` bounds; a `REAL` index read with `DOUBLE PRECISION` bounds that round.
- `random_index_scans_should_match_a_filtered_seq_scan`: 600 steps on a table with an `INTEGER`, a `SMALLINT` and a `REAL` index, mixing inserts, deletes and updates read through the index with scans of random bounds (open, inclusive, exclusive, `NULL`, every integer width, past `SMALLINT` and `INTEGER`; for the `REAL` index, `DOUBLE PRECISION` tenths that round, exact keys, integers, values past `REAL` and too close to zero); each scan must equal `Filter(SeqScan)` with the same comparisons, sorted by key; reopening every 150 steps.
- `filter.rs` / `projection.rs`: TRUE kept and FALSE and `NULL` dropped, a non-boolean predicate, the `Rid` passed through, expressions over the row, a failing expression, a `Projection(Filter(SeqScan))` pipeline over a table of several pages.
- `modify.rs`, on a table with unique indexes on two columns, checking after every statement that each index holds exactly the non-null keys of its column with the right `Rid`s:
  - Insert: rows assigned to the column types and indexed; a table inserted into itself (each row copied once); a repeated key against the index and between the rows of the statement; every row the table refuses (`23502`, `42804`, `22001`, `42601`, `54000`), each leaving the table and the indexes exactly as they were; `NULL` keys left out; an unknown table; the count once and then `None`;
  - Delete: with a filter, every row, nothing to delete, keys taken from the stored row whatever the child computed, a `Rid` given twice, a row without a `Rid`, a row already gone and rows of another table (nothing changes), an unknown table;
  - Update: matching rows, `SET a = b, b = a`, every row updated once even when the rows move, a key another row holds and a key two updated rows would share (nothing changes), two keys swapped, every value the table refuses (nothing changes), an index whose key did not change repointed when its row moves, an index of an unchanged column left alone, a row without a `Rid`, a `Rid` given twice, rows of another table (nothing changes), an unknown table;
  - through an `IndexScan`: a Delete of a range, an Update of a point, and an Update of the scanned key itself (`id = id + 10` over 26 rows, each updated once).
- `random_statements_should_keep_the_table_and_its_indexes_like_a_model`: 300 random statements (multi-row inserts, deletes and three kinds of updates over a range of ids, one of them growing rows until they move or their keys become too large), many of them breaking a unique index, compared with a model that predicts each error (`23505`, `54000`), reopening every 50 statements.
- Mutations checked by hand, each caught: entries rewritten row by row instead of in two passes, the keys of replaced rows not allowed, a new `Rid` ignored when the key did not change, and Insert without the uniqueness check; for the Index Scan, each corner of the narrowed range flipped, `NULL` as no bound, an inclusive bound made exclusive, the upper bound converted as a lower one, and an empty range scanned whole; in the float rules, rounding ignored, a rounded-up bound made exclusive on the upper side the wrong way, a float overflow failing and a value near zero not taken as zero (the random test alone catches each of `key_bounds.rs`).
- Integration in [`tests/execution_executors.rs`](../tests/execution_executors.rs), with 8 KB pages and an 8-frame pool: 3,000 rows inserted through the executors, updated in place and with rows moving, and deleted with `%`; after reopening, the scan and both indexes match. A refused insert leaves no trace after reopening.
- Integration in [`tests/execution_index_scan.rs`](../tests/execution_index_scan.rs), same setup, 3,000 rows in scattered order: after reopening, points and ranges through the `INTEGER` and the `VARCHAR` index equal `Filter(SeqScan)`; an `UPDATE … SET id = id + 10000, email = email || <300 bytes> WHERE id BETWEEN 1000 AND 1999` read through the index moves every key past the cursor and every row out of its page, and after reopening the scans and both indexes match.

## Limitations

- ⚠️ **No atomicity across I/O failures:** an I/O error while writing can leave a statement half done; the WAL and transactions will fix it.
- ⚠️ **Writes keep the child's rows in memory**, all of them, before writing.
- ⚠️ **Writing through `TableHeap` directly does not update the indexes**; only the executors do. An Index Scan over such an index fails with `XX000`, or misses rows.
- ⚠️ **The Index Scan goes down the tree and copies a leaf for every row.**
- ⚠️ **Index Scan bounds must convert to the key type without loss** (`42804` otherwise), and there is no planner yet to choose between Index Scan and SeqScan.
- **Not there yet:** an output schema with names and types, a descending or index-only scan, Sort, Limit, aggregations and joins.
