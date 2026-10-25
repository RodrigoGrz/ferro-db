# Query Execution

This document is the authoritative description of ferroDB's execution layer (`src/execution/`): expressions, the executors, how they read and write tables and keep indexes in step, their errors and their tests. The tables, the catalog and SQL types are in [data-model.md](data-model.md); the B+Tree is in [indexes.md](indexes.md).

Dependencies point downward: `execution` → `catalog` → `table` / `index` → `types` / `storage`. Nothing uses `execution` yet: plans are built by hand in Rust (as BusTub's tests do) until the SQL parser and the planner exist.

- [Iterator model](#iterator-model)
- [Expressions](#expressions)
- [Executors](#executors)
- [Index Scan](#index-scan)
- [Aggregation](#aggregation)
- [Sort, Limit and TopN](#sort-limit-and-topn)
- [Index Nested Loop Join](#index-nested-loop-join)
- [Hash Join](#hash-join)
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

- Rows read from a table carry their **`Rid`**, which Delete and Update need; rows computed by `Values`, the groups of an `Aggregation` and the rows of a join have none. `Filter`, `Projection`, `Sort`, `Limit` and `TopN` keep their child's `Rid`.
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
| `Aggregation::new(child, group_by, aggregates)` | one row per group: the `GROUP BY` values, then one value per aggregate (`COUNT(*)`, `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`), without `Rid` ([below](#aggregation)) |
| `Sort::new(child, keys)` | the child's rows ordered by the keys (`ASC`/`DESC`, `NULLS FIRST`/`LAST`), ties in input order, sorted in memory; keeps the `Rid` ([below](#sort-limit-and-topn)) |
| `Limit::new(child, count, offset)` | skips `offset` rows and hands out at most `count`, never pulling the child further; keeps the `Rid` |
| `TopN::new(child, keys, count, offset)` | the rows of `Limit(Sort(child, keys), count, offset)`, keeping at most `offset + count` rows in a heap |
| `IndexNestedLoopJoin::new(outer, outer_key, index, join_type, predicate)` | for each outer row, the inner rows the index finds for `outer_key` and the predicate accepts, as outer values followed by inner values, without `Rid`; `Inner` or `Left` (padding with `NULL`s) ([below](#index-nested-loop-join)) |
| `HashJoin::new(left, right, keys, join_type, predicate)` | builds a hash table of the right input by key, then joins each left row with its bucket; `Inner`, `Left`, `Right` or `Full`, without `Rid` ([below](#hash-join)) |

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

## Aggregation

`Aggregation` (`aggregation.rs`) is **hash aggregation** (CMU 15-445 lecture 11 and project 3, BusTub's `SimpleAggregationHashTable`). It is a *pipeline breaker*: the first call reads the whole child; for each row it evaluates the `GROUP BY` expressions (the group's key), finds or creates the group in a hash table and updates one accumulator per aggregate. Later calls hand out the groups one by one.

```rust
pub enum Aggregate { CountStar, Count(Expr), Sum(Expr), Avg(Expr), Min(Expr), Max(Expr) }

// SELECT dept, COUNT(*), AVG(salary) FROM staff WHERE age > 30 GROUP BY dept HAVING COUNT(*) > 2
let groups = Aggregation::new(
    Box::new(Filter::new(Box::new(SeqScan::new("staff")), age_above_30)),
    vec![Expr::Column(0)],
    vec![Aggregate::CountStar, Aggregate::Avg(Expr::Column(2))],
);
let having = Filter::new(Box::new(groups),
    Expr::binary(BinaryOp::Gt, Expr::Column(1), Expr::Literal(Value::BigInt(2))));
```

- **Output:** the key's values, then one value per aggregate, by position (like `Projection`, there is no output schema yet). A group is not a table row, so it has no `Rid`.
- **Groups:** two rows share a group when their keys are **not distinct** (`IS NOT DISTINCT FROM`), so every `NULL` key of a column forms one group. The table is a `HashMap<Vec<Value>, Group>` from the key to the group's accumulators and the number of groups that existed before it: `Value` derives `Hash`, and within one type its derived `Eq` is exactly "not distinct" (floats have no NaN or `-0.0`, text compares by bytes). **A key column is assumed to hold one type:** an expression over a schema always yields one type (or `NULL`), so integer widths never mix; a `Values` built by hand that mixes `Integer(1)` and `BigInt(1)` in a column would make two groups, until the planner adds the casts.
- **Without `GROUP BY`** the whole input is one group, so an empty input still gives **one row** (`SELECT COUNT(*) FROM empty` is `0`); with `GROUP BY`, an empty input gives no rows.
- **Order:** the groups come out in the order each **first appeared** in the input (the standard promises no order without `ORDER BY`): once the child is read, the groups (not the rows) are sorted by their arrival.
- **`HAVING`** is a `Filter` over the output, which already keeps only TRUE.
- **States:** `Start` (the child not read yet), `Emitting` (the finished groups), and `Done` after an `Err`, which ends the executor like every other.

### Accumulators

| Aggregate | State | Each non-`NULL` value | Result |
|---|---|---|---|
| `COUNT(*)` | `i64` | every row counts, `NULL`s included | `BIGINT` |
| `COUNT(e)` | `i64` | +1 | `BIGINT`, `0` without values |
| `SUM` of integers | `i128` | exact sum | `BIGINT`; `22003` if it does not fit, checked **only at the end** |
| `SUM` of `REAL` / `DOUBLE PRECISION` | the sum as a `Value` | `Value::add` (overflow `22003`) | the input's type |
| `AVG` of integers | `i128` sum + count | exact sum | `DOUBLE PRECISION`: sum / count |
| `AVG` of floats | `DOUBLE PRECISION` sum + count | `Value::add` | `DOUBLE PRECISION` |
| `MIN` / `MAX` | `Option<Value>` | `Value::compare` | the input's type |

- **`NULL` is skipped** by every aggregate but `COUNT(*)` (`COUNT(*)` is `COUNT` of a constant that is never `NULL`). Over no values, `COUNT` is `0` and the others `NULL`.
- **The type of an accumulator is fixed by its first non-`NULL` value.** `SUM`/`AVG` of text, `BOOLEAN`, `DATE` or `TIMESTAMP` is `UndefinedFunction` (`42883`), raised at that value: a column of only `NULL`s never fails. `MIN`/`MAX` accept any comparable type. A value of another family later on only happens in a plan built wrong, and fails with the comparison's error (`42883`).
- **Integer sums do not depend on the order of the rows:** `MAX + 1 - 1` is `MAX`, since the `i128` is checked against `BIGINT` at the end. An integer average divides the sum rounded to `f64`, so the average of huge `BIGINT`s does not overflow. A count past `i64` (2^63 rows) would be `22003`, not a wrap.

## Sort, Limit and TopN

`Sort` (`sort.rs`) is a pipeline breaker too (CMU 15-445 lecture 10): the first call reads the whole child, computes each row's sort key and sorts the rows **in memory**; later calls hand them out one by one. `Limit` (`limit.rs`) is a counter in the pipeline. `TopN` (`sort.rs`) is `ORDER BY … LIMIT` with a bounded heap (BusTub's project 3, PostgreSQL's bounded heap sort).

```rust
pub enum SortOrder { Asc, Desc }
pub enum NullOrder { First, Last }
pub struct SortKey { pub expr: Expr, pub order: SortOrder, pub nulls: NullOrder }
// SortKey::asc(expr): ASC NULLS LAST; SortKey::desc(expr): DESC NULLS FIRST

// SELECT name, score FROM players ORDER BY score DESC, name LIMIT 10 OFFSET 20
let ranked = Sort::new(Box::new(SeqScan::new("players")),
    vec![SortKey::desc(Expr::Column(1)), SortKey::asc(Expr::Column(0))]);
let page = Limit::new(Box::new(ranked), Some(Expr::Literal(Value::BigInt(10))),
    Some(Expr::Literal(Value::BigInt(20))));
// the same rows, keeping at most 30 in memory
let page = TopN::new(Box::new(SeqScan::new("players")),
    vec![SortKey::desc(Expr::Column(1)), SortKey::asc(Expr::Column(0))],
    Expr::Literal(Value::BigInt(10)), Some(Expr::Literal(Value::BigInt(20))));
```

### Order

- **Each row's key** is a `Vec` of values that carry their key's direction and `NULL` position, and that order implements `Ord`: two `NULL`s are equal; a single `NULL` goes first or last as `nulls` says, **whatever the direction**; two values compare with `Value::sort_cmp`, reversed for `DESC`. The `Vec` compares key by key, so a later key only breaks ties.
- **`Value::sort_cmp`** is a total order: numbers (integers and floats interleaved by value, compared exactly), then text (by UTF-8 bytes, `NO PAD`), booleans (`FALSE < TRUE`) and datetimes (by instant). A well-typed column never mixes families.
- **Default `NULL` position:** as PostgreSQL, `NULL` is greater than every value: last for `ASC`, first for `DESC`. `NULLS FIRST` / `NULLS LAST` override it.
- **Ties keep their input order:** `Sort` uses a stable sort, and `TopN` breaks ties by the row's arrival number. The output is deterministic, and `TopN` gives exactly the rows of `Limit(Sort)`.
- **Keys are computed once per row, before sorting:** the comparison cannot fail, and a failing key (`22012`…) stops the executor before any row is out.

### Counts

`LIMIT` and `OFFSET` are expressions without columns (`Limit::new(child, count, offset)`, `None` meaning `LIMIT ALL` and `OFFSET 0`), evaluated **once, on the first call and before the child is pulled**, like the bounds of an `IndexScan`:

| Count | Result |
|---|---|
| an integer `≥ 0` of any width | the count (assigned to `BIGINT`) |
| `NULL` | `InvalidRowCount`: `2201W` for `LIMIT`, `2201X` for `OFFSET` (`LIMIT must not be null`) |
| negative | `InvalidRowCount`, same SQLSTATEs (`OFFSET must not be negative, got -5`) |
| a float, text, boolean, date | `42804`, the store assignment rule (a float becomes an integer only through `CAST`) |
| an expression with a column | `ColumnOutOfRange` (`XX000`) |

- **`Limit` never pulls its child after the count is reached**, and `LIMIT 0` does not pull it at all: below a `Limit`, the rest of a table is never read.
- **PostgreSQL differs on `NULL`:** it takes `LIMIT NULL` as no limit and `OFFSET NULL` as `0`. ferroDB follows the standard, where a `NULL` row count is an exception (decided with the author); no limit is `count: None`.

### TopN

- `TopN::new(child, keys, count, offset)` validates the counts like `Limit` before reading the child; `count = 0` reads nothing.
- It keeps the best `offset + count` rows in a max-heap whose root is the worst of them: rows are pushed until the heap holds `offset + count`; after that a row replaces the root only when it comes before it (`peek_mut`), so most rows of a large input cost one comparison. At the end the heap is sorted, `offset` rows are skipped and the rest come out. Memory is O(`offset + count`) and time O(rows · log(`offset + count`)).
- The bound is `offset + count` saturated, and the heap is **not reserved up front**: `LIMIT 9223372036854775807` allocates nothing in advance.
- Choosing `TopN` over `Limit(Sort)` will be the planner's job; today whoever builds the plan chooses.

`Sort`, `Limit` and `TopN` all keep each row's `Rid`: they still hand out table rows, so `Delete(Limit(…))` works.

## Index Nested Loop Join

`IndexNestedLoopJoin` (`join.rs`) joins two tables through an index of the inner one (CMU 15-445 lecture 12; BusTub's `NestedIndexJoin` in project 3; PostgreSQL's nested loop over an index scan). A plain nested loop scans the whole inner table for every outer row, M + m·N pages; through the index each outer row costs one descent of the B+Tree and the fetch of its row, M + m·C. The plain nested loop is not implemented, on purpose (decided with the author), and since the inner side is reached through the index, no executor ever has to start over: `Executor` keeps only `next`.

```rust
pub enum JoinType { Inner, Left }

// SELECT o.id, o.amount, c.name FROM orders o JOIN customers c ON o.customer_id = c.id AND c.active
// orders: (id, customer_id, amount); customers: (id, name, active), index customers_id
let joined = IndexNestedLoopJoin::new(
    Box::new(SeqScan::new("orders")),
    Expr::Column(1),            // o.customer_id, over the outer row
    "customers_id",
    JoinType::Inner,
    Some(Expr::Column(5)),      // c.active, over the joined row (3 + 2)
);
```

- **The first call** looks the index and its table up in the catalog: the table's name, the key type and its number of columns. An unknown index is `42704`.
- **For each outer row:** `outer_key` is evaluated over it and converted to the key type by `key_bounds::point_key`, the rule of an `IndexScan::point` bound: an integer of another width or a `DATE` for a `TIMESTAMP` key is assigned; `NULL`, a value the key type rounds (`0.1` for a `REAL` key) or cannot hold (`70000` for a `SMALLINT` key) matches nothing; another family (text or a float for an integer key) is `42804`. `key_bounds::converts_to_key` says, by type, which keys convert without an error; the planner uses an index only for those, so the plan it picks never turns a comparison into a conversion error. The range `[key, key]` of the tree gives the `Rid`s, and `TableHeap::get` the inner rows; a `Rid` the table does not have is `XX000`, as in the `IndexScan`.
- **A joined row** is the outer values followed by the inner table's values, and has **no `Rid`**: it is not a row of one table.
- **The residual predicate** (the rest of the `ON`) is evaluated over the joined row and only TRUE matches (`42804` if it is not boolean). It decides what matches: in a `LEFT` join, an outer row whose pair it rejects comes out padded with `NULL`s, where a `Filter` above the join would drop it.
- **`LEFT`:** an outer row without a match (no key in the index, a `NULL` key, or every pair rejected) comes out once, followed by as many `NULL`s as the inner table has columns.
- **Order:** the outer order; for one outer row, its matches in key order.
- **Nothing stays pinned:** the matches of an outer row are read into a `Vec` within the call and the range over the tree is dropped, as the `IndexScan` does with its cursor. Every index is unique today, so an outer row has at most one match; iterating the range keeps the join right for non-unique indexes.

## Hash Join

`HashJoin` (`hash_join.rs`) joins two inputs by equal keys without an index (CMU 15-445 lecture 12). The first call reads the whole right input into a hash table by key (build); then each left row looks its bucket up (probe). Each input is read once; the right one is kept in memory.

```rust
pub struct JoinInput { pub child: Box<dyn Executor>, pub width: usize }

// SELECT * FROM customers c FULL JOIN regions r ON c.region = r.code
// customers: (id, name, region SMALLINT); regions: (code BIGINT, name), no index
let joined = HashJoin::new(
    JoinInput { child: Box::new(SeqScan::new("customers")), width: 3 },
    JoinInput { child: Box::new(SeqScan::new("regions")), width: 2 },
    vec![(Expr::Column(2), Expr::Column(0))],   // (over the left row, over the right row)
    JoinType::Full,
    None,
);
```

- **Keys** are pairs of expressions, one over the left row and one over the right row; two rows match when every pair is equal and the residual predicate is TRUE. With no keys every pair of rows matches: `Inner` without a predicate is a `CROSS JOIN`.
- **Normalized keys.** `Value`'s derived `Eq` separates `Integer(1)` from `BigInt(1)` and `Double(1.0)`, but SQL's `=` does not, and the two sides of a join may have different types. Each key value is hashed as: an integer of any width, or a float whose value is an `i64`, as that integer (`1 = 1.0`); any other float as the bits of its exact `f64` (`REAL 0.1 <> DOUBLE PRECISION 0.1`, the exact comparison of the types layer); text as its bytes (`NO PAD`); a `DATE` as its midnight and a `TIMESTAMP` as itself, in microseconds. Two normalized keys are equal exactly when `=` is TRUE.
- **Families:** the first non-`NULL` right value of each key column is kept, and every later key value, right or left, is compared with it by `Value::compare`: keys of different families fail with the `=` error (`42883`). With an empty right input nothing is checked.
- **`NULL`** in any key never matches; the row still comes out padded in the outer joins.
- **Outer joins:** `Left` and `Full` pad a left row without a match with `right.width` `NULL`s where it is. Each right row carries its arrival number and whether a pair with it passed the predicate; `Right` and `Full` hand out the right rows that never matched (`NULL` keys included) at the end, in input order, after `left.width` `NULL`s. The predicate decides: a right row whose only pair it rejects comes out padded.
- **Widths:** an empty input does not say how many values its rows have, and the outer joins need them, so each `JoinInput` declares its width; a row of another width is `RowWidthMismatch` (`XX000`).
- **Order:** the left input's order; for one left row, its matches in the right input's order; unmatched right rows last.
- **States:** `Start` (build on the first call), `Probing` (one left row at a time, its joined rows pending), `Finishing` (the unmatched right rows) and `Done`, also after an `Err`.

The `IndexNestedLoopJoin` refuses `Right` and `Full` with `UnsupportedJoinType` (`0A000`): it finds inner rows only for outer ones, and cannot tell which inner rows never matched.

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
| `UndefinedFunction { function, arg }` | `42883`: an aggregate given a type it does not accept (`function sum(varchar) does not exist`) |
| `InvalidRowCount { clause, count }` | `2201W` for `LIMIT`, `2201X` for `OFFSET`: a `NULL` (`count: None`) or negative count |
| `UnsupportedJoinType { join_type }` | `0A000`: a `RIGHT` or `FULL` index nested loop join |
| `RowWidthMismatch { expected, actual }` | `XX000`: a row of a join input whose width is not the declared one (a plan built wrong) |

## Design decisions

See [ADR 0011](decisions/0011-executors-with-context-per-call-and-two-phase-writes.md) for the context passed to each call and the two-phase writes. Also:

- **A trait for executors:** there are fourteen real implementations now and more to come, and a tree of `Box<dyn Executor>` is the Volcano model itself.
- **A cursor on the heap** (`HeapFile::next_after`) instead of holding a `TableScan` (which borrows the buffer pool) or reading the whole table: the scan stays lazy. The cost is fetching the page again for every row, almost always a cache hit.
- **The Index Scan's cursor is the last key**, not a leaf and a position in it: a leaf kept between calls could be merged away and freed by a write. It reuses `BPlusTree::range` (create, take one entry, drop), which already copies the leaf and unpins it before handing out entries; the cost is copying a leaf per row, which a leaf cursor can remove later.
- **Bounds that do not fit the key type narrow the range** instead of failing: `small_col < 70000` is a valid query and must return every row.
- **Columns by position** in expressions: names belong to the planner; there is no output schema to maintain yet.
- **The count as an output row**, as in BusTub, instead of a different return type: every executor has the same interface.
- **Keys checked with `get` before writing**, instead of letting the tree's `insert` fail and undoing: "fail without side effects" ([ADR 0007](decisions/0007-fail-without-side-effects.md)).
- **Hash aggregation in memory**, instead of sort aggregation: it is what CMU and BusTub do in project 3, and there is no Sort yet. The cost is the memory of every group; spilling partitions to disk (external hashing) is for later.
- **`Hash` derived on `Value`** as the group key, instead of a key encoded in bytes: `Finite32`/`Finite64`, `Date` and `Timestamp` already have a `Hash` coherent with `Eq`.
- **Groups in order of first appearance**, instead of the `HashMap`'s order: deterministic output and simple tests. The map holds each group with its arrival number and the groups are sorted by it at the end, O(g log g) in the number of groups, instead of a map to positions in a `Vec`, which cloned every key and indexed the `Vec`.
- **Integer `SUM` in `i128`**, checked at the end, instead of `Value::add` row by row: exact, and the result does not depend on the order of the rows.
- **`AVG` of integers as `DOUBLE PRECISION`** (decided with the author), instead of truncating to `BIGINT`: `AVG(1, 2)` is `1.5`, and only the type changes when `NUMERIC` arrives.
- **`HAVING` as a `Filter`** over the output, instead of a field of `Aggregation`: the `Filter` already has the semantics of a `WHERE`, and BusTub builds the plan the same way.
- **One state for the pipeline breakers** (`materialized.rs`: `Start`, `Emitting`, `Done`), shared by `Aggregation`, `Sort` and `TopN`: reading the whole child on the first call and ending the executor after an error live in one place.
- **Sort in memory** (decided with the author), instead of an external merge sort: like the Aggregation, with no temporary pages to manage. The cost is the whole input in memory.
- **Sort keys computed before sorting**, instead of evaluating the expressions inside the comparison: each expression runs once per row, errors come before sorting, and the comparison cannot fail (`sort_by` takes no fallible comparator).
- **A stable sort**, instead of `sort_unstable_by`: deterministic output, equal to `TopN`'s; the cost is a buffer of n/2.
- **The direction inside each key value**, instead of passing the `SortKey`s to the comparison: keys implement `Ord`, so `sort_by` and `BinaryHeap` take them as they are. The cost is two `Copy` enums per value.
- **`TopN` as its own executor** (decided with the author), as BusTub does, instead of a mode of `Sort`; the random test compares it with `Limit(Sort)`.
- **Counts as expressions without columns**, evaluated once, instead of `u64`s in the constructor: the planner will pass literals and parameters (`LIMIT $1`), and the checks (`NULL`, negative, type) live in one place, with SQLSTATEs.
- **The hash join builds on the right input**, always, instead of the smaller one: without statistics nothing tells which is smaller; the optimizer will order the inputs.
- **The hash table in memory**, instead of a Grace or hybrid hash join that partitions to disk: like the Aggregation and the Sort. The cost is the whole right input in memory.
- **Normalized join keys**, instead of `Value` itself as in `GROUP BY`: the two sides may have different types (`SMALLINT` against `BIGINT`, `INTEGER` against `DOUBLE PRECISION`), which the derived `Eq` would keep apart; in a `GROUP BY` a column has one type.
- **Key families checked against the first right key**, with `Value::compare`, instead of letting keys of different families simply never match: the error is the one `=` gives in a `Filter`, for one comparison per key.
- **`JoinInput` declares its width**, instead of learning it from the rows: an empty input has no rows to learn from, and the outer joins pad with it. Each row is checked against it.
- **`RIGHT` and `FULL` by marking the right rows** (decided with the author): possible because the build side is in memory. The `IndexNestedLoopJoin`, which cannot see unmatched inner rows, refuses them with `0A000`.
- **Buckets that own their rows and arrival numbers**, instead of positions into a `Vec` of rows: no indexing in production code; the unmatched right rows are gathered from the buckets and sorted by arrival at the end, O(n log n) in those rows only.
- **Key pairs `Vec<(Expr, Expr)>`**, instead of two lists: the two sides can never have a different number of keys.
- **One rule for the `ON`** (`join::joined` and `join::passes`), shared by both joins: a joined row is the left values then the right ones, without `Rid`, and only a TRUE predicate matches.
- **An index nested loop join instead of the plain one** (decided with the author): one descent of the B+Tree per outer row instead of a scan of the inner table. Joins without an index on the inner side or without an equality wait for a Hash Join or a planner.
- **The join key converted like an `IndexScan::point` bound** (`point_key`), instead of a rule of its own: `WHERE c.id = 5` and the join behave the same, rounding and out-of-range values included.
- **The range `[key, key]` instead of `BPlusTree::get`:** at most one match with unique indexes, and still right when indexes may repeat keys.
- **The matches of an outer row in a `Vec`**, instead of keeping the iterator over the tree between calls: it borrows the buffer pool and would hold a leaf a merge could free.
- **The residual predicate inside the join** (decided with the author), instead of a `Filter` above: in a `LEFT` join it decides what matches.
- **`INNER` and `LEFT` only** (decided with the author): `RIGHT` and `FULL` need to know which inner rows never matched, which only a scan of the inner table tells.
- **The inner width from the catalog**, read on the first call: a `LEFT` join pads with it even when no inner row is ever read.
- **A `NULL` count is an error** (the standard, decided with the author), instead of PostgreSQL's no limit.

## Tests

- `expression.rs`: columns and literals, a column past the row, the six comparisons with `NULL` on each side, the whole Kleene table for `AND`/`OR`/`NOT`, logic on a non-boolean (`42804`), arithmetic with `NULL` and width promotion, `22012`/`22003`/`42883`, `||`, `IS [NOT] NULL`, every boolean test negated or not (and on an integer), `IS [NOT] DISTINCT FROM` (and across types, `42883`), `CAST` with `22P02` and of `NULL`, a nested expression.
- `scan.rs`: `Values` (rows, no rows, a failing expression, a column with no row to read, `None` after the end); `SeqScan` (empty table, 80 rows over several pages with every third one deleted and each `Rid` right, a 2-frame pool, reading after reopening, an unknown table, `None` after the end).
- `key_bounds.rs`: an open bound, `NULL` on each side, assignment within a family (`SMALLINT` to `INTEGER`, `INTEGER` to `DOUBLE PRECISION`, `DATE` to `TIMESTAMP`), text longer than the `VARCHAR`, the four corners of an integer outside the key type (and `BIGINT` past `INTEGER`), a float key that rounds the bound up or down on each side (`0.1`, `16777217` and `2^53 + 1`), a float bound past `REAL` or too close to zero for it, and every refused conversion (`42804`); `converts_to_key` agreeing with the conversion for a value of every type against every key type.
- `scan.rs`, `IndexScan`: a point that exists, that does not and `NULL`; an unknown index (`42704`), a bound with a column (`XX000`), a failing bound (`22012`) and one of another family (`42804`); a `Rid` the table no longer has (`XX000`, then `None`); `None` after the end; inclusive, exclusive, open, empty and reversed ranges; 300 rows inserted in scattered order read back in key order, equal to the stored rows with their `Rid`s, through a 2-frame pool; no row with a `NULL` key; bounds converted to the key type; an empty table; rows removed by a Delete between two calls, merging the leaves under the cursor (the scan goes on after its last key); a `TIMESTAMP` index read with `DATE` bounds; a `REAL` index read with `DOUBLE PRECISION` bounds that round.
- `random_index_scans_should_match_a_filtered_seq_scan`: 600 steps on a table with an `INTEGER`, a `SMALLINT` and a `REAL` index, mixing inserts, deletes and updates read through the index with scans of random bounds (open, inclusive, exclusive, `NULL`, every integer width, past `SMALLINT` and `INTEGER`; for the `REAL` index, `DOUBLE PRECISION` tenths that round, exact keys, integers, values past `REAL` and too close to zero); each scan must equal `Filter(SeqScan)` with the same comparisons, sorted by key; reopening every 150 steps.
- `filter.rs` / `projection.rs`: TRUE kept and FALSE and `NULL` dropped, a non-boolean predicate, the `Rid` passed through, expressions over the row, a failing expression, a `Projection(Filter(SeqScan))` pipeline over a table of several pages.
- `modify.rs`, on a table with unique indexes on two columns, checking after every statement that each index holds exactly the non-null keys of its column with the right `Rid`s:
  - Insert: rows assigned to the column types and indexed; a table inserted into itself (each row copied once); a repeated key against the index and between the rows of the statement; every row the table refuses (`23502`, `42804`, `22001`, `42601`, `54000`), each leaving the table and the indexes exactly as they were; `NULL` keys left out; an unknown table; the count once and then `None`;
  - Delete: with a filter, every row, nothing to delete, keys taken from the stored row whatever the child computed, a `Rid` given twice, a row without a `Rid`, a row already gone and rows of another table (nothing changes), an unknown table;
  - Update: matching rows, `SET a = b, b = a`, every row updated once even when the rows move, a key another row holds and a key two updated rows would share (nothing changes), two keys swapped, every value the table refuses (nothing changes), an index whose key did not change repointed when its row moves, an index of an unchanged column left alone, a row without a `Rid`, a `Rid` given twice, rows of another table (nothing changes), an unknown table;
  - through an `IndexScan`: a Delete of a range, an Update of a point, and an Update of the scanned key itself (`id = id + 10` over 26 rows, each updated once).
- `random_statements_should_keep_the_table_and_its_indexes_like_a_model`: 300 random statements (multi-row inserts, deletes and three kinds of updates over a range of ids, one of them growing rows until they move or their keys become too large), many of them breaking a unique index, compared with a model that predicts each error (`23505`, `54000`), reopening every 50 statements.
- `aggregation.rs`: `COUNT(*)` and `COUNT(e)` with `NULL`s; an empty input without `GROUP BY` (one row of `0`s and `NULL`s); `MIN`/`MAX` of integers, text (`'a' < 'a '`), dates, booleans, `REAL`, `DOUBLE PRECISION` and timestamps, of only `NULL`s, and of two families (`42883`); `SUM` of every integer width (`BIGINT`) and of `REAL` and `DOUBLE PRECISION` (their type); a sum that overflows halfway and fits at the end, one that does not fit (`22003`), a float overflow in `SUM` and in `AVG` (`22003`); integers and floats mixed in a `SUM` (added as `+` would, and `22003` when the integer part does not fit); `AVG` of integers (`1.5`), of `REAL` (`DOUBLE PRECISION`) and of two `i64::MAX`; `SUM`/`AVG` of text, `BOOLEAN`, `DATE` and `TIMESTAMP` after `NULL`s (`42883` and its message); `GROUP BY` one and two keys in order of first appearance, `NULL` keys in one group, keys apart like `IS DISTINCT FROM` (`'a'` and `'a '` apart, `-0.0` with `0.0`, dates), `GROUP BY` without aggregates (each key once), `id % 3` as the key, an empty input (no rows), a failing key or argument (`22012`, then `None`), a failing child (`42P01`, then `None`), no `Rid` over a `SeqScan`, `HAVING` as a `Filter`; `None` after the end. `value.rs` checks that equal values (`-0.0` and `0.0` included) hash the same.
- `random_aggregations_should_match_a_model`: 60 rounds over a table of `(SMALLINT, VARCHAR, INTEGER, DOUBLE PRECISION)` with `NULL`s, changed by Insert and Delete executors, grouped by one or two columns with all ten aggregates and compared, sorted by key, with a model computed in `i128`, `f64` (in the rows' order) and `String`, without `Value` arithmetic.
- `sort.rs`: `ASC` and `DESC` with their default `NULL` positions, `NULLS FIRST`/`LAST` in both directions, later keys only breaking ties, integer widths and floats interleaved and equal values of different widths tying, text by bytes (`'a' < 'a '`), dates, booleans, timestamps and `REAL`; `Sort` and `TopN` without keys (input order); `Sort` of no rows and of one, ties in input order, the `Rid`s of a table spanning pages, a failing key (`22012`) and a failing child (`42P01`), each ending the sort; `TopN` equal to `Limit(Sort)` for several counts and offsets (past the end included), ties in input order, `count = 0` without reading the child, a count of `i64::MAX` with an offset of `i64::MAX - 1` and of `1`, every invalid count before reading the child (`2201W`, `2201X`, `42804`), a failing key and a failing count (`22012`), the `Rid`s.
- `limit.rs`: every combination of count and offset (none, one, both, offset past the end, `0`), every integer width, a float, text or boolean count (`42804`), `NULL` and negative counts (`2201W`, `2201X` and their messages), a count with a column (`XX000`), a count or offset that fails to evaluate (`22012`), the child not pulled after the count nor with `LIMIT 0`, an invalid count failing before the child is pulled, the `Rid`s, `None` after the end.
- `random_sorts_and_limits_should_match_a_model`: 80 rounds over a table of `(INTEGER, VARCHAR, DOUBLE PRECISION)` with `NULL`s and few distinct values, changed by Insert and Delete executors, sorted by 1 to 3 random keys (direction and `NULL` position random) with random counts and offsets; `Sort`, `Limit(Sort)` and `TopN` must equal, row and `Rid`, a stable sort of the stored rows by a model comparator over `Option<i32>`, `Option<String>` and `Option<f64>`.
- `key_bounds.rs`, `point_key`: an integer of another width, a `DATE` for a `TIMESTAMP` key, an exact `DOUBLE PRECISION` for a `REAL` key and text longer than the `VARCHAR` are assigned; `NULL`, `0.1` and `1e-50` for a `REAL` key, `70000` for a `SMALLINT` key and `i64::MIN` for an `INTEGER` key match nothing; text and a float for an integer key are `42804`.
- `join.rs`: `INNER` pairs in outer order, a `NULL` outer key, a `SMALLINT` key against an `INTEGER` index, a key of another family (`42804`), an empty outer side and an empty inner table, an unknown index (`42704`), a failing key (`22012`, after a first row, then `None`), no `Rid` over a `SeqScan`; `LEFT` padding unmatched rows, `NULL` keys and every row of an empty inner table with the inner width; a residual predicate dropping pairs in `INNER` (`FALSE` and `UNKNOWN`), padding them in `LEFT` where a `Filter` above drops them, reading both sides, not boolean (`42804`) or failing (`22012`); an index out of step with its table (`XX000`, then `None`); a failing outer child (`42P01`); an outer key longer than the index can hold (no match, padded in `LEFT`); a `DOUBLE PRECISION` key against a `REAL` index (`0.5` matches, `0.1` does not) and a `DATE` key against a `TIMESTAMP` index (midnight matches); a self-join of a table scanned and searched through its own index.
- `random_joins_should_match_a_plain_nested_loop`: 80 rounds over `orders (INTEGER, SMALLINT, INTEGER)` and `customers (INTEGER, VARCHAR, BOOLEAN)` with a unique index, both changed by Insert and Delete executors (customers deleted under their orders, orders with `NULL` or missing customers), joined `INNER` or `LEFT` with no predicate, `c.active` or `o.amount > 50`; the output must equal, row by row and in order, a plain nested loop over the stored rows written in Rust, without the index or `key_bounds`.
- `hash_join.rs`: normalized keys equal for integers of every width, an integer and an integral `REAL` or `DOUBLE PRECISION`, `REAL 0.5` and `DOUBLE PRECISION 0.5`, equal text, booleans, a `DATE` and its midnight `TIMESTAMP`; different for `0.5` against `0` and `1`, `REAL 0.1` against `DOUBLE PRECISION 0.1`, floats past the `i64` range against its limits, `'a'` against `'a '`, the next day, text against a number; no key for `NULL`. `INNER` pairs many-to-many in left then right order, two keys, keys of different numeric types, `NULL` in either key, keys of different families from the left and among right rows (`42883`), empty inputs, no keys (every pair), a row of another width on each side (`XX000`), a failing key on each side (`22012`) and a failing child (`42P01`), no `Rid`. `LEFT` padding in place, `RIGHT` adding unmatched right rows at the end, `FULL` both, the declared width of an empty side, `NULL` keys padded on both sides in arrival order. A residual predicate dropping pairs, a right row whose only pair it rejects padded in `RIGHT` and `FULL`, a predicate not boolean (`42804`) or failing (`22012`). A key of another family against an empty right input (no error: no match, padded in `LEFT`), a `DATE` key joining the midnight `TIMESTAMP` and not ten o'clock, outer joins of two empty inputs (no rows), a failing left child after the build (`42P01`). `join.rs`: `RIGHT` and `FULL` refused by the index nested loop join (`0A000`).
- `random_hash_joins_should_match_a_nested_loop`: 300 rounds of two `VALUES` inputs with repeated and `NULL` keys, a `SMALLINT`, `INTEGER` or `DOUBLE PRECISION` key on the left against an `INTEGER`, `BIGINT` or `REAL` key on the right (with halves no integer equals), one or two keys, the four join types and an optional predicate; the output must equal, in order, a nested loop over the rows that compares numbers as `f64` and labels as `String`, without the normalized key or a hash table.
- Mutations checked by hand, each caught: for the hash join, integral floats not normalized, a right row marked as matched before the predicate, and unmatched right rows not sorted by arrival; for the index join, a predicate ignored, `LEFT` padding outer rows that matched, and inner rows that fail to load skipped instead of failing; for Sort and TopN, an unstable sort, `DESC` also moving the `NULL`s, `TopN` without the arrival tiebreak, a full heap letting a later tie replace its worst row, and reading the child with `count = 0`; for the Aggregation, a key cut to its first expression (caught by the random test) and an executor that reads on after an error; entries rewritten row by row instead of in two passes, the keys of replaced rows not allowed, a new `Rid` ignored when the key did not change, and Insert without the uniqueness check; for the Index Scan, each corner of the narrowed range flipped, `NULL` as no bound, an inclusive bound made exclusive, the upper bound converted as a lower one, and an empty range scanned whole; in the float rules, rounding ignored, a rounded-up bound made exclusive on the upper side the wrong way, a float overflow failing and a value near zero not taken as zero (the random test alone catches each of `key_bounds.rs`).
- Integration in [`tests/execution_executors.rs`](../tests/execution_executors.rs), with 8 KB pages and an 8-frame pool: 3,000 rows inserted through the executors, updated in place and with rows moving, and deleted with `%`; after reopening, the scan and both indexes match. A refused insert leaves no trace after reopening.
- Integration in [`tests/execution_index_scan.rs`](../tests/execution_index_scan.rs), same setup, 3,000 rows in scattered order: after reopening, points and ranges through the `INTEGER` and the `VARCHAR` index equal `Filter(SeqScan)`; an `UPDATE … SET id = id + 10000, email = email || <300 bytes> WHERE id BETWEEN 1000 AND 1999` read through the index moves every key past the cursor and every row out of its page, and after reopening the scans and both indexes match.
- Integration in [`tests/execution_aggregation.rs`](../tests/execution_aggregation.rs), same setup, 4,000 orders with `NULL` customers and amounts: `GROUP BY customer` over a `SeqScan` equals a model, and again after reopening; over an `IndexScan` of a range with a `HAVING`, it equals the same plan over `Filter(SeqScan)` and the model; a Delete of one customer removes its group and changes the total `COUNT(*)`.
- Integration in [`tests/execution_sort.rs`](../tests/execution_sort.rs), same setup, 3,000 players inserted in scattered id order: `ORDER BY team, score DESC, id` over a `SeqScan` equals a model and gives the same rows after reopening; `TopN` equals `Limit(Sort)` over the whole table; a `Limit` with an offset over an `IndexScan` gives the first keys of the range; `ORDER BY COUNT(*) DESC, team` over an `Aggregation` sorts the groups.
- Integration in [`tests/execution_join.rs`](../tests/execution_join.rs), same setup, 4,000 orders and 257 customers: `INNER` and `LEFT` over a `SeqScan` equal a model after reopening; a join feeds an `Aggregation` (revenue per region) and a `Sort`; deleting customers through the executors removes their pairs from `INNER` and pads them in `LEFT`.
- Integration in [`tests/execution_hash_join.rs`](../tests/execution_hash_join.rs), same setup, 400 customers, 3,000 orders and 5 regions: the four join types on a `SMALLINT` column against a `BIGINT` one without index equal a model after reopening; `INNER` and `LEFT` equal the `IndexNestedLoopJoin` where the right column has an index; two chained hash joins feed an `Aggregation`.

## Limitations

- ⚠️ **No atomicity across I/O failures:** an I/O error while writing can leave a statement half done; the WAL and transactions will fix it.
- ⚠️ **Writes keep the child's rows in memory**, all of them, before writing.
- ⚠️ **Writing through `TableHeap` directly does not update the indexes**; only the executors do. An Index Scan over such an index fails with `XX000`, or misses rows.
- ⚠️ **The Index Scan goes down the tree and copies a leaf for every row.**
- ⚠️ **Aggregation keeps every group in memory**; there is no spilling to disk.
- ⚠️ **Aggregate types are checked while running**, at the first non-`NULL` value, not before: a `SUM` of text over only `NULL`s does not fail. The planner will check them with an output schema.
- **Aggregates are not complete:** no `COUNT(DISTINCT x)` or `SELECT DISTINCT`, no `FILTER (WHERE …)`, `STRING_AGG`, `EVERY`/`BOOL_AND`, `GROUPING SETS`/`ROLLUP`/`CUBE` or window functions.
- ⚠️ **Sort keeps its whole input in memory**; there is no external merge sort. `TopN` keeps `offset + count` rows.
- **Sorting is not complete:** no `FETCH FIRST … WITH TIES` or `PERCENT`, no `COLLATE`, and no planner to choose `TopN` over `Limit(Sort)`.
- ⚠️ **Index Scan bounds must convert to the key type without loss** (`42804` otherwise), and there is no planner yet to choose between Index Scan and SeqScan.
- ⚠️ **Joins need an equality** (or no condition at all): the index nested loop join needs an index on the inner side, and the hash join keeps its whole right input in memory, with no partitioning to disk. There is no plain nested loop or Sort-Merge Join for other conditions (`ON a.x < b.y` only works as a residual predicate), and no planner to choose the join, its build side or the index.
- ⚠️ **Hash join key types are checked while running**, and only when both sides have values: a key of the wrong family against an empty right input does not fail.
- **Not there yet:** an output schema with names and types, a descending or index-only scan, and joins.
