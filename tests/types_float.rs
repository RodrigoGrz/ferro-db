use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::Catalog;
use ferro_db::config::PAGE_SIZE;
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::storage::heap_file::HeapFile;
use ferro_db::table::{Column, Schema, SchemaError};
use ferro_db::types::{DataType, Finite32, Finite64, TypeError, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-float-{}-{id}.db", std::process::id())))
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

fn real(value: f32) -> Value {
    Value::Real(Finite32::new(value).unwrap())
}

fn double(value: f64) -> Value {
    Value::Double(Finite64::new(value).unwrap())
}

fn measurements() -> Schema {
    Schema::new(vec![
        Column::not_null("id", DataType::Integer),
        Column::not_null("price", DataType::Double),
        Column::new("ratio", DataType::Real),
    ])
    .unwrap()
}

/// Enough rows for several pages, with the limits of both types, subnormals, zero and NULL.
fn measurement_rows() -> Vec<Vec<Value>> {
    let double_limits = [
        f64::MAX,
        -f64::MAX,
        f64::MIN_POSITIVE,
        f64::from_bits(1),
        0.0,
    ];
    let real_limits = [
        f32::MAX,
        -f32::MAX,
        f32::MIN_POSITIVE,
        f32::from_bits(1),
        0.0,
    ];
    (0..2_000)
        .map(|id: i32| {
            let index = usize::try_from(id).unwrap();
            let price = match double_limits.get(index) {
                Some(&limit) => double(limit),
                None => double(f64::from(id) * 0.01),
            };
            let ratio = match (real_limits.get(index), id % 4) {
                (Some(&limit), _) => real(limit),
                (None, 0) => Value::Null,
                (None, _) => real(id as f32 / 3.0),
            };
            vec![Value::Integer(id), price, ratio]
        })
        .collect()
}

fn create_measurements(bpm: &mut BufferPoolManager) -> Catalog {
    let mut catalog = Catalog::open(bpm).unwrap();
    catalog
        .create_table(bpm, "measurements", measurements())
        .unwrap();
    let table = catalog.table("measurements").unwrap();
    let mut heap = HeapFile::open(bpm, table.first_page_id).unwrap();
    for row in measurement_rows() {
        heap.insert(bpm, &table.schema.encode_row(&row).unwrap())
            .unwrap();
    }
    catalog
}

/// Sorted by `id`: the scan follows pages, not insertion order.
fn read_rows(bpm: &mut BufferPoolManager, catalog: &Catalog) -> Vec<Vec<Value>> {
    let table = catalog.table("measurements").unwrap();
    let heap = HeapFile::open(bpm, table.first_page_id).unwrap();
    let mut rows: Vec<Vec<Value>> = heap
        .scan(bpm)
        .map(|entry| table.schema.decode_row(&entry.unwrap().1).unwrap())
        .collect();
    rows.sort_by(|lhs, rhs| lhs[0].sort_cmp(&rhs[0]));
    rows
}

#[test]
fn float_rows_should_survive_eviction() {
    let db = TempDb::new();
    let mut bpm = db.open(2);

    let catalog = create_measurements(&mut bpm);

    let first_page_id = catalog.table("measurements").unwrap().first_page_id;
    assert!(
        HeapFile::open(&mut bpm, first_page_id)
            .unwrap()
            .page_count()
            > 2
    );
    assert_eq!(read_rows(&mut bpm, &catalog), measurement_rows());
}

#[test]
fn float_rows_and_columns_should_survive_reopening_the_file() {
    let db = TempDb::new();
    {
        let mut bpm = db.open(2);
        create_measurements(&mut bpm);
    }

    let mut bpm = db.open(2);
    let catalog = Catalog::open(&mut bpm).unwrap();

    assert_eq!(
        catalog.table("measurements").unwrap().schema,
        measurements()
    );
    assert_eq!(read_rows(&mut bpm, &catalog), measurement_rows());
}

#[test]
fn integers_assigned_to_float_columns_should_come_back_as_floats() {
    let schema = measurements();

    let tuple = schema
        .encode_row(&[
            Value::Integer(1),
            Value::Integer(10),
            Value::BigInt(16_777_217),
        ])
        .unwrap();

    assert_eq!(
        schema.decode_row(&tuple).unwrap(),
        [Value::Integer(1), double(10.0), real(16_777_216.0)]
    );
}

#[test]
fn a_stored_nan_should_be_corrupted() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let mut catalog = Catalog::open(&mut bpm).unwrap();
    let table = catalog
        .create_table(&mut bpm, "measurements", measurements())
        .unwrap();
    let mut heap = HeapFile::open(&mut bpm, table.first_page_id).unwrap();
    // Written below the schema, as damage that the page checksum cannot see would look.
    let id = 7_i32.to_le_bytes();
    let nan = f64::NAN.to_le_bytes();
    let tuple = table
        .schema
        .layout()
        .encode(&[Some(&id[..]), Some(&nan[..]), None])
        .unwrap();
    let rid = heap.insert(&mut bpm, &tuple).unwrap();

    let stored = heap.get(&mut bpm, rid).unwrap();
    let err = table.schema.decode_row(&stored).unwrap_err();

    assert_eq!(
        err,
        SchemaError::Type {
            column: "price".to_string(),
            source: TypeError::CorruptedValue {
                ty: DataType::Double,
                reason: "float must be finite",
            },
        }
    );
    assert_eq!(err.sqlstate(), "XX001");
}
