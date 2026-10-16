# 0006. Catalog bootstrapped from page 0, stored in two heaps with its own record format

- Status: Accepted
- Date: 2026-10-08 (commit `7399f35`)

## Context

The catalog (tables, columns, where each table's data starts) must itself live on disk, and something has to say where. A table can have up to 1,600 columns, so a single record per table could exceed a page. There is no String type yet, so the catalog cannot be stored as ordinary typed tables.

## Decision

- Page 0 gets a dedicated page type, `DatabaseHeader`, holding a magic number and the first page of two catalog heaps.
- Tables and columns are stored in **two separate heaps** (one `TableRecord` per table, one `ColumnRecord` per column), like PostgreSQL's `pg_class` and `pg_attribute`.
- Records use their own little-endian format through the serialization codec (called "option B" in the design notes), so the catalog did not have to wait for strings.
- `create_table` writes the column records first and the table record **last**: the table record is the commit point. Ids are consumed before writing and never reused; columns without a table record are ignored as orphans.
- The whole catalog is loaded into memory on `open`.

## Alternatives considered

Recorded in the design notes:

- *Waiting for the String type to store the catalog as system tables:* deferred. The public API (`TableInfo`, `create_table`) is meant to stay the same when the catalog becomes system tables.
- *Page 0 as a slotted page with a special tuple:* rejected in favor of `PageType::DatabaseHeader`, so the exhaustive `match` forces every page reader to decide what to do with it. `PAGE_FORMAT_VERSION` did not change, because no existing page changed format.
- *One record per table with all its columns:* would exceed 100 KB with 1,600 columns.

## Consequences

- Without a WAL, "table record last" protects against failures **inside the process**, including a failed undo, but not against a real crash: pages can reach the disk in any order, which can leave orphan columns or, in the worst case, a table record without columns (`CorruptedSchema` on `open`).
- Heaps created by a failed `create_table` or an interrupted database creation are orphaned until a future `VACUUM`.
- The record format is provisional; old databases will not open when the catalog becomes system tables.
- Table ids can have gaps.

## Implementation and tests

- [`src/catalog/`](../../src/catalog/) (`header.rs`, `records.rs`, `catalog.rs`, `error.rs`)
- Unit tests in each module, including injected failures and `random_creations_should_match_model_across_reopens`
- [`tests/catalog_table_metadata.rs`](../../tests/catalog_table_metadata.rs)
- Description: [data-model.md § Catalog](../data-model.md#catalog)
