use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use crate::index::BPlusTree;
use crate::storage::buffer_pool::BufferPoolManager;
use crate::storage::heap_file::HeapFile;
use crate::storage::page::RESERVED_PAGE_ID;
use crate::storage::rid::Rid;
use crate::storage::slotted_page::max_tuple_size_for;
use crate::table::{Column, Schema, TableHeap};
use crate::types::Value;

use super::records::{ColumnRecord, IndexRecord, TableRecord};
use super::{CatalogError, DatabaseHeader};

const FIRST_TABLE_ID: u64 = 1;
const FIRST_INDEX_ID: u64 = 1;

/// Identifies a table in the catalog; a newtype so it is never mixed up with a page id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableId(pub u32);

/// Identifies an index in the catalog; its own space of ids, apart from tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IndexId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    pub id: TableId,
    pub name: String,
    pub schema: Schema,
    /// First page of the table's data; `HeapFile::open` reads the rows from there.
    pub first_page_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfo {
    pub id: IndexId,
    pub name: String,
    pub table_id: TableId,
    /// Position of the key column in the table's schema.
    pub column: u16,
    /// The tree's header page: `BPlusTree::open` finds the tree from there.
    pub header_page_id: u32,
}

#[derive(Debug)]
pub struct Catalog {
    tables_heap: HeapFile,
    columns_heap: HeapFile,
    /// `None` until the first index is created.
    indexes_heap: Option<HeapFile>,
    tables: Vec<TableInfo>,
    indexes: Vec<IndexInfo>,
    /// Each table is opened once, on first use, and the same handle is handed out after that.
    open_tables: HashMap<TableId, TableHeap>,
    /// Same for the trees of the indexes.
    open_indexes: HashMap<IndexId, BPlusTree>,
    /// `u64` so a stored id of `u32::MAX` still loads; creating the next table then fails.
    next_table_id: u64,
    next_index_id: u64,
}

impl Catalog {
    pub const MAX_TABLE_NAME_LEN: usize = Schema::MAX_COLUMN_NAME_LEN;
    pub const MAX_INDEX_NAME_LEN: usize = Schema::MAX_COLUMN_NAME_LEN;

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
                indexes_first_page_id: 0,
            },
        )?;
        Ok(Self {
            tables_heap,
            columns_heap,
            indexes_heap: None,
            tables: Vec::new(),
            indexes: Vec::new(),
            open_tables: HashMap::new(),
            open_indexes: HashMap::new(),
            next_table_id: FIRST_TABLE_ID,
            next_index_id: FIRST_INDEX_ID,
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
        let tables = assemble(table_records, column_records)?;
        let indexes_heap = match header.indexes_first_page_id {
            RESERVED_PAGE_ID => None,
            first_page_id => Some(HeapFile::open(bpm, first_page_id)?),
        };
        let index_records = match &indexes_heap {
            Some(heap) => decode_all(heap, bpm, IndexRecord::decode)?,
            None => Vec::new(),
        };
        let indexes = assemble_indexes(index_records, &tables)?;
        let next_index_id = indexes
            .last()
            .map_or(FIRST_INDEX_ID, |index| u64::from(index.id.0) + 1);

        Ok(Self {
            tables_heap,
            columns_heap,
            indexes_heap,
            tables,
            indexes,
            open_tables: HashMap::new(),
            open_indexes: HashMap::new(),
            next_table_id,
            next_index_id,
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
        if self.index(name).is_ok() {
            return Err(CatalogError::DuplicateIndex {
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

    /// A unique index on one column, filled with the table's rows (`NULL` keys are skipped).
    /// Everything is validated before any page is touched; the entry is written last.
    pub fn create_index(
        &mut self,
        bpm: &mut BufferPoolManager,
        name: &str,
        table: &str,
        column: &str,
    ) -> Result<&IndexInfo, CatalogError> {
        self.check_new_index_name(name)?;
        let table_info = find_table(&self.tables, table)?;
        let (column_position, key_type) = table_info
            .schema
            .columns()
            .iter()
            .enumerate()
            .find(|(_, candidate)| candidate.name == column)
            .map(|(position, candidate)| (position, candidate.data_type))
            .ok_or_else(|| CatalogError::UndefinedColumn {
                table: table.to_string(),
                column: column.to_string(),
            })?;
        let table_id = table_info.id;
        let column = u16::try_from(column_position).map_err(|_| CatalogError::Corrupted {
            reason: "column position does not fit u16",
        })?;
        let id =
            IndexId(u32::try_from(self.next_index_id).map_err(|_| CatalogError::TooManyIndexes)?);
        // Consumed before anything is written, like a table id.
        self.next_index_id += 1;

        let keys = self.non_null_keys(bpm, table, column_position)?;
        let mut tree = BPlusTree::create(bpm, key_type)?;
        for (key, rid) in keys {
            tree.insert(bpm, &key, rid)?;
        }
        let index = IndexInfo {
            id,
            name: name.to_string(),
            table_id,
            column,
            header_page_id: tree.header_page_id(),
        };
        self.persist_index(bpm, &index)?;

        self.open_indexes.insert(id, tree);
        let position = self.indexes.len();
        self.indexes.push(index);
        Ok(&self.indexes[position])
    }

    /// The key of every row whose key is not `NULL`, with its `Rid`. Read whole before the tree
    /// is filled: the scan holds the buffer pool.
    fn non_null_keys(
        &mut self,
        bpm: &mut BufferPoolManager,
        table: &str,
        column: usize,
    ) -> Result<Vec<(Value, Rid)>, CatalogError> {
        let heap = self.table_heap(bpm, table)?;
        let mut keys = Vec::new();
        for entry in heap.scan(bpm) {
            let (rid, row) = entry?;
            match row.into_iter().nth(column) {
                Some(Value::Null) => {}
                Some(key) => keys.push((key, rid)),
                None => {
                    return Err(CatalogError::Corrupted {
                        reason: "a row has fewer values than its table's columns",
                    });
                }
            }
        }
        Ok(keys)
    }

    /// Creates the indexes heap on the first index (pointing the header to it first), then
    /// writes the entry: it is what makes the index exist on the next `open`.
    fn persist_index(
        &mut self,
        bpm: &mut BufferPoolManager,
        index: &IndexInfo,
    ) -> Result<(), CatalogError> {
        let record = IndexRecord {
            index_id: index.id,
            table_id: index.table_id,
            column: index.column,
            header_page_id: index.header_page_id,
            name: index.name.clone(),
        }
        .encode()?;
        let heap = match &mut self.indexes_heap {
            Some(heap) => heap,
            None => {
                let heap = HeapFile::create(bpm)?;
                write_header(
                    bpm,
                    DatabaseHeader {
                        tables_first_page_id: self.tables_heap.first_page_id(),
                        columns_first_page_id: self.columns_heap.first_page_id(),
                        indexes_first_page_id: heap.first_page_id(),
                    },
                )?;
                self.indexes_heap.insert(heap)
            }
        };
        heap.insert(bpm, &record)?;
        Ok(())
    }

    /// Tables and indexes share one namespace, as relations do in PostgreSQL.
    fn check_new_index_name(&self, name: &str) -> Result<(), CatalogError> {
        if name.is_empty() {
            return Err(CatalogError::EmptyIndexName);
        }
        if name.len() > Catalog::MAX_INDEX_NAME_LEN {
            return Err(CatalogError::IndexNameTooLong {
                name: name.to_string(),
                max: Catalog::MAX_INDEX_NAME_LEN,
            });
        }
        if self.index(name).is_ok() {
            return Err(CatalogError::DuplicateIndex {
                name: name.to_string(),
            });
        }
        if self.table(name).is_ok() {
            return Err(CatalogError::DuplicateTable {
                name: name.to_string(),
            });
        }
        Ok(())
    }

    /// In creation order.
    pub fn indexes(&self) -> &[IndexInfo] {
        &self.indexes
    }

    pub fn index(&self, name: &str) -> Result<&IndexInfo, CatalogError> {
        find_index(&self.indexes, name)
    }

    /// The indexes of a table, in creation order: the ones a change to the table must update.
    pub fn table_indexes(&self, table: &str) -> Result<Vec<&IndexInfo>, CatalogError> {
        let table_id = self.table(table)?.id;
        Ok(self
            .indexes
            .iter()
            .filter(|index| index.table_id == table_id)
            .collect())
    }

    /// The index's tree, opened on first use; every later call returns the same handle.
    pub fn index_tree(
        &mut self,
        bpm: &mut BufferPoolManager,
        name: &str,
    ) -> Result<&mut BPlusTree, CatalogError> {
        let index = find_index(&self.indexes, name)?;
        let tree = match self.open_indexes.entry(index.id) {
            Entry::Occupied(open) => open.into_mut(),
            Entry::Vacant(closed) => {
                let key_type = self
                    .tables
                    .iter()
                    .find(|table| table.id == index.table_id)
                    .and_then(|table| table.schema.columns().get(usize::from(index.column)))
                    .map(|column| column.data_type)
                    .ok_or(CatalogError::Corrupted {
                        reason: "an index points to a column that does not exist",
                    })?;
                closed.insert(BPlusTree::open(bpm, key_type, index.header_page_id)?)
            }
        };
        Ok(tree)
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

fn find_index<'a>(indexes: &'a [IndexInfo], name: &str) -> Result<&'a IndexInfo, CatalogError> {
    indexes
        .iter()
        .find(|index| index.name == name)
        .ok_or_else(|| CatalogError::UndefinedIndex {
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

/// The index entries in id order (creation order), checked against the tables: each index
/// belongs to a table and a column that exist, and no name is used by two relations.
fn assemble_indexes(
    mut records: Vec<IndexRecord>,
    tables: &[TableInfo],
) -> Result<Vec<IndexInfo>, CatalogError> {
    records.sort_by_key(|record| record.index_id);
    let mut names: HashSet<&str> = tables.iter().map(|table| table.name.as_str()).collect();
    let mut indexes: Vec<IndexInfo> = Vec::with_capacity(records.len());
    for record in &records {
        if indexes
            .last()
            .is_some_and(|last| last.id == record.index_id)
        {
            return Err(CatalogError::Corrupted {
                reason: "two indexes have the same id",
            });
        }
        let table = tables
            .iter()
            .find(|table| table.id == record.table_id)
            .ok_or(CatalogError::Corrupted {
                reason: "an index belongs to a table that does not exist",
            })?;
        if usize::from(record.column) >= table.schema.columns().len() {
            return Err(CatalogError::Corrupted {
                reason: "an index points to a column that does not exist",
            });
        }
        if !names.insert(record.name.as_str()) {
            return Err(CatalogError::Corrupted {
                reason: "two relations have the same name",
            });
        }
        indexes.push(IndexInfo {
            id: record.index_id,
            name: record.name.clone(),
            table_id: record.table_id,
            column: record.column,
            header_page_id: record.header_page_id,
        });
    }
    Ok(indexes)
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
    use super::super::records::IndexRecord;
    use super::*;
    use crate::index::IndexError;
    use crate::storage::buffer_pool::BufferPoolError;
    use crate::storage::heap_file::HeapFileError;
    use crate::storage::rid::Rid;
    use crate::storage::slotted_page::SlottedPage;
    use crate::table::{Column, SchemaError, TableError};
    use crate::types::{DataType, Value};
    use std::fs;
    use std::num::NonZeroU16;
    use std::ops::Bound;
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
    fn create_index_should_validate_everything_first() {
        let mut pool = TestPool::new(4);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        catalog
            .create_table(pool.bpm(), "accounts", accounts())
            .unwrap();
        let too_long = "x".repeat(Catalog::MAX_INDEX_NAME_LEN + 1);
        let cases = [
            ("", "accounts", "id", "42602"),
            (too_long.as_str(), "accounts", "id", "42622"),
            ("accounts", "accounts", "id", "42P07"),
            ("by_id", "missing", "id", "42P01"),
            ("by_id", "accounts", "ID", "42703"),
        ];

        for (name, table, column, sqlstate) in cases {
            let err = catalog
                .create_index(pool.bpm(), name, table, column)
                .unwrap_err();

            assert_eq!(err.sqlstate(), sqlstate, "{err:?}");
        }
        let err = catalog
            .create_index(pool.bpm(), "by_age", "accounts", "Age")
            .unwrap_err();
        assert!(matches!(
            err,
            CatalogError::UndefinedColumn { ref table, ref column } if table == "accounts" && column == "Age"
        ));
        assert!(catalog.indexes().is_empty());
    }

    /// `accounts` with `count` rows: ids 0.., ages unique except every fifth `NULL`, and a
    /// balance that repeats (so it cannot have a unique index).
    fn accounts_with_rows(pool: &mut TestPool, catalog: &mut Catalog, count: i32) -> Vec<Rid> {
        catalog
            .create_table(pool.bpm(), "accounts", accounts())
            .unwrap();
        (0..count)
            .map(|id| {
                let age = if id % 5 == 0 {
                    Value::Null
                } else {
                    Value::SmallInt(id as i16)
                };
                let row = [Value::Integer(id), age, Value::BigInt(i64::from(id % 7))];
                let heap = catalog.table_heap(pool.bpm(), "accounts").unwrap();
                heap.insert(pool.bpm(), &row).unwrap()
            })
            .collect()
    }

    fn index_keys(pool: &mut TestPool, catalog: &mut Catalog, name: &str) -> Vec<(Value, Rid)> {
        let tree = catalog.index_tree(pool.bpm(), name).unwrap();
        tree.range(pool.bpm(), Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect()
    }

    #[test]
    fn create_index_should_fill_the_index_with_the_rows_of_the_table() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        let rids = accounts_with_rows(&mut pool, &mut catalog, 300);

        let index = catalog
            .create_index(pool.bpm(), "accounts_id", "accounts", "id")
            .unwrap()
            .clone();

        assert_eq!(index.id, IndexId(1));
        assert_eq!(index.name, "accounts_id");
        assert_eq!(index.table_id, catalog.table("accounts").unwrap().id);
        assert_eq!(index.column, 0);
        assert_eq!(catalog.indexes(), [index]);
        let expected: Vec<(Value, Rid)> = (0..300)
            .map(|id| (Value::Integer(id), rids[id as usize]))
            .collect();
        assert_eq!(index_keys(&mut pool, &mut catalog, "accounts_id"), expected);
        let rid = catalog
            .index_tree(pool.bpm(), "accounts_id")
            .unwrap()
            .get(pool.bpm(), &Value::Integer(123))
            .unwrap()
            .unwrap();
        let row = catalog
            .table_heap(pool.bpm(), "accounts")
            .unwrap()
            .get(pool.bpm(), rid)
            .unwrap();
        assert_eq!(row[0], Value::Integer(123));
    }

    #[test]
    fn create_index_should_leave_null_keys_out() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        accounts_with_rows(&mut pool, &mut catalog, 50);

        catalog
            .create_index(pool.bpm(), "accounts_age", "accounts", "age")
            .unwrap();

        let keys: Vec<Value> = index_keys(&mut pool, &mut catalog, "accounts_age")
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        let expected: Vec<Value> = (0..50)
            .filter(|id| id % 5 != 0)
            .map(|id| Value::SmallInt(id as i16))
            .collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn create_index_on_a_column_with_repeated_values_should_leave_no_index() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        accounts_with_rows(&mut pool, &mut catalog, 50);

        let err = catalog
            .create_index(pool.bpm(), "accounts_balance", "accounts", "balance")
            .unwrap_err();

        assert!(
            matches!(err, CatalogError::Index(IndexError::DuplicateKey { .. })),
            "{err:?}"
        );
        assert_eq!(err.sqlstate(), "23505");
        assert!(catalog.indexes().is_empty());
        assert!(matches!(
            catalog.index("accounts_balance"),
            Err(CatalogError::UndefinedIndex { .. })
        ));
        let next = catalog
            .create_index(pool.bpm(), "accounts_balance", "accounts", "id")
            .unwrap();
        assert_eq!(next.id, IndexId(2), "the failed attempt consumed id 1");
    }

    #[test]
    fn create_index_on_an_empty_table_should_make_an_empty_index() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        catalog
            .create_table(pool.bpm(), "accounts", accounts())
            .unwrap();

        catalog
            .create_index(pool.bpm(), "accounts_id", "accounts", "id")
            .unwrap();

        assert!(index_keys(&mut pool, &mut catalog, "accounts_id").is_empty());
    }

    #[test]
    fn indexes_should_be_found_by_name_and_by_table() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        accounts_with_rows(&mut pool, &mut catalog, 10);
        catalog
            .create_table(pool.bpm(), "empty", accounts())
            .unwrap();
        for (name, table, column) in [
            ("a_id", "accounts", "id"),
            ("e_id", "empty", "id"),
            ("a_age", "accounts", "age"),
        ] {
            catalog
                .create_index(pool.bpm(), name, table, column)
                .unwrap();
        }

        let by_table: Vec<&str> = catalog
            .table_indexes("accounts")
            .unwrap()
            .iter()
            .map(|index| index.name.as_str())
            .collect();

        assert_eq!(by_table, ["a_id", "a_age"]);
        assert_eq!(catalog.index("e_id").unwrap().column, 0);
        assert_eq!(catalog.index("a_age").unwrap().column, 1);
        let missing = catalog.index("A_ID").unwrap_err();
        assert!(matches!(missing, CatalogError::UndefinedIndex { ref name } if name == "A_ID"));
        assert_eq!(missing.sqlstate(), "42704");
        assert!(matches!(
            catalog.table_indexes("missing"),
            Err(CatalogError::UndefinedTable { .. })
        ));
    }

    #[test]
    fn index_tree_should_return_the_same_handle_with_the_column_type() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        accounts_with_rows(&mut pool, &mut catalog, 10);
        catalog
            .create_index(pool.bpm(), "accounts_age", "accounts", "age")
            .unwrap();

        let first: *const BPlusTree = catalog.index_tree(pool.bpm(), "accounts_age").unwrap();
        let second: *const BPlusTree = catalog.index_tree(pool.bpm(), "accounts_age").unwrap();
        let missing = catalog.index_tree(pool.bpm(), "nope").unwrap_err();

        assert!(std::ptr::eq(first, second));
        assert_eq!(
            catalog
                .index_tree(pool.bpm(), "accounts_age")
                .unwrap()
                .key_type(),
            DataType::SmallInt
        );
        assert!(matches!(missing, CatalogError::UndefinedIndex { .. }));
    }

    #[test]
    fn tables_and_indexes_should_share_one_namespace() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        accounts_with_rows(&mut pool, &mut catalog, 10);
        catalog
            .create_index(pool.bpm(), "accounts_id", "accounts", "id")
            .unwrap();

        let as_table = catalog
            .create_table(pool.bpm(), "accounts_id", accounts())
            .unwrap_err();
        let as_index = catalog
            .create_index(pool.bpm(), "accounts_id", "accounts", "age")
            .unwrap_err();

        for err in [as_table, as_index] {
            assert!(
                matches!(err, CatalogError::DuplicateIndex { ref name } if name == "accounts_id"),
                "{err:?}"
            );
            assert_eq!(err.sqlstate(), "42P07");
        }
        assert_eq!(catalog.indexes().len(), 1);
    }

    #[test]
    fn create_index_should_work_on_every_column_type() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        let mut random = Rng(7);
        let columns: Vec<Column> = (0..10)
            .map(|i| Column::new(format!("c{i}"), random.data_type()))
            .collect();
        let types: Vec<DataType> = columns.iter().map(|column| column.data_type).collect();
        catalog
            .create_table(pool.bpm(), "all_types", Schema::new(columns).unwrap())
            .unwrap();
        let row = all_types_row(&types);
        let rid = catalog
            .table_heap(pool.bpm(), "all_types")
            .unwrap()
            .insert(pool.bpm(), &row)
            .unwrap();

        for (position, key) in row.iter().enumerate() {
            let name = format!("by_c{position}");
            catalog
                .create_index(pool.bpm(), &name, "all_types", &format!("c{position}"))
                .unwrap();

            let tree = catalog.index_tree(pool.bpm(), &name).unwrap();
            assert_eq!(tree.key_type(), types[position]);
            assert_eq!(tree.get(pool.bpm(), key).unwrap(), Some(rid), "{key:?}");
        }
    }

    /// One value of each column type (a short text for VARCHAR).
    fn all_types_row(types: &[DataType]) -> Vec<Value> {
        use crate::types::{Date, Finite32, Finite64, Timestamp};
        types
            .iter()
            .map(|data_type| match data_type {
                DataType::SmallInt => Value::SmallInt(-3),
                DataType::Integer => Value::Integer(42),
                DataType::BigInt => Value::BigInt(i64::MIN),
                DataType::Boolean => Value::Boolean(true),
                DataType::Real => Value::Real(Finite32::new(-1.5).unwrap()),
                DataType::Double => Value::Double(Finite64::new(2.25).unwrap()),
                DataType::Date => Value::Date(Date::from_ymd(2024, 2, 29).unwrap()),
                DataType::Timestamp => Value::Timestamp(Timestamp::at_midnight(Date::MIN)),
                DataType::Varchar(_) => Value::Text("k".to_string()),
            })
            .collect()
    }

    #[test]
    fn the_indexes_heap_should_be_created_by_the_first_index() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        accounts_with_rows(&mut pool, &mut catalog, 10);
        let before = stored_header(pool.bpm()).unwrap();

        catalog
            .create_index(pool.bpm(), "accounts_id", "accounts", "id")
            .unwrap();
        let after_first = stored_header(pool.bpm()).unwrap();
        catalog
            .create_index(pool.bpm(), "accounts_age", "accounts", "age")
            .unwrap();

        assert_eq!(before.indexes_first_page_id, 0);
        assert_ne!(after_first.indexes_first_page_id, 0);
        assert_eq!(stored_header(pool.bpm()).unwrap(), after_first);
        assert_eq!(
            after_first.tables_first_page_id,
            before.tables_first_page_id
        );
        assert_eq!(
            after_first.columns_first_page_id,
            before.columns_first_page_id
        );
    }

    #[test]
    fn indexes_should_come_back_after_reopening() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        let rids = accounts_with_rows(&mut pool, &mut catalog, 100);
        catalog
            .create_index(pool.bpm(), "accounts_id", "accounts", "id")
            .unwrap();
        catalog
            .create_index(pool.bpm(), "accounts_age", "accounts", "age")
            .unwrap();
        let created = catalog.indexes().to_vec();
        pool.reopen(16);

        let mut catalog = Catalog::open(pool.bpm()).unwrap();

        assert_eq!(catalog.indexes(), created);
        assert!(catalog.open_indexes.is_empty(), "trees open on first use");
        let tree = catalog.index_tree(pool.bpm(), "accounts_id").unwrap();
        assert_eq!(
            tree.get(pool.bpm(), &Value::Integer(42)).unwrap(),
            Some(rids[42])
        );
        let next = catalog
            .create_index(pool.bpm(), "accounts_id_2", "accounts", "id")
            .unwrap();
        assert_eq!(next.id, IndexId(3));
    }

    fn index_entry(index_id: u32, table_id: u32, column: u16, name: &str) -> Vec<u8> {
        IndexRecord {
            index_id: IndexId(index_id),
            table_id: TableId(table_id),
            column,
            header_page_id: 1_000,
            name: name.to_string(),
        }
        .encode()
        .unwrap()
    }

    /// A database with table "accounts" (id 1) and index "accounts_id" (id 1), plus a raw index
    /// entry written behind the catalog's back. Returns the catalog reopened.
    fn reopen_with_index_entry(pool: &mut TestPool, entry: &[u8]) -> Result<Catalog, CatalogError> {
        {
            let mut catalog = Catalog::open(pool.bpm()).unwrap();
            catalog
                .create_table(pool.bpm(), "accounts", accounts())
                .unwrap();
            catalog
                .create_index(pool.bpm(), "accounts_id", "accounts", "id")
                .unwrap();
            catalog
                .indexes_heap
                .as_mut()
                .unwrap()
                .insert(pool.bpm(), entry)
                .unwrap();
        }
        pool.reopen(16);
        Catalog::open(pool.bpm())
    }

    #[test]
    fn index_entries_that_do_not_match_the_catalog_should_fail_the_open() {
        let mut garbage = index_entry(5, 1, 1, "x");
        garbage.truncate(6);
        let cases = [
            (
                index_entry(5, 9, 0, "orphan"),
                "an index belongs to a table that does not exist",
            ),
            (
                index_entry(5, 1, 3, "past_the_end"),
                "an index points to a column that does not exist",
            ),
            (
                index_entry(5, 1, 1, "accounts_id"),
                "two relations have the same name",
            ),
            (
                index_entry(5, 1, 1, "accounts"),
                "two relations have the same name",
            ),
            (
                index_entry(1, 1, 1, "other"),
                "two indexes have the same id",
            ),
            (garbage, "catalog entry is truncated"),
        ];

        for (entry, reason) in cases {
            let mut pool = TestPool::new(16);

            let err = reopen_with_index_entry(&mut pool, &entry).unwrap_err();

            assert!(
                matches!(err, CatalogError::Corrupted { reason: found } if found == reason),
                "{err:?}"
            );
        }
    }

    /// The injected failure, wherever it surfaced: no free frame is `53000` in every layer.
    fn is_no_frame(err: &CatalogError) -> bool {
        err.sqlstate() == "53000"
    }

    /// After a failed `create_index`: no index, not even after reopening, and the same name
    /// can be created again.
    fn assert_no_index_after_failure(pool: &mut TestPool, catalog: &mut Catalog, existing: usize) {
        assert_eq!(catalog.indexes().len(), existing);
        assert!(matches!(
            catalog.index("by_id"),
            Err(CatalogError::UndefinedIndex { .. })
        ));
        pool.reopen(16);
        let mut reopened = Catalog::open(pool.bpm()).unwrap();
        assert_eq!(reopened.indexes().len(), existing);
        reopened
            .create_index(pool.bpm(), "by_id", "accounts", "id")
            .unwrap();
        let tree = reopened.index_tree(pool.bpm(), "by_id").unwrap();
        assert!(tree.get(pool.bpm(), &Value::Integer(3)).unwrap().is_some());
    }

    #[test]
    fn create_index_failing_to_read_the_table_should_leave_no_index() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        accounts_with_rows(&mut pool, &mut catalog, 20);
        let data_page = catalog.table("accounts").unwrap().first_page_id;
        pool.bpm().fail_fetch_after(data_page, 0);

        let err = catalog
            .create_index(pool.bpm(), "by_id", "accounts", "id")
            .unwrap_err();

        assert!(is_no_frame(&err), "{err:?}");
        assert_eq!(stored_header(pool.bpm()).unwrap().indexes_first_page_id, 0);
        assert_no_index_after_failure(&mut pool, &mut catalog, 0);
    }

    #[test]
    fn create_index_failing_to_point_the_header_to_the_indexes_heap_should_leave_no_index() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        accounts_with_rows(&mut pool, &mut catalog, 20);
        pool.bpm().fail_fetch_after(RESERVED_PAGE_ID, 0);

        let err = catalog
            .create_index(pool.bpm(), "by_id", "accounts", "id")
            .unwrap_err();

        assert!(is_no_frame(&err), "{err:?}");
        assert!(catalog.indexes_heap.is_none());
        assert_eq!(stored_header(pool.bpm()).unwrap().indexes_first_page_id, 0);
        assert_no_index_after_failure(&mut pool, &mut catalog, 0);
    }

    #[test]
    fn create_index_failing_to_write_its_entry_should_leave_no_index() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        accounts_with_rows(&mut pool, &mut catalog, 20);
        catalog
            .create_index(pool.bpm(), "by_age", "accounts", "age")
            .unwrap();
        let indexes_page = catalog.indexes_heap.as_ref().unwrap().first_page_id();
        pool.bpm().fail_fetch_after(indexes_page, 0);

        let err = catalog
            .create_index(pool.bpm(), "by_id", "accounts", "id")
            .unwrap_err();

        assert!(is_no_frame(&err), "{err:?}");
        assert_eq!(catalog.next_index_id, 3, "the failed attempt consumed id 2");
        assert_no_index_after_failure(&mut pool, &mut catalog, 1);
    }

    /// Two nullable INTEGER columns, `k` and `d`, so a column has repeated values or not.
    fn pair_schema() -> Schema {
        Schema::new(vec![
            Column::new("k", DataType::Integer),
            Column::new("d", DataType::Integer),
        ])
        .unwrap()
    }

    #[derive(Default)]
    struct IndexModel {
        /// Name and rows (`None` = NULL) of every table, in creation order.
        tables: Vec<(String, Vec<[Option<i32>; 2]>)>,
        /// Name, table, column and the keys the index got when it was created.
        indexes: Vec<(String, String, usize, Vec<i32>)>,
    }

    impl IndexModel {
        fn is_taken(&self, name: &str) -> bool {
            self.tables.iter().any(|(table, _)| table == name)
                || self.indexes.iter().any(|(index, ..)| index == name)
        }

        /// The SQLSTATE `create_index` must fail with, or the keys of the new index.
        fn create_index(
            &self,
            name: &str,
            table: &str,
            column: &str,
        ) -> Result<Vec<i32>, &'static str> {
            if self.is_taken(name) {
                return Err("42P07");
            }
            let (_, rows) = self
                .tables
                .iter()
                .find(|(existing, _)| existing == table)
                .ok_or("42P01")?;
            let position = ["k", "d"]
                .iter()
                .position(|existing| *existing == column)
                .ok_or("42703")?;
            let mut keys: Vec<i32> = rows.iter().filter_map(|row| row[position]).collect();
            keys.sort();
            if keys.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err("23505");
            }
            Ok(keys)
        }
    }

    fn assert_indexes_match(pool: &mut TestPool, catalog: &mut Catalog, model: &IndexModel) {
        let names: Vec<&str> = catalog
            .indexes()
            .iter()
            .map(|index| index.name.as_str())
            .collect();
        let expected: Vec<&str> = model
            .indexes
            .iter()
            .map(|(name, ..)| name.as_str())
            .collect();
        assert_eq!(names, expected);
        for (name, table, column, keys) in &model.indexes {
            assert_eq!(
                catalog
                    .table_indexes(table)
                    .unwrap()
                    .iter()
                    .filter(|index| &index.name == name)
                    .count(),
                1
            );
            assert_eq!(usize::from(catalog.index(name).unwrap().column), *column);
            let tree = catalog.index_tree(pool.bpm(), name).unwrap();
            let found: Vec<Value> = tree
                .range(pool.bpm(), Bound::Unbounded, Bound::Unbounded)
                .unwrap()
                .map(|entry| entry.unwrap().0)
                .collect();
            let expected: Vec<Value> = keys.iter().map(|&key| Value::Integer(key)).collect();
            assert_eq!(found, expected, "{name}");
        }
    }

    #[test]
    fn random_tables_and_indexes_should_match_model_across_reopens() {
        let mut rng = Rng(0xA076_1D64_78BD_642F);
        let mut pool = TestPool::new(8);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        let mut model = IndexModel::default();
        let name = |rng: &mut Rng| format!("r{}", rng.below(12));

        for step in 1..=400 {
            match rng.below(10) {
                0..=1 => {
                    let table = name(&mut rng);
                    let result = catalog.create_table(pool.bpm(), &table, pair_schema());
                    if model.is_taken(&table) {
                        assert_eq!(result.unwrap_err().sqlstate(), "42P07", "step {step}");
                    } else {
                        result.unwrap();
                        model.tables.push((table, Vec::new()));
                    }
                }
                2..=6 if !model.tables.is_empty() => {
                    let index = rng.below(model.tables.len());
                    let value = |rng: &mut Rng| match rng.below(6) {
                        0 => None,
                        _ => Some(rng.below(400) as i32),
                    };
                    let row = [value(&mut rng), value(&mut rng)];
                    let (table, rows) = &mut model.tables[index];
                    let values = row.map(|value| value.map_or(Value::Null, Value::Integer));
                    let heap = catalog.table_heap(pool.bpm(), table).unwrap();
                    heap.insert(pool.bpm(), &values).unwrap();
                    rows.push(row);
                }
                _ => {
                    let index = name(&mut rng);
                    let table = name(&mut rng);
                    let column = ["k", "d", "x"][rng.below(3)];
                    let result = catalog.create_index(pool.bpm(), &index, &table, column);
                    match model.create_index(&index, &table, column) {
                        Ok(keys) => {
                            result.unwrap();
                            let position = usize::from(column == "d");
                            model.indexes.push((index, table, position, keys));
                        }
                        Err(sqlstate) => {
                            assert_eq!(result.unwrap_err().sqlstate(), sqlstate, "step {step}");
                        }
                    }
                }
            }
            if step % 50 == 0 {
                pool.reopen(8);
                catalog = Catalog::open(pool.bpm()).unwrap();
                assert_indexes_match(&mut pool, &mut catalog, &model);
            }
        }
        assert!(
            model.indexes.len() >= 3,
            "the test should create some indexes"
        );
    }

    #[test]
    fn create_index_with_a_key_too_large_for_the_tree_should_leave_no_index() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        let notes = Schema::new(vec![Column::new(
            "note",
            DataType::Varchar(NonZeroU16::new(300).unwrap()),
        )])
        .unwrap();
        catalog.create_table(pool.bpm(), "notes", notes).unwrap();
        // The middle row is longer than the 111-byte keys of 512-byte pages.
        for note in ["a".to_string(), "b".repeat(200), "c".to_string()] {
            let heap = catalog.table_heap(pool.bpm(), "notes").unwrap();
            heap.insert(pool.bpm(), &[Value::Text(note)]).unwrap();
        }

        let err = catalog
            .create_index(pool.bpm(), "notes_note", "notes", "note")
            .unwrap_err();

        assert!(
            matches!(
                err,
                CatalogError::Index(IndexError::KeyTooLarge {
                    size: 200,
                    max: 111
                })
            ),
            "{err:?}"
        );
        assert_eq!(err.sqlstate(), "54000");
        assert!(catalog.indexes().is_empty());
        pool.reopen(16);
        assert!(Catalog::open(pool.bpm()).unwrap().indexes().is_empty());
    }

    #[test]
    fn create_index_after_the_last_id_should_fail_without_changes() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        accounts_with_rows(&mut pool, &mut catalog, 5);
        catalog.next_index_id = u64::from(u32::MAX);
        catalog
            .create_index(pool.bpm(), "last", "accounts", "id")
            .unwrap();

        let err = catalog
            .create_index(pool.bpm(), "one_more", "accounts", "id")
            .unwrap_err();
        pool.reopen(16);
        let mut reopened = Catalog::open(pool.bpm()).unwrap();
        let after_reopening = reopened
            .create_index(pool.bpm(), "one_more", "accounts", "id")
            .unwrap_err();

        assert!(matches!(err, CatalogError::TooManyIndexes), "{err:?}");
        assert_eq!(err.sqlstate(), "54000");
        assert_eq!(catalog.indexes().len(), 1);
        assert_eq!(reopened.index("last").unwrap().id, IndexId(u32::MAX));
        assert!(
            matches!(after_reopening, CatalogError::TooManyIndexes),
            "{after_reopening:?}"
        );
    }

    #[test]
    fn index_tree_of_an_entry_pointing_to_the_wrong_page_should_fail() {
        let mut pool = TestPool::new(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();
        accounts_with_rows(&mut pool, &mut catalog, 5);
        let integer_tree = catalog
            .create_index(pool.bpm(), "accounts_id", "accounts", "id")
            .unwrap()
            .header_page_id;
        let data_page = catalog.table("accounts").unwrap().first_page_id;
        // Entries that load (their table and column exist) but whose tree is not right: a data
        // page, and the INTEGER tree of another index for the SMALLINT column `age`.
        let entries = [
            (5, 0, data_page, "wrong_page"),
            (6, 1, integer_tree, "wrong_type"),
        ];
        for (id, column, header_page_id, name) in entries {
            let entry = IndexRecord {
                index_id: IndexId(id),
                table_id: TableId(1),
                column,
                header_page_id,
                name: name.to_string(),
            }
            .encode()
            .unwrap();
            catalog
                .indexes_heap
                .as_mut()
                .unwrap()
                .insert(pool.bpm(), &entry)
                .unwrap();
        }
        pool.reopen(16);
        let mut catalog = Catalog::open(pool.bpm()).unwrap();

        let wrong_page = catalog.index_tree(pool.bpm(), "wrong_page").unwrap_err();
        let wrong_type = catalog.index_tree(pool.bpm(), "wrong_type").unwrap_err();

        assert!(
            matches!(
                wrong_page,
                CatalogError::Index(IndexError::Corrupted { .. })
            ),
            "{wrong_page:?}"
        );
        assert_eq!(wrong_page.sqlstate(), "XX001");
        assert!(
            matches!(
                wrong_type,
                CatalogError::Index(IndexError::KeyTypeMismatch {
                    expected: DataType::Integer,
                    found: DataType::SmallInt
                })
            ),
            "{wrong_type:?}"
        );
        assert!(catalog.index_tree(pool.bpm(), "accounts_id").is_ok());
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
                CatalogError::EmptyIndexName,
                "an index name cannot be empty".to_string(),
                "42602",
                false,
            ),
            (
                CatalogError::IndexNameTooLong {
                    name: "accounts_by_id".to_string(),
                    max: 3,
                },
                "index name \"acc…\" is longer than 3 bytes".to_string(),
                "42622",
                false,
            ),
            (
                CatalogError::DuplicateIndex {
                    name: "accounts_id".to_string(),
                },
                "index \"accounts_id\" already exists".to_string(),
                "42P07",
                false,
            ),
            (
                CatalogError::UndefinedIndex {
                    name: "accounts_id".to_string(),
                },
                "index \"accounts_id\" does not exist".to_string(),
                "42704",
                false,
            ),
            (
                CatalogError::UndefinedColumn {
                    table: "accounts".to_string(),
                    column: "age".to_string(),
                },
                "column \"age\" of table \"accounts\" does not exist".to_string(),
                "42703",
                false,
            ),
            (
                CatalogError::TooManyIndexes,
                "no index ids left".to_string(),
                "54000",
                false,
            ),
            (
                CatalogError::Index(IndexError::DuplicateKey {
                    key: "7".to_string(),
                }),
                "duplicate key value (7) violates the unique index".to_string(),
                "23505",
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
