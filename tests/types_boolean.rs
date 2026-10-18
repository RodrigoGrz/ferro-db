use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::Catalog;
use ferro_db::config::PAGE_SIZE;
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::storage::heap_file::HeapFile;
use ferro_db::table::{Column, Schema, SchemaError};
use ferro_db::types::{DataType, TypeError, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-boolean-{}-{id}.db", std::process::id())))
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

fn users() -> Schema {
    Schema::new(vec![
        Column::not_null("id", DataType::Integer),
        Column::not_null("active", DataType::Boolean),
        Column::new("verified", DataType::Boolean),
    ])
    .unwrap()
}

/// Enough rows to spread over several pages; `verified` cycles TRUE, FALSE, NULL.
fn user_rows() -> Vec<Vec<Value>> {
    (0..2_000)
        .map(|id| {
            let verified = match id % 3 {
                0 => Value::Boolean(true),
                1 => Value::Boolean(false),
                _ => Value::Null,
            };
            vec![Value::Integer(id), Value::Boolean(id % 2 == 0), verified]
        })
        .collect()
}

fn create_users(bpm: &mut BufferPoolManager) -> Catalog {
    let mut catalog = Catalog::open(bpm).unwrap();
    catalog.create_table(bpm, "users", users()).unwrap();
    let table = catalog.table("users").unwrap();
    let mut heap = HeapFile::open(bpm, table.first_page_id).unwrap();
    for row in user_rows() {
        heap.insert(bpm, &table.schema.encode_row(&row).unwrap())
            .unwrap();
    }
    catalog
}

/// Sorted by `id`: the scan follows pages, not insertion order.
fn read_rows(bpm: &mut BufferPoolManager, catalog: &Catalog) -> Vec<Vec<Value>> {
    let table = catalog.table("users").unwrap();
    let heap = HeapFile::open(bpm, table.first_page_id).unwrap();
    let mut rows: Vec<Vec<Value>> = heap
        .scan(bpm)
        .map(|entry| table.schema.decode_row(&entry.unwrap().1).unwrap())
        .collect();
    rows.sort_by(|lhs, rhs| lhs[0].sort_cmp(&rhs[0]));
    rows
}

#[test]
fn boolean_rows_should_survive_eviction() {
    let db = TempDb::new();
    let mut bpm = db.open(2);

    let catalog = create_users(&mut bpm);

    let first_page_id = catalog.table("users").unwrap().first_page_id;
    assert!(
        HeapFile::open(&mut bpm, first_page_id)
            .unwrap()
            .page_count()
            > 2
    );
    assert_eq!(read_rows(&mut bpm, &catalog), user_rows());
}

#[test]
fn boolean_rows_and_columns_should_survive_reopening_the_file() {
    let db = TempDb::new();
    {
        let mut bpm = db.open(2);
        create_users(&mut bpm);
    }

    let mut bpm = db.open(2);
    let catalog = Catalog::open(&mut bpm).unwrap();

    assert_eq!(catalog.table("users").unwrap().schema, users());
    assert_eq!(read_rows(&mut bpm, &catalog), user_rows());
}

#[test]
fn a_where_filter_should_keep_only_rows_whose_condition_is_true() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let catalog = create_users(&mut bpm);
    let rows = read_rows(&mut bpm, &catalog);

    let both: Vec<i32> = rows
        .iter()
        .filter(|row| row[1].and(&row[2]).unwrap().is_true().unwrap())
        .map(|row| match row[0] {
            Value::Integer(id) => id,
            ref other => panic!("unexpected id {other:?}"),
        })
        .collect();
    let unknown = rows
        .iter()
        .filter(|row| row[2].is_unknown().unwrap())
        .count();

    let expected: Vec<i32> = (0..2_000).filter(|id| id % 2 == 0 && id % 3 == 0).collect();
    assert_eq!(both, expected);
    assert_eq!(unknown, (0..2_000).filter(|id| id % 3 == 2).count());
}

#[test]
fn a_stored_boolean_byte_other_than_0_or_1_should_be_corrupted() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let mut catalog = Catalog::open(&mut bpm).unwrap();
    let table = catalog.create_table(&mut bpm, "users", users()).unwrap();
    let mut heap = HeapFile::open(&mut bpm, table.first_page_id).unwrap();
    // Written below the schema, as damage that the page checksum cannot see would look.
    let id = 7_i32.to_le_bytes();
    let tuple = table
        .schema
        .layout()
        .encode(&[Some(&id[..]), Some(&[2]), None])
        .unwrap();
    let rid = heap.insert(&mut bpm, &tuple).unwrap();

    let stored = heap.get(&mut bpm, rid).unwrap();
    let err = table.schema.decode_row(&stored).unwrap_err();

    assert_eq!(
        err,
        SchemaError::Type {
            column: "active".to_string(),
            source: TypeError::CorruptedValue {
                ty: DataType::Boolean,
                reason: "boolean byte must be 0 or 1",
            },
        }
    );
    assert_eq!(err.sqlstate(), "XX001");
}
