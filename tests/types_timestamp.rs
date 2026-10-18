use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::Catalog;
use ferro_db::config::PAGE_SIZE;
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::storage::heap_file::HeapFile;
use ferro_db::table::{Column, Schema, SchemaError};
use ferro_db::types::{DataType, Date, Timestamp, TypeError, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-timestamp-{}-{id}.db", std::process::id())))
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

fn instant(micros: i64) -> Timestamp {
    Timestamp::from_micros_since_epoch(micros).unwrap()
}

fn tickets() -> Schema {
    Schema::new(vec![
        Column::not_null("id", DataType::Integer),
        Column::not_null("created", DataType::Timestamp),
        Column::new("closed", DataType::Timestamp),
        Column::new("due", DataType::Date),
    ])
    .unwrap()
}

/// Enough rows for several pages: the range ends, instants before 1970, fractions, NULL.
/// `due` is three days after the day of `created`, so it is always later.
fn ticket_rows() -> Vec<Vec<Value>> {
    let special = [
        Timestamp::MIN,
        Timestamp::MAX,
        instant(-1),
        instant(0),
        instant(1_710_513_000_250_000),
    ];
    (0..2_000)
        .map(|id: i32| {
            let index = usize::try_from(id).unwrap();
            let created = special
                .get(index)
                .copied()
                .unwrap_or_else(|| instant(i64::from(id - 1_000) * 3_600_000_123));
            let closed = match id % 3 {
                0 => Value::Null,
                _ => Value::Timestamp(created),
            };
            let due = Value::Date(created.date())
                .add(&Value::Integer(3))
                .unwrap_or(Value::Null);
            vec![Value::Integer(id), Value::Timestamp(created), closed, due]
        })
        .collect()
}

fn create_tickets(bpm: &mut BufferPoolManager) -> Catalog {
    let mut catalog = Catalog::open(bpm).unwrap();
    catalog.create_table(bpm, "tickets", tickets()).unwrap();
    let table = catalog.table("tickets").unwrap();
    let mut heap = HeapFile::open(bpm, table.first_page_id).unwrap();
    for row in ticket_rows() {
        heap.insert(bpm, &table.schema.encode_row(&row).unwrap())
            .unwrap();
    }
    catalog
}

/// Sorted by `id`: the scan follows pages, not insertion order.
fn read_rows(bpm: &mut BufferPoolManager, catalog: &Catalog) -> Vec<Vec<Value>> {
    let table = catalog.table("tickets").unwrap();
    let heap = HeapFile::open(bpm, table.first_page_id).unwrap();
    let mut rows: Vec<Vec<Value>> = heap
        .scan(bpm)
        .map(|entry| table.schema.decode_row(&entry.unwrap().1).unwrap())
        .collect();
    rows.sort_by(|lhs, rhs| lhs[0].sort_cmp(&rhs[0]));
    rows
}

#[test]
fn timestamp_rows_should_survive_eviction() {
    let db = TempDb::new();
    let mut bpm = db.open(2);

    let catalog = create_tickets(&mut bpm);

    let first_page_id = catalog.table("tickets").unwrap().first_page_id;
    assert!(
        HeapFile::open(&mut bpm, first_page_id)
            .unwrap()
            .page_count()
            > 2
    );
    assert_eq!(read_rows(&mut bpm, &catalog), ticket_rows());
}

#[test]
fn timestamp_rows_and_columns_should_survive_reopening_the_file() {
    let db = TempDb::new();
    {
        let mut bpm = db.open(2);
        create_tickets(&mut bpm);
    }

    let mut bpm = db.open(2);
    let catalog = Catalog::open(&mut bpm).unwrap();

    assert_eq!(catalog.table("tickets").unwrap().schema, tickets());
    assert_eq!(read_rows(&mut bpm, &catalog), ticket_rows());
}

#[test]
fn stored_timestamps_should_compare_with_stored_dates() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let catalog = create_tickets(&mut bpm);
    let rows = read_rows(&mut bpm, &catalog);

    let with_due: Vec<&Vec<Value>> = rows.iter().filter(|row| !row[3].is_null()).collect();
    let created_before_due = with_due
        .iter()
        .filter(|row| row[1].sql_lt(&row[3]).unwrap().is_true().unwrap())
        .count();
    let midnight_of_due: Vec<Value> = with_due
        .iter()
        .map(|row| row[3].cast(DataType::Timestamp).unwrap())
        .collect();

    assert!(with_due.len() > 1_900);
    assert_eq!(created_before_due, with_due.len());
    assert!(
        with_due
            .iter()
            .zip(&midnight_of_due)
            .all(|(row, midnight)| row[3].sql_eq(midnight) == Ok(Value::Boolean(true)))
    );
    assert_eq!(
        Value::Date(Date::MAX).compare(&Value::Timestamp(Timestamp::MAX)),
        Ok(Some(std::cmp::Ordering::Less))
    );
}

#[test]
fn a_stored_instant_outside_the_range_should_be_corrupted() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let mut catalog = Catalog::open(&mut bpm).unwrap();
    let table = catalog
        .create_table(&mut bpm, "tickets", tickets())
        .unwrap();
    let mut heap = HeapFile::open(&mut bpm, table.first_page_id).unwrap();
    // Written below the schema, as damage that the page checksum cannot see would look.
    let id = 7_i32.to_le_bytes();
    let after_the_range = (Timestamp::MAX.micros_since_epoch() + 1).to_le_bytes();
    let tuple = table
        .schema
        .layout()
        .encode(&[Some(&id[..]), Some(&after_the_range[..]), None, None])
        .unwrap();
    let rid = heap.insert(&mut bpm, &tuple).unwrap();

    let stored = heap.get(&mut bpm, rid).unwrap();
    let err = table.schema.decode_row(&stored).unwrap_err();

    assert_eq!(
        err,
        SchemaError::Type {
            column: "created".to_string(),
            source: TypeError::CorruptedValue {
                ty: DataType::Timestamp,
                reason: "timestamp out of range",
            },
        }
    );
    assert_eq!(err.sqlstate(), "XX001");
}
