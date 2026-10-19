use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use crate::storage::buffer_pool::BufferPoolManager;
use crate::storage::heap_file::HeapFile;
use crate::storage::page::RESERVED_PAGE_ID;
use crate::storage::rid::Rid;
use crate::storage::slotted_page::max_tuple_size_for;
use crate::table::{Column, Schema, TableHeap};

use super::records::{ColumnRecord, TableRecord};
use super::{CatalogError, DatabaseHeader};

const FIRST_TABLE_ID: u64 = 1;

/// Identifies a table in the catalog; a newtype so it is never mixed up with a page id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    pub id: TableId,
    pub name: String,
    pub schema: Schema,
    /// First page of the table's data; `HeapFile::open` reads the rows from there.
    pub first_page_id: u32,
}

#[derive(Debug)]
pub struct Catalog {
    tables_heap: HeapFile,
    columns_heap: HeapFile,
    tables: Vec<TableInfo>,
    /// Each table is opened once, on first use, and the same handle is handed out after that.
    open_tables: HashMap<TableId, TableHeap>,
    /// `u64` so a stored id of `u32::MAX` still loads; creating the next table then fails.
    next_table_id: u64,
}

impl Catalog {
    pub const MAX_TABLE_NAME_LEN: usize = Schema::MAX_COLUMN_NAME_LEN;

    /// Creates the database when page 0 was never written, otherwise loads its catalog.
    pub fn open(bpm: &mut BufferPoolManager) -> Result<Self, CatalogError> {
        match read_header(bpm)? {
            None => Self::create_database(bpm),
            Some(header) => Self::load(bpm, header),
        }
    }

    /// The header is written last: until then page 0 stays `Free` and the next `open`
    /// starts over (the heaps created here become orphan pages).
    fn create_database(bpm: &mut BufferPoolManager) -> Result<Self, CatalogError> {
        let tables_heap = HeapFile::create(bpm)?;
        let columns_heap = HeapFile::create(bpm)?;
        write_header(
            bpm,
            DatabaseHeader {
                tables_first_page_id: tables_heap.first_page_id(),
                columns_first_page_id: columns_heap.first_page_id(),
            },
        )?;
        Ok(Self {
            tables_heap,
            columns_heap,
            tables: Vec::new(),
            open_tables: HashMap::new(),
            next_table_id: FIRST_TABLE_ID,
        })
    }

    fn load(bpm: &mut BufferPoolManager, header: DatabaseHeader) -> Result<Self, CatalogError> {
        let tables_heap = HeapFile::open(bpm, header.tables_first_page_id)?;
        let columns_heap = HeapFile::open(bpm, header.columns_first_page_id)?;
        let table_records = decode_all(&tables_heap, bpm, TableRecord::decode)?;
        let column_records = decode_all(&columns_heap, bpm, ColumnRecord::decode)?;

        // Orphan columns (from a create_table that failed) also count, so their id is
        // never handed to a new table that would then inherit them.
        let highest_id = table_records
            .iter()
            .map(|record| record.table_id)
            .chain(column_records.iter().map(|record| record.table_id))
            .max();
        let next_table_id = highest_id.map_or(FIRST_TABLE_ID, |TableId(id)| u64::from(id) + 1);

        Ok(Self {
            tables_heap,
            columns_heap,
            tables: assemble(table_records, column_records)?,
            open_tables: HashMap::new(),
            next_table_id,
        })
    }

    /// In creation order.
    pub fn tables(&self) -> &[TableInfo] {
        &self.tables
    }

    /// The table's rows, opened on first use; every later call returns the same handle.
    pub fn table_heap(
        &mut self,
        bpm: &mut BufferPoolManager,
        name: &str,
    ) -> Result<&mut TableHeap, CatalogError> {
        let table = find_table(&self.tables, name)?;
        let heap = match self.open_tables.entry(table.id) {
            Entry::Occupied(open) => open.into_mut(),
            Entry::Vacant(closed) => closed.insert(TableHeap::open(
                bpm,
                table.schema.clone(),
                table.first_page_id,
            )?),
        };
        Ok(heap)
    }

    pub fn table(&self, name: &str) -> Result<&TableInfo, CatalogError> {
        find_table(&self.tables, name)
    }

    /// Everything is validated before any page is touched. The table's data heap is created
    /// first, then one entry per column, and the table entry last: it is what makes the table
    /// exist on the next `open`. A failure leaves no table behind, but the id stays consumed.
    pub fn create_table(
        &mut self,
        bpm: &mut BufferPoolManager,
        name: &str,
        schema: Schema,
    ) -> Result<&TableInfo, CatalogError> {
        check_table_name(name)?;
        if self.table(name).is_ok() {
            return Err(CatalogError::DuplicateTable {
                name: name.to_string(),
            });
        }
        let max = max_tuple_size_for(bpm.page_size());
        if schema.min_row_size() > max {
            return Err(CatalogError::RowTooWide {
                table: name.to_string(),
                size: schema.min_row_size(),
                max,
            });
        }
        let id =
            TableId(u32::try_from(self.next_table_id).map_err(|_| CatalogError::TooManyTables)?);
        // Consumed before anything is written: if the rollback below fails, column entries
        // with this id stay behind, and a new table reusing it would inherit them.
        self.next_table_id += 1;

        let data_heap = TableHeap::create(bpm, schema.clone())?;
        let table = TableInfo {
            id,
            name: name.to_string(),
            schema,
            first_page_id: data_heap.first_page_id(),
        };
        self.persist(bpm, &table)?;

        self.open_tables.insert(id, data_heap);
        let index = self.tables.len();
        self.tables.push(table);
        Ok(&self.tables[index])
    }

    /// On failure, deletes the column entries already written (best effort): without the
    /// table entry they would be ignored anyway, but they would waste space.
    fn persist(
        &mut self,
        bpm: &mut BufferPoolManager,
        table: &TableInfo,
    ) -> Result<(), CatalogError> {
        let mut written = Vec::new();
        let result = self.write_entries(bpm, table, &mut written);
        if result.is_err() {
            for rid in written {
                let _ = self.columns_heap.delete(bpm, rid);
            }
        }
        result
    }

    fn write_entries(
        &mut self,
        bpm: &mut BufferPoolManager,
        table: &TableInfo,
        written: &mut Vec<Rid>,
    ) -> Result<(), CatalogError> {
        for (position, column) in table.schema.columns().iter().enumerate() {
            let record = ColumnRecord {
                table_id: table.id,
                position: u16::try_from(position).map_err(|_| CatalogError::Corrupted {
                    reason: "column position does not fit u16",
                })?,
                column: column.clone(),
            };
            written.push(self.columns_heap.insert(bpm, &record.encode()?)?);
        }
        let record = TableRecord {
            table_id: table.id,
            first_page_id: table.first_page_id,
            name: table.name.clone(),
        };
        self.tables_heap.insert(bpm, &record.encode()?)?;
        Ok(())
    }
}

/// Exact byte comparison, like column names: case folding is the parser's job.
fn find_table<'a>(tables: &'a [TableInfo], name: &str) -> Result<&'a TableInfo, CatalogError> {
    tables
        .iter()
        .find(|table| table.name == name)
        .ok_or_else(|| CatalogError::UndefinedTable {
            name: name.to_string(),
        })
}

fn decode_all<T>(
    heap: &HeapFile,
    bpm: &mut BufferPoolManager,
    decode: fn(&[u8]) -> Result<T, CatalogError>,
) -> Result<Vec<T>, CatalogError> {
    heap.scan(bpm).map(|entry| decode(&entry?.1)).collect()
}

/// Joins table entries with their columns, in id order. Columns of an unknown table are
/// orphans and are dropped.
fn assemble(
    mut table_records: Vec<TableRecord>,
    column_records: Vec<ColumnRecord>,
) -> Result<Vec<TableInfo>, CatalogError> {
    table_records.sort_by_key(|record| record.table_id);
    let mut columns_by_table: HashMap<TableId, Vec<ColumnRecord>> = HashMap::new();
    for record in column_records {
        columns_by_table
            .entry(record.table_id)
            .or_default()
            .push(record);
    }

    let mut names = HashSet::new();
    let mut tables: Vec<TableInfo> = Vec::with_capacity(table_records.len());
    for record in table_records {
        if tables.last().is_some_and(|last| last.id == record.table_id) {
            return Err(CatalogError::Corrupted {
                reason: "two tables have the same id",
            });
        }
        if !names.insert(record.name.clone()) {
            return Err(CatalogError::Corrupted {
                reason: "two tables have the same name",
            });
        }
        let columns = columns_by_table
            .remove(&record.table_id)
            .unwrap_or_default();
        let schema = Schema::new(ordered_columns(columns)?).map_err(|source| {
            CatalogError::CorruptedSchema {
                table: record.name.clone(),
                source,
            }
        })?;
        tables.push(TableInfo {
            id: record.table_id,
            name: record.name,
            schema,
            first_page_id: record.first_page_id,
        });
    }
    Ok(tables)
}

fn ordered_columns(mut records: Vec<ColumnRecord>) -> Result<Vec<Column>, CatalogError> {
    records.sort_by_key(|record| record.position);
    let numbered = records
        .iter()
        .enumerate()
        .all(|(index, record)| usize::from(record.position) == index);
    if !numbered {
        return Err(CatalogError::Corrupted {
            reason: "columns of a table are not numbered 0..n",
        });
    }
    Ok(records.into_iter().map(|record| record.column).collect())
}

fn check_table_name(name: &str) -> Result<(), CatalogError> {
    if name.is_empty() {
        return Err(CatalogError::EmptyTableName);
    }
    if name.len() > Catalog::MAX_TABLE_NAME_LEN {
        return Err(CatalogError::TableNameTooLong {
            name: name.to_string(),
            max: Catalog::MAX_TABLE_NAME_LEN,
        });
    }
    Ok(())
}

fn read_header(bpm: &mut BufferPoolManager) -> Result<Option<DatabaseHeader>, CatalogError> {
    let page = bpm.fetch_page(RESERVED_PAGE_ID)?;
    let header = DatabaseHeader::read(page.data());
    bpm.unpin_page(RESERVED_PAGE_ID, false);
    header
}

fn write_header(bpm: &mut BufferPoolManager, header: DatabaseHeader) -> Result<(), CatalogError> {
    let page = bpm.fetch_page(RESERVED_PAGE_ID)?;
    let written = header.write(page.data_mut());
    bpm.unpin_page(RESERVED_PAGE_ID, written.is_ok());
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::buffer_pool::BufferPoolError;
    use crate::storage::heap_file::HeapFileError;
    use crate::storage::rid::Rid;
    use crate::storage::slotted_page::SlottedPage;
    use crate::table::{Column, SchemaError, TableError};
    use crate::types::{DataType, Value};
    use std::fs;
    use std::num::NonZeroU16;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(super) const TEST_PAGE_SIZE: usize = 512;

    pub(super) struct TestPool {
        path: PathBuf,
        bpm: Option<BufferPoolManager>,
    }

    impl TestPool {
        pub(super) fn new(pool_size: usize) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("ferro-catalog-{}-{id}.db", std::process::id()));
            let bpm = BufferPoolManager::new(pool_size, TEST_PAGE_SIZE, &path).unwrap();
            Self {
                path,
                bpm: Some(bpm),
            }
        }

        pub(super) fn bpm(&mut self) -> &mut BufferPoolManager {
            self.bpm.as_mut().unwrap()
        }

        /// Drops the pool (flushing every page) and opens the file again.
        pub(super) fn reopen(&mut self, pool_size: usize) {
            self.bpm.take();
            self.bpm = Some(BufferPoolManager::new(pool_size, TEST_PAGE_SIZE, &self.path).unwrap());
        }
    }

    impl Drop for TestPool {
        fn drop(&mut self) {
            self.bpm.take();
            let _ = fs::remove_file(&self.path);
        }
    }

    fn stored_header(bpm: &mut BufferPoolManager) -> Option<DatabaseHeader> {
        let header =
            DatabaseHeader::read(bpm.fetch_page(RESERVED_PAGE_ID).unwrap().data()).unwrap();
        bpm.unpin_page(RESERVED_PAGE_ID, false);
        header
    }

    #[test]
    fn open_of_a_new_file_should_create_an_empty_database() {
        let mut pool = TestPool::new(4);

        let catalog = Catalog::open(pool.bpm()).unwrap();

        assert!(catalog.tables().is_empty());
        let header = stored_header(pool.bpm()).unwrap();
        assert_ne!(header.tables_first_page_id, RESERVED_PAGE_ID);
        assert_ne!(header.columns_first_page_id, RESERVED_PAGE_ID);
        assert_ne!(header.tables_first_page_id, header.columns_first_page_id);
    }

    #[test]
    fn reopening_an_empty_database_should_not_create_it_again() {
        let mut pool = TestPool::new(4);
        Catalog::open(pool.bpm()).unwrap();
        let created = stored_header(pool.bpm());
        pool.reopen(4);

        let catalog = Catalog::open(pool.bpm()).unwrap();

        assert!(catalog.tables().is_empty());
        assert_eq!(stored_header(pool.bpm()), created);
    }

    pub(super) fn accounts() -> Schema {
        Schema::new(vec![
            Column::not_null("id", DataType::Integer),
            Column::new("age", DataType::SmallInt),
            Column::new("balance", DataType::BigInt),
        ])
        .unwrap()
    }

    fn bigints(count: usize) -> Schema {
        Schema::new(
            (0..count)
                .map(|i| Column::new(format!("c{i}"), DataType::BigInt))
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn create_table_should_make_the_table_findable() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();

        let created = catalog
            .create_table(pool.bpm(), "accounts", accounts())
            .unwrap()
            .clone();

        assert_eq!(created.id, TableId(1));
        assert_eq!(created.name, "accounts");
        assert_eq!(created.schema, accounts());
        assert_ne!(created.first_page_id, RESERVED_PAGE_ID);
        assert_eq!(catalog.table("accounts").unwrap(), &created);
        assert_eq!(catalog.tables(), [created]);
    }

    #[test]
    fn tables_should_keep_creation_order_with_increasing_ids() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();

        for name in ["b", "a", "c"] {
            catalog.create_table(pool.bpm(), name, accounts()).unwrap();
        }

        let listed: Vec<(&str, TableId)> = catalog
            .tables()
            .iter()
            .map(|table| (table.name.as_str(), table.id))
            .collect();
        assert_eq!(
            listed,
            [("b", TableId(1)), ("a", TableId(2)), ("c", TableId(3))]
        );
    }

    #[test]
    fn table_of_an_unknown_name_should_fail() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        catalog
            .create_table(pool.bpm(), "users", accounts())
            .unwrap();

        for name in ["missing", "USERS"] {
            let err = catalog.table(name).unwrap_err();

            assert!(matches!(err, CatalogError::UndefinedTable { name: n } if n == name));
        }
    }

    #[test]
    fn create_table_with_an_invalid_name_should_fail() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        let too_long = "t".repeat(Catalog::MAX_TABLE_NAME_LEN + 1);

        let empty = catalog
            .create_table(pool.bpm(), "", accounts())
            .unwrap_err();
        let long = catalog
            .create_table(pool.bpm(), &too_long, accounts())
            .unwrap_err();
        let at_limit = catalog.create_table(
            pool.bpm(),
            &"t".repeat(Catalog::MAX_TABLE_NAME_LEN),
            accounts(),
        );

        assert!(matches!(empty, CatalogError::EmptyTableName));
        assert!(matches!(
            long,
            CatalogError::TableNameTooLong { name, max: 63 } if name == too_long
        ));
        assert!(at_limit.is_ok());
    }

    #[test]
    fn create_table_with_a_name_in_use_should_fail() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        catalog
            .create_table(pool.bpm(), "users", accounts())
            .unwrap();

        let err = catalog
            .create_table(pool.bpm(), "users", bigints(1))
            .unwrap_err();
        let other_case = catalog.create_table(pool.bpm(), "USERS", accounts());

        assert!(matches!(err, CatalogError::DuplicateTable { name } if name == "users"));
        assert!(other_case.is_ok());
        assert_eq!(catalog.table("users").unwrap().schema, accounts());
    }

    /// 512-byte pages hold tuples of up to 484 bytes; `n` BIGINTs need 2 + ceil(n/8) + 8n.
    #[test]
    fn create_table_whose_rows_do_not_fit_a_page_should_fail() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();

        let fits = catalog
            .create_table(pool.bpm(), "fits", bigints(59))
            .map(|table| table.id);
        let err = catalog
            .create_table(pool.bpm(), "wide", bigints(60))
            .unwrap_err();
        let next = catalog
            .create_table(pool.bpm(), "next", accounts())
            .unwrap();

        assert_eq!(fits.unwrap(), TableId(1));
        assert!(matches!(
            err,
            CatalogError::RowTooWide { table, size: 490, max: 484 } if table == "wide"
        ));
        assert_eq!(next.id, TableId(2));
    }

    fn smallints(count: usize) -> Schema {
        Schema::new(
            (0..count)
                .map(|i| Column::new(format!("c{i}"), DataType::SmallInt))
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn reopening_should_restore_every_table() {
        let mut pool = TestPool::new(4);
        let created = {
            let mut catalog = Catalog::open(pool.bpm()).unwrap();
            catalog
                .create_table(pool.bpm(), "accounts", accounts())
                .unwrap();
            catalog
                .create_table(pool.bpm(), "wide", bigints(40))
                .unwrap();
            catalog
                .create_table(
                    pool.bpm(),
                    "flags",
                    Schema::new(vec![Column::not_null("on", DataType::SmallInt)]).unwrap(),
                )
                .unwrap();
            catalog.tables().to_vec()
        };
        pool.reopen(4);

        let catalog = Catalog::open(pool.bpm()).unwrap();

        assert_eq!(catalog.tables(), created);
    }

    #[test]
    fn ids_should_continue_after_reopening() {
        let mut pool = TestPool::new(4);
        {
            let mut catalog = Catalog::open(pool.bpm()).unwrap();
            catalog.create_table(pool.bpm(), "a", accounts()).unwrap();
            catalog.create_table(pool.bpm(), "b", accounts()).unwrap();
        }
        pool.reopen(4);

        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        let next = catalog.create_table(pool.bpm(), "c", accounts()).unwrap();

        assert_eq!(next.id, TableId(3));
    }

    #[test]
    fn many_columns_spread_over_many_pages_should_survive_eviction_and_reopening() {
        let mut pool = TestPool::new(3);
        let created = {
            let mut catalog = Catalog::open(pool.bpm()).unwrap();
            for i in 0..20 {
                catalog
                    .create_table(pool.bpm(), &format!("t{i}"), smallints(50))
                    .unwrap();
            }
            assert!(catalog.columns_heap.page_count() > 10);
            catalog.tables().to_vec()
        };
        pool.reopen(3);

        let catalog = Catalog::open(pool.bpm()).unwrap();

        assert_eq!(catalog.tables(), created);
    }

    fn column_entry(table_id: u32, position: u16, name: &str) -> Vec<u8> {
        ColumnRecord {
            table_id: TableId(table_id),
            position,
            column: Column::new(name, DataType::Integer),
        }
        .encode()
        .unwrap()
    }

    fn table_entry(table_id: u32, name: &str) -> Vec<u8> {
        TableRecord {
            table_id: TableId(table_id),
            first_page_id: 1_000,
            name: name.to_string(),
        }
        .encode()
        .unwrap()
    }

    /// A database with table "accounts" (id 1), plus raw entries written behind the catalog's
    /// back, as a crash or a bug would leave them. Returns the catalog reopened.
    fn reopen_with(
        pool: &mut TestPool,
        tables: &[Vec<u8>],
        columns: &[Vec<u8>],
    ) -> Result<Catalog, CatalogError> {
        {
            let mut catalog = Catalog::open(pool.bpm()).unwrap();
            catalog
                .create_table(pool.bpm(), "accounts", accounts())
                .unwrap();
            for entry in tables {
                catalog.tables_heap.insert(pool.bpm(), entry).unwrap();
            }
            for entry in columns {
                catalog.columns_heap.insert(pool.bpm(), entry).unwrap();
            }
        }
        pool.reopen(4);
        Catalog::open(pool.bpm())
    }

    #[test]
    fn orphan_columns_should_be_ignored_and_their_id_never_reused() {
        let mut pool = TestPool::new(4);

        let mut catalog = reopen_with(&mut pool, &[], &[column_entry(7, 0, "x")]).unwrap();
        let next = catalog
            .create_table(pool.bpm(), "next", accounts())
            .unwrap()
            .id;

        assert_eq!(catalog.tables().len(), 2);
        assert_eq!(next, TableId(8));
    }

    #[test]
    fn columns_not_numbered_from_zero_without_gaps_should_be_corrupted() {
        for extra in [column_entry(1, 4, "gap"), column_entry(1, 0, "again")] {
            let mut pool = TestPool::new(4);

            let err = reopen_with(&mut pool, &[], &[extra]).unwrap_err();

            assert!(matches!(
                err,
                CatalogError::Corrupted {
                    reason: "columns of a table are not numbered 0..n"
                }
            ));
        }
    }

    #[test]
    fn a_table_whose_columns_do_not_form_a_schema_should_be_corrupted() {
        let mut pool = TestPool::new(4);
        let columns = [column_entry(9, 0, "dup"), column_entry(9, 1, "dup")];

        let err = reopen_with(&mut pool, &[table_entry(9, "broken")], &columns).unwrap_err();

        assert!(matches!(
            err,
            CatalogError::CorruptedSchema { table, source: SchemaError::DuplicateColumn { .. } }
                if table == "broken"
        ));
    }

    #[test]
    fn a_table_without_columns_should_be_corrupted() {
        let mut pool = TestPool::new(4);

        let err = reopen_with(&mut pool, &[table_entry(9, "empty")], &[]).unwrap_err();

        assert!(matches!(
            err,
            CatalogError::CorruptedSchema {
                source: SchemaError::NoColumns,
                ..
            }
        ));
    }

    #[test]
    fn two_tables_with_the_same_name_or_id_should_be_corrupted() {
        let cases = [
            (table_entry(9, "accounts"), "two tables have the same name"),
            (table_entry(1, "other"), "two tables have the same id"),
        ];

        for (entry, reason) in cases {
            let mut pool = TestPool::new(4);
            let columns = [column_entry(9, 0, "x")];

            let err = reopen_with(&mut pool, &[entry], &columns).unwrap_err();

            assert!(
                matches!(err, CatalogError::Corrupted { reason: r } if r == reason),
                "{reason}"
            );
        }
    }

    #[test]
    fn a_corrupted_entry_should_fail_the_open() {
        let mut pool = TestPool::new(4);
        let mut garbage = column_entry(1, 9, "x");
        garbage[6] = 0;

        let err = reopen_with(&mut pool, &[], &[garbage]).unwrap_err();

        assert!(matches!(
            err,
            CatalogError::Corrupted {
                reason: "unknown column type"
            }
        ));
    }

    fn assert_no_table_after_failure(
        pool: &mut TestPool,
        catalog: &mut Catalog,
        err: CatalogError,
    ) {
        assert!(matches!(
            err,
            CatalogError::Heap(HeapFileError::BufferPool(BufferPoolError::NoAvailableFrame))
        ));
        assert!(catalog.tables().is_empty());
        assert!(catalog.open_tables.is_empty());
        assert_eq!(catalog.columns_heap.scan(pool.bpm()).count(), 0);

        pool.reopen(4);
        let mut reopened = Catalog::open(pool.bpm()).unwrap();
        assert!(reopened.tables().is_empty());
        let retried = reopened
            .create_table(pool.bpm(), "accounts", accounts())
            .unwrap();
        assert_eq!(retried.id, TableId(1));
    }

    #[test]
    fn create_table_failing_on_the_table_entry_should_leave_no_table() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        let tables_page = catalog.tables_heap.first_page_id();
        pool.bpm().fail_fetch_after(tables_page, 0);

        let err = catalog
            .create_table(pool.bpm(), "accounts", accounts())
            .unwrap_err();

        assert_no_table_after_failure(&mut pool, &mut catalog, err);
    }

    #[test]
    fn create_table_failing_between_column_entries_should_leave_no_table() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        let columns_page = catalog.columns_heap.first_page_id();
        pool.bpm().fail_fetch_after(columns_page, 1);

        let err = catalog
            .create_table(pool.bpm(), "accounts", accounts())
            .unwrap_err();

        assert_no_table_after_failure(&mut pool, &mut catalog, err);
    }

    /// The rollback is best effort: when it fails too, a column entry stays behind with the
    /// failed table's id. Reusing that id would give the next table an extra column at
    /// position 0, and the whole database would stop opening.
    #[test]
    fn create_table_whose_rollback_fails_should_not_reuse_the_id() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        let tables_page = catalog.tables_heap.first_page_id();
        let columns_page = catalog.columns_heap.first_page_id();
        // `accounts` has 3 columns: 3 fetches write them, the 4th is the rollback's first delete.
        pool.bpm().fail_fetch_after(columns_page, 3);
        pool.bpm().fail_fetch_after(tables_page, 0);

        catalog
            .create_table(pool.bpm(), "failed", accounts())
            .unwrap_err();
        let orphans = catalog.columns_heap.scan(pool.bpm()).count();
        let next = catalog
            .create_table(pool.bpm(), "next", accounts())
            .unwrap()
            .id;
        pool.reopen(4);
        let reopened = Catalog::open(pool.bpm());

        assert_eq!(orphans, 1, "the scenario needs one column left behind");
        assert_ne!(next, TableId(1));
        let reopened = reopened.unwrap();
        assert_eq!(reopened.table("next").unwrap().id, next);
        assert_eq!(reopened.table("next").unwrap().schema, accounts());
    }

    #[test]
    fn interrupted_database_creation_should_start_over_on_the_next_open() {
        let mut pool = TestPool::new(4);
        // The first fetch of page 0 reads the header; the second one would write it.
        pool.bpm().fail_fetch_after(RESERVED_PAGE_ID, 1);

        let err = Catalog::open(pool.bpm()).unwrap_err();
        let header_after_failure = stored_header(pool.bpm());
        pool.reopen(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();

        assert!(matches!(
            err,
            CatalogError::BufferPool(BufferPoolError::NoAvailableFrame)
        ));
        assert_eq!(header_after_failure, None);
        assert!(catalog.tables().is_empty());
        assert!(stored_header(pool.bpm()).is_some());
        assert!(
            catalog
                .create_table(pool.bpm(), "accounts", accounts())
                .is_ok()
        );
    }

    #[test]
    fn a_stored_maximum_id_should_still_open_and_refuse_new_tables() {
        let mut pool = TestPool::new(4);
        let last = u32::MAX;

        let mut catalog = reopen_with(
            &mut pool,
            &[table_entry(last, "last")],
            &[column_entry(last, 0, "x")],
        )
        .unwrap();
        let err = catalog
            .create_table(pool.bpm(), "more", accounts())
            .unwrap_err();

        assert_eq!(catalog.table("last").unwrap().id, TableId(last));
        assert!(matches!(err, CatalogError::TooManyTables));
        assert_eq!(catalog.tables().len(), 2);
    }

    #[test]
    fn create_table_after_the_last_id_should_fail_without_changes() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        catalog.next_table_id = u64::from(u32::MAX);
        catalog
            .create_table(pool.bpm(), "last", accounts())
            .unwrap();

        let err = catalog
            .create_table(pool.bpm(), "one_more", accounts())
            .unwrap_err();

        assert!(matches!(err, CatalogError::TooManyTables));
        assert_eq!(catalog.tables().len(), 1);
        assert_eq!(catalog.table("last").unwrap().id, TableId(u32::MAX));
    }

    fn account(id: i32) -> Vec<Value> {
        vec![
            Value::Integer(id),
            Value::Null,
            Value::BigInt(i64::from(id) * 10),
        ]
    }

    #[test]
    fn table_heap_should_return_the_same_handle_on_every_call() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        catalog
            .create_table(pool.bpm(), "accounts", accounts())
            .unwrap();

        let first: *const TableHeap = catalog.table_heap(pool.bpm(), "accounts").unwrap();
        let second: *const TableHeap = catalog.table_heap(pool.bpm(), "accounts").unwrap();

        assert!(std::ptr::eq(first, second));
    }

    #[test]
    fn table_heap_should_read_and_write_the_rows_of_the_table() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        let table = catalog
            .create_table(pool.bpm(), "accounts", accounts())
            .unwrap()
            .clone();

        let heap = catalog.table_heap(pool.bpm(), "accounts").unwrap();
        let rid = heap.insert(pool.bpm(), &account(7)).unwrap();

        assert_eq!(heap.first_page_id(), table.first_page_id);
        assert_eq!(heap.schema(), &table.schema);
        let stored = HeapFile::open(pool.bpm(), table.first_page_id)
            .unwrap()
            .get(pool.bpm(), rid)
            .unwrap();
        assert_eq!(table.schema.decode_row(&stored).unwrap(), account(7));
    }

    #[test]
    fn create_table_should_keep_the_heap_it_created_open() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();

        let id = catalog
            .create_table(pool.bpm(), "accounts", accounts())
            .unwrap()
            .id;

        assert!(catalog.open_tables.contains_key(&id));
    }

    #[test]
    fn table_heap_of_an_unknown_table_should_fail() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        catalog
            .create_table(pool.bpm(), "accounts", accounts())
            .unwrap();

        let err = catalog.table_heap(pool.bpm(), "Accounts").unwrap_err();

        assert!(matches!(err, CatalogError::UndefinedTable { ref name } if name == "Accounts"));
        assert_eq!(err.sqlstate(), "42P01");
    }

    #[test]
    fn table_heap_should_open_a_table_on_first_use_after_reopening() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        catalog
            .create_table(pool.bpm(), "accounts", accounts())
            .unwrap();
        catalog
            .create_table(pool.bpm(), "empty", accounts())
            .unwrap();
        let rids: Vec<Rid> = (0..40)
            .map(|id| {
                let heap = catalog.table_heap(pool.bpm(), "accounts").unwrap();
                heap.insert(pool.bpm(), &account(id)).unwrap()
            })
            .collect();
        pool.reopen(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        assert!(catalog.open_tables.is_empty());

        let heap = catalog.table_heap(pool.bpm(), "accounts").unwrap();
        let rows: Vec<(Rid, Vec<Value>)> = heap.scan(pool.bpm()).map(|row| row.unwrap()).collect();

        let expected: Vec<(Rid, Vec<Value>)> = rids.into_iter().zip((0..40).map(account)).collect();
        assert_eq!(rows, expected);
        assert_eq!(catalog.open_tables.len(), 1);
    }

    #[test]
    fn table_heap_should_not_keep_a_table_that_failed_to_open() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        let first_page_id = catalog
            .create_table(pool.bpm(), "accounts", accounts())
            .unwrap()
            .first_page_id;
        pool.reopen(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        pool.bpm().fail_fetch_after(first_page_id, 0);

        let err = catalog.table_heap(pool.bpm(), "accounts").unwrap_err();

        assert!(
            matches!(
                err,
                CatalogError::Table(TableError::Heap(HeapFileError::BufferPool(
                    BufferPoolError::NoAvailableFrame
                )))
            ),
            "{err:?}"
        );
        assert!(catalog.open_tables.is_empty());
        let retried = catalog.table_heap(pool.bpm(), "accounts").unwrap();
        assert_eq!(retried.first_page_id(), first_page_id);
    }

    #[test]
    fn create_table_failing_to_create_the_data_heap_should_leave_no_table() {
        let mut pool = TestPool::new(3);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        // Every frame pinned: the data heap's first page gets none.
        let pinned = [
            RESERVED_PAGE_ID,
            catalog.tables_heap.first_page_id(),
            catalog.columns_heap.first_page_id(),
        ];
        for page_id in pinned {
            pool.bpm().fetch_page(page_id).unwrap();
        }

        let err = catalog
            .create_table(pool.bpm(), "accounts", accounts())
            .unwrap_err();

        for page_id in pinned {
            pool.bpm().unpin_page(page_id, false);
        }
        assert!(
            matches!(
                err,
                CatalogError::Table(TableError::Heap(HeapFileError::BufferPool(
                    BufferPoolError::NoAvailableFrame
                )))
            ),
            "{err:?}"
        );
        assert_eq!(err.sqlstate(), "53000");
        assert!(catalog.tables().is_empty());
        assert!(catalog.open_tables.is_empty());
        assert_eq!(catalog.columns_heap.scan(pool.bpm()).count(), 0);
        let retried = catalog
            .create_table(pool.bpm(), "accounts", accounts())
            .unwrap();
        assert_eq!(retried.id, TableId(2), "the failed attempt consumed id 1");
    }

    #[test]
    fn open_of_a_file_whose_page_0_is_not_a_header_should_fail() {
        let mut pool = TestPool::new(4);
        let page = pool.bpm().fetch_page(RESERVED_PAGE_ID).unwrap();
        SlottedPage::init(page.data_mut()).unwrap();
        pool.bpm().unpin_page(RESERVED_PAGE_ID, true);

        let err = Catalog::open(pool.bpm()).unwrap_err();

        assert!(matches!(err, CatalogError::NotADatabase { .. }));
    }

    fn every_error() -> Vec<(CatalogError, String, &'static str, bool)> {
        vec![
            (
                CatalogError::NotADatabase {
                    reason: "wrong magic number",
                },
                "not a ferroDB database: wrong magic number".to_string(),
                "XX001",
                false,
            ),
            (
                CatalogError::EmptyTableName,
                "a table name cannot be empty".to_string(),
                "42602",
                false,
            ),
            (
                CatalogError::TableNameTooLong {
                    name: "accounts".to_string(),
                    max: 3,
                },
                "table name \"acc…\" is longer than 3 bytes".to_string(),
                "42622",
                false,
            ),
            (
                CatalogError::DuplicateTable {
                    name: "users".to_string(),
                },
                "table \"users\" already exists".to_string(),
                "42P07",
                false,
            ),
            (
                CatalogError::UndefinedTable {
                    name: "users".to_string(),
                },
                "table \"users\" does not exist".to_string(),
                "42P01",
                false,
            ),
            (
                CatalogError::RowTooWide {
                    table: "wide".to_string(),
                    size: 9000,
                    max: 8164,
                },
                "rows of table \"wide\" need 9000 bytes, more than the 8164 that fit a page"
                    .to_string(),
                "54000",
                false,
            ),
            (
                CatalogError::TooManyTables,
                "no table ids left".to_string(),
                "54000",
                false,
            ),
            (
                CatalogError::Corrupted {
                    reason: "unknown column type",
                },
                "catalog is corrupted: unknown column type".to_string(),
                "XX001",
                false,
            ),
            (
                CatalogError::CorruptedSchema {
                    table: "t".to_string(),
                    source: SchemaError::NoColumns,
                },
                "catalog is corrupted: table \"t\": a table must have at least one column"
                    .to_string(),
                "XX001",
                true,
            ),
            (
                CatalogError::Heap(HeapFileError::PageNotInHeap { page_id: 3 }),
                "page 3 does not belong to this heap file".to_string(),
                "XX000",
                true,
            ),
            (
                CatalogError::BufferPool(BufferPoolError::NoAvailableFrame),
                "no available frame".to_string(),
                "53000",
                true,
            ),
            (
                CatalogError::Heap(HeapFileError::BufferPool(
                    BufferPoolError::ChecksumMismatch { page_id: 3 },
                )),
                "checksum mismatch on page 3".to_string(),
                "XX001",
                true,
            ),
            (
                CatalogError::Heap(HeapFileError::TupleTooLarge {
                    size: 9000,
                    max: 8164,
                }),
                "tuple of 9000 bytes exceeds maximum of 8164 bytes".to_string(),
                "54000",
                true,
            ),
            (
                CatalogError::Table(TableError::Schema(SchemaError::NullViolation {
                    column: "id".to_string(),
                })),
                "null value in column \"id\" violates not-null constraint".to_string(),
                "23502",
                true,
            ),
        ]
    }

    #[test]
    fn errors_should_describe_the_problem() {
        for (err, message, _, _) in every_error() {
            assert_eq!(err.to_string(), message);
        }
    }

    #[test]
    fn errors_should_map_to_sqlstates() {
        for (err, _, sqlstate, _) in every_error() {
            assert_eq!(err.sqlstate(), sqlstate, "{err:?}");
        }
    }

    #[test]
    fn wrapped_errors_should_expose_their_source() {
        for (err, _, _, has_source) in every_error() {
            assert_eq!(
                std::error::Error::source(&err).is_some(),
                has_source,
                "{err:?}"
            );
        }
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }

        /// Mostly from a small pool, so duplicates are frequent; sometimes invalid.
        fn table_name(&mut self) -> String {
            match self.below(20) {
                0 => String::new(),
                1 => "x".repeat(Catalog::MAX_TABLE_NAME_LEN + 1),
                _ => format!("t{}", self.below(40)),
            }
        }

        /// Every type, with VARCHARs of any length including the limits 1 and 65535.
        fn data_type(&mut self) -> DataType {
            match self.below(10) {
                0 => DataType::SmallInt,
                1 => DataType::Integer,
                2 => DataType::BigInt,
                3 => DataType::Boolean,
                4 => DataType::Real,
                5 => DataType::Double,
                6 => DataType::Date,
                7 => DataType::Timestamp,
                _ => {
                    let max_chars = [1, u16::MAX, self.next() as u16][self.below(3)];
                    DataType::Varchar(NonZeroU16::new(max_chars).unwrap_or(NonZeroU16::MIN))
                }
            }
        }

        fn schema(&mut self) -> Schema {
            let columns = (0..1 + self.below(30))
                .map(|i| Column {
                    name: format!("c{i}"),
                    data_type: self.data_type(),
                    nullable: self.below(2) == 0,
                })
                .collect();
            Schema::new(columns).unwrap()
        }
    }

    fn assert_matches_model(catalog: &Catalog, model: &[(String, Schema)]) {
        let actual: Vec<(TableId, &str, &Schema)> = catalog
            .tables()
            .iter()
            .map(|table| (table.id, table.name.as_str(), &table.schema))
            .collect();
        let expected: Vec<(TableId, &str, &Schema)> = model
            .iter()
            .enumerate()
            .map(|(index, (name, schema))| (TableId(index as u32 + 1), name.as_str(), schema))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn random_creations_should_match_model_across_reopens() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut pool = TestPool::new(3);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        let mut model: Vec<(String, Schema)> = Vec::new();

        for step in 1..=300 {
            let name = rng.table_name();
            let schema = rng.schema();
            let is_valid = !name.is_empty() && name.len() <= Catalog::MAX_TABLE_NAME_LEN;
            let is_new = model.iter().all(|(existing, _)| *existing != name);

            let created = catalog
                .create_table(pool.bpm(), &name, schema.clone())
                .map(|table| table.id);

            assert_eq!(created.is_ok(), is_valid && is_new, "step {step}: {name:?}");
            if let Ok(id) = created {
                model.push((name, schema));
                assert_eq!(id, TableId(model.len() as u32));
            }
            if step % 50 == 0 {
                pool.reopen(3);
                catalog = Catalog::open(pool.bpm()).unwrap();
                assert_matches_model(&catalog, &model);
            }
        }
    }
}
