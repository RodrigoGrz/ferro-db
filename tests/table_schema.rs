use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::config::PAGE_SIZE;
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::storage::heap_file::{HeapFile, HeapFileError};
use ferro_db::storage::rid::Rid;
use ferro_db::table::{Column, Schema, SchemaError};
use ferro_db::types::{DataType, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-schema-{}-{id}.db", std::process::id())))
    }

    fn open(&self, pool_size: usize) -> BufferPoolManager {
        BufferPoolManager::new(pool_size, PAGE_SIZE, &self.0).unwrap()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// `(id INTEGER NOT NULL, age SMALLINT, balance BIGINT)`
fn accounts() -> Schema {
    Schema::new(vec![
        Column::not_null("id", DataType::Integer),
        Column::new("age", DataType::SmallInt),
        Column::new("balance", DataType::BigInt),
    ])
    .unwrap()
}

/// Enough rows to fill several 8 KB pages (each row is 17 bytes plus a 4-byte slot).
fn rows(count: i32) -> Vec<Vec<Value>> {
    (0..count)
        .map(|id| {
            let age = if id % 5 == 0 {
                Value::Null
            } else {
                Value::SmallInt((id % 100) as i16)
            };
            vec![
                Value::Integer(id),
                age,
                Value::BigInt(i64::from(id) * 1_000_000_007),
            ]
        })
        .collect()
}

fn insert_all(
    heap: &mut HeapFile,
    bpm: &mut BufferPoolManager,
    schema: &Schema,
    rows: &[Vec<Value>],
) -> Vec<Rid> {
    rows.iter()
        .map(|row| heap.insert(bpm, &schema.encode_row(row).unwrap()).unwrap())
        .collect()
}

fn scan_rows(heap: &HeapFile, bpm: &mut BufferPoolManager, schema: &Schema) -> Vec<Vec<Value>> {
    heap.scan(bpm)
        .map(|entry| schema.decode_row(&entry.unwrap().1).unwrap())
        .collect()
}

#[test]
fn typed_rows_should_survive_eviction() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let schema = accounts();
    let mut heap = HeapFile::create(&mut bpm).unwrap();
    let expected = rows(1_000);

    let rids = insert_all(&mut heap, &mut bpm, &schema, &expected);

    assert!(heap.page_count() > 2);
    for (rid, row) in rids.into_iter().zip(&expected) {
        let tuple = heap.get(&mut bpm, rid).unwrap();
        assert_eq!(&schema.decode_row(&tuple).unwrap(), row);
    }
}

#[test]
fn typed_rows_should_survive_reopening_the_file() {
    let db = TempDb::new();
    let schema = accounts();
    let expected = rows(1_000);
    let first_page_id = {
        let mut bpm = db.open(2);
        let mut heap = HeapFile::create(&mut bpm).unwrap();
        insert_all(&mut heap, &mut bpm, &schema, &expected);
        heap.first_page_id()
    };

    let mut bpm = db.open(2);
    let heap = HeapFile::open(&mut bpm, first_page_id).unwrap();

    assert_eq!(scan_rows(&heap, &mut bpm, &schema), expected);
}

#[test]
fn decode_column_should_project_one_column_from_stored_rows() {
    let db = TempDb::new();
    let mut bpm = db.open(4);
    let schema = accounts();
    let mut heap = HeapFile::create(&mut bpm).unwrap();
    insert_all(&mut heap, &mut bpm, &schema, &rows(50));
    let balance = schema.column_index("balance").unwrap();

    let balances: Vec<Value> = heap
        .scan(&mut bpm)
        .map(|entry| schema.decode_column(&entry.unwrap().1, balance).unwrap())
        .collect();

    let expected: Vec<Value> = (0..50)
        .map(|id| Value::BigInt(i64::from(id) * 1_000_000_007))
        .collect();
    assert_eq!(balances, expected);
}

#[test]
fn rejected_rows_should_never_reach_the_heap() {
    let db = TempDb::new();
    let mut bpm = db.open(4);
    let schema = accounts();
    let mut heap = HeapFile::create(&mut bpm).unwrap();
    let invalid = [
        vec![Value::Null, Value::Null, Value::Null],
        vec![Value::Integer(1), Value::Integer(40_000), Value::Null],
        vec![Value::Integer(1)],
    ];

    for row in &invalid {
        if let Ok(tuple) = schema.encode_row(row) {
            heap.insert(&mut bpm, &tuple).unwrap();
        }
    }

    assert!(matches!(
        schema.encode_row(&invalid[0]),
        Err(SchemaError::NullViolation { .. })
    ));
    assert_eq!(heap.scan(&mut bpm).count(), 0);
}

/// A schema can be valid and still have rows larger than a page: the heap is what refuses
/// them, until `CREATE TABLE` checks `min_row_size` against the page size.
#[test]
fn rows_wider_than_a_page_should_be_rejected_by_the_heap() {
    let db = TempDb::new();
    let mut bpm = db.open(4);
    let columns = (0..1_600)
        .map(|i| Column::new(format!("c{i}"), DataType::BigInt))
        .collect();
    let schema = Schema::new(columns).unwrap();
    let mut heap = HeapFile::create(&mut bpm).unwrap();
    let tuple = schema.encode_row(&vec![Value::BigInt(1); 1_600]).unwrap();

    let err = heap.insert(&mut bpm, &tuple).unwrap_err();

    assert!(schema.min_row_size() > PAGE_SIZE);
    assert!(matches!(
        err,
        HeapFileError::TupleTooLarge { size, .. } if size == schema.min_row_size()
    ));
    assert_eq!(heap.scan(&mut bpm).count(), 0);
}
