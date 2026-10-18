use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::Catalog;
use ferro_db::config::PAGE_SIZE;
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::storage::heap_file::HeapFile;
use ferro_db::table::{Column, Schema, SchemaError};
use ferro_db::types::{DataType, Date, TypeError, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-date-{}-{id}.db", std::process::id())))
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

fn date(year: i32, month: u32, day: u32) -> Value {
    Value::Date(Date::from_ymd(year, month, day).unwrap())
}

fn people() -> Schema {
    Schema::new(vec![
        Column::not_null("id", DataType::Integer),
        Column::not_null("born", DataType::Date),
        Column::new("died", DataType::Date),
    ])
    .unwrap()
}

/// Enough rows for several pages: the ends of the range, leap days, the epoch, and NULL.
fn people_rows() -> Vec<Vec<Value>> {
    let special = [
        Value::Date(Date::MIN),
        Value::Date(Date::MAX),
        date(1970, 1, 1),
        date(2000, 2, 29),
        date(1600, 2, 29),
    ];
    (0..2_000)
        .map(|id: i32| {
            let index = usize::try_from(id).unwrap();
            let born = special
                .get(index)
                .cloned()
                .unwrap_or_else(|| date(1900, 1, 1).add(&Value::Integer(id * 17)).unwrap());
            let died = match id % 3 {
                0 => Value::Null,
                _ => born.add(&Value::Integer(30_000)).unwrap_or(Value::Null),
            };
            vec![Value::Integer(id), born, died]
        })
        .collect()
}

fn create_people(bpm: &mut BufferPoolManager) -> Catalog {
    let mut catalog = Catalog::open(bpm).unwrap();
    catalog.create_table(bpm, "people", people()).unwrap();
    let table = catalog.table("people").unwrap();
    let mut heap = HeapFile::open(bpm, table.first_page_id).unwrap();
    for row in people_rows() {
        heap.insert(bpm, &table.schema.encode_row(&row).unwrap())
            .unwrap();
    }
    catalog
}

/// Sorted by `id`: the scan follows pages, not insertion order.
fn read_rows(bpm: &mut BufferPoolManager, catalog: &Catalog) -> Vec<Vec<Value>> {
    let table = catalog.table("people").unwrap();
    let heap = HeapFile::open(bpm, table.first_page_id).unwrap();
    let mut rows: Vec<Vec<Value>> = heap
        .scan(bpm)
        .map(|entry| table.schema.decode_row(&entry.unwrap().1).unwrap())
        .collect();
    rows.sort_by(|lhs, rhs| lhs[0].sort_cmp(&rhs[0]));
    rows
}

#[test]
fn date_rows_should_survive_eviction() {
    let db = TempDb::new();
    let mut bpm = db.open(2);

    let catalog = create_people(&mut bpm);

    let first_page_id = catalog.table("people").unwrap().first_page_id;
    assert!(
        HeapFile::open(&mut bpm, first_page_id)
            .unwrap()
            .page_count()
            > 2
    );
    assert_eq!(read_rows(&mut bpm, &catalog), people_rows());
}

#[test]
fn date_rows_and_columns_should_survive_reopening_the_file() {
    let db = TempDb::new();
    {
        let mut bpm = db.open(2);
        create_people(&mut bpm);
    }

    let mut bpm = db.open(2);
    let catalog = Catalog::open(&mut bpm).unwrap();

    assert_eq!(catalog.table("people").unwrap().schema, people());
    assert_eq!(read_rows(&mut bpm, &catalog), people_rows());
}

#[test]
fn stored_dates_should_support_day_arithmetic_and_comparison() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let catalog = create_people(&mut bpm);
    let rows = read_rows(&mut bpm, &catalog);

    let lifespans: Vec<Value> = rows
        .iter()
        .filter(|row| !row[2].is_null())
        .map(|row| row[2].sub(&row[1]).unwrap())
        .collect();
    let born_after_2000 = rows
        .iter()
        .filter(|row| row[1].sql_gt(&date(2000, 1, 1)).unwrap().is_true().unwrap())
        .count();

    let year_2000 = Date::from_ymd(2000, 1, 1).unwrap();
    let expected_after_2000 = rows
        .iter()
        .filter(|row| matches!(&row[1], Value::Date(day) if *day > year_2000))
        .count();
    assert!(!lifespans.is_empty());
    assert!(lifespans.iter().all(|days| *days == Value::Integer(30_000)));
    assert!(born_after_2000 > 0);
    assert_eq!(born_after_2000, expected_after_2000);
}

#[test]
fn a_stored_day_outside_the_range_should_be_corrupted() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let mut catalog = Catalog::open(&mut bpm).unwrap();
    let table = catalog.create_table(&mut bpm, "people", people()).unwrap();
    let mut heap = HeapFile::open(&mut bpm, table.first_page_id).unwrap();
    // Written below the schema, as damage that the page checksum cannot see would look.
    let id = 7_i32.to_le_bytes();
    let day_after_the_range = 2_932_897_i32.to_le_bytes();
    let tuple = table
        .schema
        .layout()
        .encode(&[Some(&id[..]), Some(&day_after_the_range[..]), None])
        .unwrap();
    let rid = heap.insert(&mut bpm, &tuple).unwrap();

    let stored = heap.get(&mut bpm, rid).unwrap();
    let err = table.schema.decode_row(&stored).unwrap_err();

    assert_eq!(
        err,
        SchemaError::Type {
            column: "born".to_string(),
            source: TypeError::CorruptedValue {
                ty: DataType::Date,
                reason: "date out of range",
            },
        }
    );
    assert_eq!(err.sqlstate(), "XX001");
}
