//! Table heap: the rows of one table as typed values. `Schema` validates and encodes a row
//! (store assignment, NOT NULL, sizes) before `HeapFile` stores its bytes, so a rejected row
//! never writes anything; reads decode through the schema and never trust the disk.

use std::fmt;

use crate::storage::buffer_pool::BufferPoolManager;
use crate::storage::heap_file::{HeapFile, HeapFileError, HeapScan};
use crate::storage::rid::Rid;
use crate::types::Value;

use super::{Schema, SchemaError};

/// The rows of one table: `Schema` validates and encodes them, `HeapFile` stores them. It does
/// not know the table's name; the catalog finds a table by name and keeps one open heap each.
#[derive(Debug)]
pub struct TableHeap {
    schema: Schema,
    heap: HeapFile,
}

impl TableHeap {
    pub fn create(bpm: &mut BufferPoolManager, schema: Schema) -> Result<TableHeap, TableError> {
        let heap = HeapFile::create(bpm)?;
        Ok(TableHeap { schema, heap })
    }

    pub fn open(
        bpm: &mut BufferPoolManager,
        schema: Schema,
        first_page_id: u32,
    ) -> Result<TableHeap, TableError> {
        let heap = HeapFile::open(bpm, first_page_id)?;
        Ok(TableHeap { schema, heap })
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn first_page_id(&self) -> u32 {
        self.heap.first_page_id()
    }

    /// Store assignment first (`Schema::encode_row`), so a rejected row writes nothing.
    pub fn insert(
        &mut self,
        bpm: &mut BufferPoolManager,
        row: &[Value],
    ) -> Result<Rid, TableError> {
        let tuple = self.schema.encode_row(row)?;
        Ok(self.heap.insert(bpm, &tuple)?)
    }

    /// The row is assigned before the heap is touched, so a rejected row keeps the old one.
    /// The `Rid` changes when the new row no longer fits its page (`HeapFile::update`).
    pub fn update(
        &mut self,
        bpm: &mut BufferPoolManager,
        rid: Rid,
        row: &[Value],
    ) -> Result<Rid, TableError> {
        let tuple = self.schema.encode_row(row)?;
        Ok(self.heap.update(bpm, rid, &tuple)?)
    }

    pub fn delete(&mut self, bpm: &mut BufferPoolManager, rid: Rid) -> Result<(), TableError> {
        Ok(self.heap.delete(bpm, rid)?)
    }

    pub fn scan<'a>(&'a self, bpm: &'a mut BufferPoolManager) -> TableScan<'a> {
        TableScan {
            schema: &self.schema,
            rows: self.heap.scan(bpm),
        }
    }

    pub fn get(&self, bpm: &mut BufferPoolManager, rid: Rid) -> Result<Vec<Value>, TableError> {
        let tuple = self.heap.get(bpm, rid)?;
        Ok(self.schema.decode_row(&tuple)?)
    }
}

/// Live rows in heap order (page by page, slot by slot), decoded. A row that does not decode is
/// an error for that row, never skipped.
pub struct TableScan<'a> {
    schema: &'a Schema,
    rows: HeapScan<'a>,
}

impl Iterator for TableScan<'_> {
    type Item = Result<(Rid, Vec<Value>), TableError>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.rows.next()?;
        Some(
            entry
                .map_err(TableError::from)
                .and_then(|(rid, tuple)| Ok((rid, self.schema.decode_row(&tuple)?))),
        )
    }
}

#[derive(Debug)]
pub enum TableError {
    /// The row was rejected (assignment, NOT NULL, size) or a stored row does not decode.
    Schema(SchemaError),
    Heap(HeapFileError),
}

impl TableError {
    pub fn sqlstate(&self) -> &'static str {
        match self {
            TableError::Schema(err) => err.sqlstate(),
            TableError::Heap(err) => err.sqlstate(),
        }
    }
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableError::Schema(err) => write!(f, "{err}"),
            TableError::Heap(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for TableError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TableError::Schema(err) => Some(err),
            TableError::Heap(err) => Some(err),
        }
    }
}

impl From<SchemaError> for TableError {
    fn from(err: SchemaError) -> Self {
        TableError::Schema(err)
    }
}

impl From<HeapFileError> for TableError {
    fn from(err: HeapFileError) -> Self {
        TableError::Heap(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::buffer_pool::BufferPoolError;
    use crate::storage::disk_manager::DiskError;
    use crate::storage::page::PageError;
    use crate::storage::slotted_page::SlottedPageError;
    use crate::table::Column;
    use crate::types::DataType;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    const TEST_PAGE_SIZE: usize = 512;

    struct TestPool {
        path: PathBuf,
        bpm: Option<BufferPoolManager>,
    }

    impl TestPool {
        fn new(pool_size: usize) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("ferro-table-heap-{}-{id}.db", std::process::id()));
            let bpm = BufferPoolManager::new(pool_size, TEST_PAGE_SIZE, &path).unwrap();
            Self {
                path,
                bpm: Some(bpm),
            }
        }

        fn bpm(&mut self) -> &mut BufferPoolManager {
            self.bpm.as_mut().unwrap()
        }

        /// Drops the pool (flushing every page) and opens the file again.
        fn reopen(&mut self, pool_size: usize) {
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

    fn accounts() -> Schema {
        Schema::new(vec![
            Column::not_null("id", DataType::Integer),
            Column::new("age", DataType::SmallInt),
            Column::new(
                "name",
                DataType::Varchar(std::num::NonZeroU16::new(20).unwrap()),
            ),
        ])
        .unwrap()
    }

    #[test]
    fn create_should_keep_the_schema_and_a_heap() {
        let mut pool = TestPool::new(4);

        let table = TableHeap::create(pool.bpm(), accounts()).unwrap();

        assert_eq!(table.schema(), &accounts());
        assert_ne!(table.first_page_id(), 0);
    }

    #[test]
    fn open_should_find_the_heap_again_after_reopening_the_file() {
        let mut pool = TestPool::new(4);
        let first_page_id = TableHeap::create(pool.bpm(), accounts())
            .unwrap()
            .first_page_id();
        pool.reopen(4);

        let table = TableHeap::open(pool.bpm(), accounts(), first_page_id).unwrap();

        assert_eq!(table.first_page_id(), first_page_id);
        assert_eq!(table.schema(), &accounts());
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_string())
    }

    fn stored_tuples(pool: &mut TestPool, table: &TableHeap) -> usize {
        table.heap.scan(pool.bpm()).count()
    }

    #[test]
    fn insert_then_get_should_return_the_row() {
        let mut pool = TestPool::new(4);
        let mut table = TableHeap::create(pool.bpm(), accounts()).unwrap();
        let row = vec![Value::Integer(1), Value::SmallInt(30), text("ana")];

        let rid = table.insert(pool.bpm(), &row).unwrap();

        assert_eq!(table.get(pool.bpm(), rid).unwrap(), row);
    }

    #[test]
    fn insert_should_store_the_assigned_row() {
        let mut pool = TestPool::new(4);
        let mut table = TableHeap::create(pool.bpm(), accounts()).unwrap();
        let name = format!("{}   ", "a".repeat(20));

        let rid = table
            .insert(pool.bpm(), &[Value::SmallInt(7), Value::Null, text(&name)])
            .unwrap();

        assert_eq!(
            table.get(pool.bpm(), rid).unwrap(),
            [Value::Integer(7), Value::Null, text(&"a".repeat(20))]
        );
    }

    #[test]
    fn insert_of_a_rejected_row_should_write_nothing() {
        let mut pool = TestPool::new(4);
        let mut table = TableHeap::create(pool.bpm(), accounts()).unwrap();
        let cases = [
            (vec![Value::Null, Value::Null, Value::Null], "23502"),
            (vec![Value::Integer(1)], "42601"),
            (vec![text("1"), Value::Null, Value::Null], "42804"),
            (
                vec![Value::Integer(1), Value::Null, text(&"b".repeat(21))],
                "22001",
            ),
            (
                vec![Value::Integer(1), Value::Integer(70_000), Value::Null],
                "22003",
            ),
        ];

        for (row, sqlstate) in cases {
            let err = table.insert(pool.bpm(), &row).unwrap_err();

            assert!(matches!(err, TableError::Schema(_)), "{err:?}");
            assert_eq!(err.sqlstate(), sqlstate);
        }
        assert_eq!(stored_tuples(&mut pool, &table), 0);
    }

    #[test]
    fn insert_of_a_row_larger_than_a_page_should_be_tuple_too_large() {
        let mut pool = TestPool::new(4);
        let notes = Schema::new(vec![Column::new(
            "body",
            DataType::Varchar(std::num::NonZeroU16::new(1_000).unwrap()),
        )])
        .unwrap();
        let mut table = TableHeap::create(pool.bpm(), notes).unwrap();

        let err = table
            .insert(pool.bpm(), &[text(&"x".repeat(600))])
            .unwrap_err();

        assert!(
            matches!(err, TableError::Heap(HeapFileError::TupleTooLarge { .. })),
            "{err:?}"
        );
        assert_eq!(err.sqlstate(), "54000");
        assert_eq!(stored_tuples(&mut pool, &table), 0);
    }

    #[test]
    fn get_of_a_rid_outside_the_table_should_fail() {
        let mut pool = TestPool::new(4);
        let mut table = TableHeap::create(pool.bpm(), accounts()).unwrap();
        let rid = table
            .insert(pool.bpm(), &[Value::Integer(1), Value::Null, Value::Null])
            .unwrap();

        let other_page = table.get(pool.bpm(), Rid::new(999, 0)).unwrap_err();
        let missing_slot = table
            .get(pool.bpm(), Rid::new(rid.page_id, rid.slot_id + 1))
            .unwrap_err();

        assert!(matches!(
            other_page,
            TableError::Heap(HeapFileError::PageNotInHeap { .. })
        ));
        assert!(matches!(
            missing_slot,
            TableError::Heap(HeapFileError::TupleNotFound { .. })
        ));
        assert_eq!(missing_slot.sqlstate(), "XX000");
    }

    fn tiny_row(id: i32) -> Vec<Value> {
        vec![Value::Integer(id), Value::Null, Value::Null]
    }

    /// A row on a page so full that not even a tiny row fits: growing it must move it.
    fn row_on_a_full_page(pool: &mut TestPool, table: &mut TableHeap) -> Rid {
        let rid = table.insert(pool.bpm(), &tiny_row(0)).unwrap();
        for id in 1.. {
            let filler = table.insert(pool.bpm(), &tiny_row(id)).unwrap();
            if filler.page_id != rid.page_id {
                break;
            }
        }
        rid
    }

    #[test]
    fn update_that_fits_should_keep_the_rid() {
        let mut pool = TestPool::new(4);
        let mut table = TableHeap::create(pool.bpm(), accounts()).unwrap();
        let rid = table
            .insert(pool.bpm(), &[Value::Integer(1), Value::Null, text("ana")])
            .unwrap();
        let new_row = vec![Value::Integer(1), Value::SmallInt(40), text("bea")];

        let updated = table.update(pool.bpm(), rid, &new_row).unwrap();

        assert_eq!(updated, rid);
        assert_eq!(table.get(pool.bpm(), rid).unwrap(), new_row);
    }

    #[test]
    fn update_that_no_longer_fits_its_page_should_move_the_row() {
        let mut pool = TestPool::new(4);
        let mut table = TableHeap::create(pool.bpm(), accounts()).unwrap();
        let rid = row_on_a_full_page(&mut pool, &mut table);
        let grown = vec![Value::Integer(0), Value::Null, text(&"x".repeat(20))];

        let moved = table.update(pool.bpm(), rid, &grown).unwrap();

        assert_ne!(moved, rid);
        assert_eq!(table.get(pool.bpm(), moved).unwrap(), grown);
        assert!(matches!(
            table.get(pool.bpm(), rid).unwrap_err(),
            TableError::Heap(HeapFileError::TupleNotFound { .. })
        ));
    }

    #[test]
    fn update_with_a_rejected_row_should_keep_the_old_row() {
        let mut pool = TestPool::new(4);
        let mut table = TableHeap::create(pool.bpm(), accounts()).unwrap();
        let row = vec![Value::Integer(1), Value::SmallInt(30), text("ana")];
        let rid = table.insert(pool.bpm(), &row).unwrap();

        let err = table
            .update(pool.bpm(), rid, &tiny_row(1)[..1])
            .unwrap_err();
        let null_id = table
            .update(pool.bpm(), rid, &[Value::Null, Value::Null, Value::Null])
            .unwrap_err();

        assert_eq!(err.sqlstate(), "42601");
        assert_eq!(null_id.sqlstate(), "23502");
        assert_eq!(table.get(pool.bpm(), rid).unwrap(), row);
    }

    #[test]
    fn delete_should_remove_the_row_once() {
        let mut pool = TestPool::new(4);
        let mut table = TableHeap::create(pool.bpm(), accounts()).unwrap();
        let rid = table.insert(pool.bpm(), &tiny_row(1)).unwrap();

        table.delete(pool.bpm(), rid).unwrap();

        let not_found =
            |err: TableError| matches!(err, TableError::Heap(HeapFileError::TupleNotFound { .. }));
        assert!(not_found(table.get(pool.bpm(), rid).unwrap_err()));
        assert!(not_found(table.delete(pool.bpm(), rid).unwrap_err()));
        assert_eq!(stored_tuples(&mut pool, &table), 0);
    }

    #[test]
    fn update_and_delete_of_a_rid_outside_the_table_should_fail() {
        let mut pool = TestPool::new(4);
        let mut table = TableHeap::create(pool.bpm(), accounts()).unwrap();
        let foreign = Rid::new(999, 0);

        let update = table.update(pool.bpm(), foreign, &tiny_row(1)).unwrap_err();
        let delete = table.delete(pool.bpm(), foreign).unwrap_err();

        for err in [update, delete] {
            assert!(matches!(
                err,
                TableError::Heap(HeapFileError::PageNotInHeap { .. })
            ));
        }
    }

    fn scanned_rows(pool: &mut TestPool, table: &TableHeap) -> Vec<Vec<Value>> {
        let mut rows: Vec<Vec<Value>> = table
            .scan(pool.bpm())
            .map(|entry| entry.unwrap().1)
            .collect();
        rows.sort_by(|lhs, rhs| lhs[0].sort_cmp(&rhs[0]));
        rows
    }

    #[test]
    fn scan_of_an_empty_table_should_yield_nothing() {
        let mut pool = TestPool::new(4);
        let table = TableHeap::create(pool.bpm(), accounts()).unwrap();

        let rows = table.scan(pool.bpm()).count();

        assert_eq!(rows, 0);
    }

    #[test]
    fn scan_should_yield_every_live_row_decoded_across_pages() {
        let mut pool = TestPool::new(2);
        let mut table = TableHeap::create(pool.bpm(), accounts()).unwrap();
        let row = |id: i32| {
            vec![
                Value::Integer(id),
                Value::SmallInt(1),
                text(&format!("user {id}")),
            ]
        };
        let rids: Vec<Rid> = (0..60)
            .map(|id| table.insert(pool.bpm(), &row(id)).unwrap())
            .collect();
        for &rid in rids.iter().step_by(3) {
            table.delete(pool.bpm(), rid).unwrap();
        }
        let grown = vec![Value::Integer(1), Value::Null, text(&"z".repeat(20))];
        table.update(pool.bpm(), rids[1], &grown).unwrap();

        let rows = scanned_rows(&mut pool, &table);

        let mut expected: Vec<Vec<Value>> = (0..60).filter(|id| id % 3 != 0).map(row).collect();
        expected[0] = grown;
        assert!(table.heap.page_count() > 2);
        assert_eq!(rows, expected);
    }

    #[test]
    fn scan_should_report_a_row_that_does_not_decode_and_go_on() {
        let mut pool = TestPool::new(4);
        let mut table = TableHeap::create(pool.bpm(), accounts()).unwrap();
        table.insert(pool.bpm(), &tiny_row(1)).unwrap();
        // Written below the schema: one column, as a row of another table would look.
        let other = Schema::new(vec![Column::new("x", DataType::Integer)]).unwrap();
        let foreign = other.encode_row(&[Value::Integer(9)]).unwrap();
        table.heap.insert(pool.bpm(), &foreign).unwrap();
        table.insert(pool.bpm(), &tiny_row(2)).unwrap();

        let results: Vec<_> = table.scan(pool.bpm()).collect();

        assert_eq!(results.len(), 3);
        assert!(matches!(
            results[1],
            Err(TableError::Schema(SchemaError::Tuple(_)))
        ));
        assert_eq!(results[1].as_ref().unwrap_err().sqlstate(), "XX001");
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 2);
    }

    #[test]
    fn get_of_a_row_that_does_not_decode_should_be_corrupted() {
        let mut pool = TestPool::new(4);
        let mut table = TableHeap::create(pool.bpm(), accounts()).unwrap();
        let other = Schema::new(vec![Column::new("x", DataType::Integer)]).unwrap();
        let foreign = other.encode_row(&[Value::Integer(9)]).unwrap();
        let rid = table.heap.insert(pool.bpm(), &foreign).unwrap();

        let err = table.get(pool.bpm(), rid).unwrap_err();

        assert!(
            matches!(err, TableError::Schema(SchemaError::Tuple(_))),
            "{err:?}"
        );
        assert_eq!(err.sqlstate(), "XX001");
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
    }

    /// A row for `accounts`, sometimes one the schema rejects: NULL id, an age that does not
    /// fit SMALLINT, or a name past VARCHAR(20) (no trailing spaces, so nothing is cut).
    fn random_row(rng: &mut Rng) -> Vec<Value> {
        let id = match rng.below(20) {
            0 => Value::Null,
            _ => Value::Integer(rng.below(1_000) as i32),
        };
        let age = match rng.below(10) {
            0 => Value::Null,
            1 => Value::Integer(70_000),
            _ => Value::SmallInt(rng.below(100) as i16),
        };
        let name = match rng.below(5) {
            0 => Value::Null,
            _ => text(&"n".repeat(rng.below(24))),
        };
        vec![id, age, name]
    }

    /// The row as stored, or `None` when the schema rejects it.
    fn model_assign(row: &[Value]) -> Option<Vec<Value>> {
        let is_valid = !row[0].is_null()
            && row[1] != Value::Integer(70_000)
            && !matches!(&row[2], Value::Text(name) if name.chars().count() > 20);
        let age = match &row[1] {
            Value::SmallInt(age) => Value::SmallInt(*age),
            _ => Value::Null,
        };
        is_valid.then(|| vec![row[0].clone(), age, row[2].clone()])
    }

    #[test]
    fn random_operations_should_match_model() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut pool = TestPool::new(2);
        let mut table = TableHeap::create(pool.bpm(), accounts()).unwrap();
        let mut model: HashMap<Rid, Vec<Value>> = HashMap::new();

        for step in 0..2_000 {
            let live: Vec<Rid> = model.keys().copied().collect();
            let target = (!live.is_empty()).then(|| live[rng.below(live.len())]);
            match (rng.below(10), target) {
                (0..=4, _) | (_, None) => {
                    let row = random_row(&mut rng);
                    let result = table.insert(pool.bpm(), &row);
                    match model_assign(&row) {
                        Some(stored) => {
                            model.insert(result.unwrap(), stored);
                        }
                        None => assert!(matches!(result, Err(TableError::Schema(_)))),
                    }
                }
                (5..=6, Some(rid)) => {
                    let row = random_row(&mut rng);
                    let result = table.update(pool.bpm(), rid, &row);
                    match model_assign(&row) {
                        Some(stored) => {
                            model.remove(&rid);
                            model.insert(result.unwrap(), stored);
                        }
                        None => assert!(matches!(result, Err(TableError::Schema(_)))),
                    }
                }
                (7..=8, Some(rid)) => {
                    table.delete(pool.bpm(), rid).unwrap();
                    model.remove(&rid);
                }
                (_, Some(rid)) => {
                    assert_eq!(table.get(pool.bpm(), rid).unwrap(), model[&rid]);
                }
            }

            if step % 50 == 0 {
                let scanned: HashMap<Rid, Vec<Value>> =
                    table.scan(pool.bpm()).map(|entry| entry.unwrap()).collect();
                assert_eq!(scanned, model, "step {step}");
            }
        }
        let scanned: HashMap<Rid, Vec<Value>> =
            table.scan(pool.bpm()).map(|entry| entry.unwrap()).collect();
        assert_eq!(scanned, model);
    }

    #[test]
    fn open_of_the_reserved_page_should_fail() {
        let mut pool = TestPool::new(4);

        let err = TableHeap::open(pool.bpm(), accounts(), 0).unwrap_err();

        assert!(matches!(err, TableError::Heap(_)), "{err:?}");
    }

    fn every_error() -> Vec<(TableError, &'static str)> {
        vec![
            (
                SchemaError::NullViolation {
                    column: "id".to_string(),
                }
                .into(),
                "23502",
            ),
            (
                SchemaError::RowTooLarge {
                    size: 70_000,
                    max: 65_535,
                }
                .into(),
                "54000",
            ),
            (
                HeapFileError::TupleTooLarge {
                    size: 9_000,
                    max: 8_164,
                }
                .into(),
                "54000",
            ),
            (
                HeapFileError::BufferPool(BufferPoolError::NoAvailableFrame).into(),
                "53000",
            ),
            (
                HeapFileError::BufferPool(BufferPoolError::PageIdOverflow).into(),
                "54000",
            ),
            (
                HeapFileError::BufferPool(BufferPoolError::ChecksumMismatch { page_id: 4 }).into(),
                "XX001",
            ),
            (
                HeapFileError::BufferPool(BufferPoolError::Page(PageError::UnsupportedVersion {
                    found: 9,
                    expected: 1,
                }))
                .into(),
                "XX001",
            ),
            (
                HeapFileError::BufferPool(BufferPoolError::Disk(DiskError::TornPage {
                    page_id: 2,
                    file_len: 100,
                }))
                .into(),
                "XX001",
            ),
            (
                HeapFileError::BufferPool(BufferPoolError::Disk(DiskError::Io(
                    std::io::Error::other("device lost"),
                )))
                .into(),
                "58030",
            ),
            (
                HeapFileError::BufferPool(BufferPoolError::InvalidPoolSize).into(),
                "XX000",
            ),
            (
                HeapFileError::TupleNotFound {
                    rid: Rid::new(3, 1),
                }
                .into(),
                "XX000",
            ),
            (HeapFileError::PageNotInHeap { page_id: 9 }.into(), "XX000"),
            (
                HeapFileError::UnsupportedPageSize { size: 16 }.into(),
                "XX000",
            ),
            (
                HeapFileError::CorruptedChain {
                    page_id: 4,
                    reason: "cycle",
                }
                .into(),
                "XX001",
            ),
            (
                HeapFileError::Page {
                    page_id: 4,
                    source: SlottedPageError::Corrupted { reason: "bad slot" },
                }
                .into(),
                "XX001",
            ),
        ]
    }

    #[test]
    fn errors_should_map_to_sql_standard_sqlstates() {
        for (err, sqlstate) in every_error() {
            assert_eq!(err.sqlstate(), sqlstate, "{err:?}");
        }
    }

    #[test]
    fn errors_should_show_and_expose_the_wrapped_error() {
        for (err, _) in every_error() {
            let inner = match &err {
                TableError::Schema(inner) => inner.to_string(),
                TableError::Heap(inner) => inner.to_string(),
            };

            assert_eq!(err.to_string(), inner);
            assert!(std::error::Error::source(&err).is_some(), "{err:?}");
        }
    }
}
