use std::fs;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::Catalog;
use ferro_db::config::PAGE_SIZE;
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::storage::heap_file::{HeapFile, HeapFileError};
use ferro_db::table::{Column, Schema};
use ferro_db::types::{DataType, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-string-{}-{id}.db", std::process::id())))
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

fn varchar(max_chars: u16) -> DataType {
    DataType::Varchar(NonZeroU16::new(max_chars).unwrap())
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

fn profiles() -> Schema {
    Schema::new(vec![
        Column::not_null("id", DataType::Integer),
        Column::not_null("name", varchar(40)),
        Column::new("bio", varchar(4000)),
    ])
    .unwrap()
}

/// Empty text, NULL, accents, emoji, names at exactly 40 characters and bios of 3000 bytes
/// that spread the table over many pages.
fn profile_rows() -> Vec<Vec<Value>> {
    (0..300)
        .map(|id| {
            let name = match id % 4 {
                0 => text(""),
                1 => text(&format!("ação {id}")),
                2 => text(&format!("🦀{}", "x".repeat(39))),
                _ => text(&format!("user {id}")),
            };
            let bio = match id {
                _ if id % 5 == 0 => Value::Null,
                _ if id % 7 == 0 => text(""),
                _ if id % 3 == 0 => text(&"é".repeat(1500)),
                _ => text(&format!("bio of {id} 名前")),
            };
            vec![Value::Integer(id), name, bio]
        })
        .collect()
}

fn insert_rows(bpm: &mut BufferPoolManager, catalog: &Catalog, table: &str, rows: &[Vec<Value>]) {
    let table = catalog.table(table).unwrap();
    let mut heap = HeapFile::open(bpm, table.first_page_id).unwrap();
    for row in rows {
        heap.insert(bpm, &table.schema.encode_row(row).unwrap())
            .unwrap();
    }
}

/// Sorted by the first column: rows of different sizes land wherever the free space map
/// finds room, so the scan does not follow insertion order.
fn read_rows(bpm: &mut BufferPoolManager, catalog: &Catalog, table: &str) -> Vec<Vec<Value>> {
    let table = catalog.table(table).unwrap();
    let heap = HeapFile::open(bpm, table.first_page_id).unwrap();
    let mut rows: Vec<Vec<Value>> = heap
        .scan(bpm)
        .map(|entry| table.schema.decode_row(&entry.unwrap().1).unwrap())
        .collect();
    rows.sort_by(|lhs, rhs| lhs[0].sort_cmp(&rhs[0]));
    rows
}

#[test]
fn text_rows_should_survive_eviction() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let mut catalog = Catalog::open(&mut bpm).unwrap();
    catalog
        .create_table(&mut bpm, "profiles", profiles())
        .unwrap();

    insert_rows(&mut bpm, &catalog, "profiles", &profile_rows());

    let first_page_id = catalog.table("profiles").unwrap().first_page_id;
    assert!(
        HeapFile::open(&mut bpm, first_page_id)
            .unwrap()
            .page_count()
            > 2
    );
    assert_eq!(read_rows(&mut bpm, &catalog, "profiles"), profile_rows());
}

#[test]
fn text_rows_and_varchar_columns_should_survive_reopening_the_file() {
    let db = TempDb::new();
    {
        let mut bpm = db.open(2);
        let mut catalog = Catalog::open(&mut bpm).unwrap();
        catalog
            .create_table(&mut bpm, "profiles", profiles())
            .unwrap();
        insert_rows(&mut bpm, &catalog, "profiles", &profile_rows());
    }

    let mut bpm = db.open(2);
    let catalog = Catalog::open(&mut bpm).unwrap();

    assert_eq!(catalog.table("profiles").unwrap().schema, profiles());
    assert_eq!(read_rows(&mut bpm, &catalog, "profiles"), profile_rows());
}

#[test]
fn the_varchar_length_limits_should_survive_the_catalog() {
    let db = TempDb::new();
    let limits = Schema::new(vec![
        Column::new("shortest", varchar(1)),
        Column::not_null("widest", DataType::VARCHAR_MAX),
    ])
    .unwrap();
    {
        let mut bpm = db.open(2);
        let mut catalog = Catalog::open(&mut bpm).unwrap();
        catalog
            .create_table(&mut bpm, "limits", limits.clone())
            .unwrap();
    }

    let mut bpm = db.open(2);
    let catalog = Catalog::open(&mut bpm).unwrap();

    assert_eq!(catalog.table("limits").unwrap().schema, limits);
}

#[test]
fn text_that_fits_the_column_but_not_a_page_should_be_too_large() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let mut catalog = Catalog::open(&mut bpm).unwrap();
    let notes = Schema::new(vec![Column::new("body", varchar(8000))]).unwrap();
    let table = catalog.create_table(&mut bpm, "notes", notes).unwrap();
    let mut heap = HeapFile::open(&mut bpm, table.first_page_id).unwrap();
    // 5000 characters fit VARCHAR(8000), but take 10000 bytes of UTF-8.
    let tuple = table.schema.encode_row(&[text(&"é".repeat(5000))]).unwrap();

    let err = heap.insert(&mut bpm, &tuple).unwrap_err();

    assert!(
        matches!(err, HeapFileError::TupleTooLarge { .. }),
        "{err:?}"
    );
}
