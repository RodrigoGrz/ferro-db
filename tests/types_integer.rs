use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::config::PAGE_SIZE;
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::storage::rid::Rid;
use ferro_db::storage::slotted_page::SlottedPage;
use ferro_db::storage::tuple::{TupleLayout, TupleRef};
use ferro_db::types::{DataType, TypeError, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-integer-{}-{id}.db", std::process::id())))
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

const SCHEMA: [DataType; 3] = [DataType::SmallInt, DataType::Integer, DataType::BigInt];

type Row = [Value; 3];

const ROWS: [Row; 6] = [
    [
        Value::SmallInt(i16::MIN),
        Value::Integer(i32::MIN),
        Value::BigInt(i64::MIN),
    ],
    [
        Value::SmallInt(i16::MAX),
        Value::Integer(i32::MAX),
        Value::BigInt(i64::MAX),
    ],
    [Value::Null, Value::Integer(-1), Value::BigInt(0)],
    [Value::SmallInt(0), Value::Null, Value::BigInt(1)],
    [Value::SmallInt(-1), Value::Integer(0), Value::Null],
    [Value::Null, Value::Null, Value::Null],
];

fn layout() -> TupleLayout {
    TupleLayout::new(SCHEMA.iter().map(|ty| ty.layout()).collect()).unwrap()
}

fn encode(row: &Row) -> Vec<u8> {
    let fields: Vec<_> = row.iter().map(Value::encode).collect();
    let borrowed: Vec<_> = fields.iter().map(Option::as_deref).collect();
    layout().encode(&borrowed).unwrap()
}

fn decode(bytes: &[u8]) -> Vec<Value> {
    let layout = layout();
    let tuple = TupleRef::new(&layout, bytes).unwrap();
    SCHEMA
        .iter()
        .zip(tuple.fields())
        .map(|(&ty, field)| Value::decode(ty, field).unwrap())
        .collect()
}

fn insert_rows(bpm: &mut BufferPoolManager, page_id: u32) -> Vec<Rid> {
    let page = bpm.fetch_page(page_id).unwrap();
    let mut slotted = SlottedPage::init(page.data_mut()).unwrap();
    let rids = ROWS
        .iter()
        .map(|row| Rid::new(page_id, slotted.insert(&encode(row)).unwrap()))
        .collect();
    assert!(bpm.unpin_page(page_id, true));
    rids
}

fn read_row(bpm: &mut BufferPoolManager, rid: Rid) -> Vec<Value> {
    let page = bpm.fetch_page(rid.page_id).unwrap();
    let row = decode(
        SlottedPage::new(page.data())
            .unwrap()
            .get(rid.slot_id)
            .unwrap(),
    );
    assert!(bpm.unpin_page(rid.page_id, false));
    row
}

#[test]
fn integer_rows_should_survive_eviction() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let rids = insert_rows(&mut bpm, 1);

    for page_id in 100..102 {
        bpm.fetch_page(page_id).unwrap();
        bpm.unpin_page(page_id, false);
    }

    for (rid, row) in rids.into_iter().zip(ROWS) {
        assert_eq!(read_row(&mut bpm, rid), row);
    }
}

#[test]
fn integer_rows_should_survive_reopen() {
    let db = TempDb::new();
    let rids = {
        let mut bpm = db.open(2);
        insert_rows(&mut bpm, 0)
    };

    let mut bpm = db.open(2);

    for (rid, row) in rids.into_iter().zip(ROWS) {
        assert_eq!(read_row(&mut bpm, rid), row);
    }
}

#[test]
fn null_fields_should_only_set_the_bitmap() {
    let zeroed_fixed_part = vec![0; 2 + 4 + 8];

    let all_null = encode(&[Value::Null; 3]);

    assert_eq!(all_null.len(), layout().min_tuple_size());
    assert_eq!(all_null[..3], [3, 0, 0b111]);
    assert_eq!(all_null[3..], zeroed_fixed_part);
}

#[test]
fn decoding_a_field_with_the_wrong_type_should_fail() {
    let bytes = encode(&ROWS[0]);
    let layout = layout();
    let tuple = TupleRef::new(&layout, &bytes).unwrap();
    let smallint_field = tuple.field(0).unwrap();

    let err = Value::decode(DataType::Integer, smallint_field).unwrap_err();

    assert_eq!(
        err,
        TypeError::InvalidEncoding {
            ty: DataType::Integer,
            expected: 4,
            actual: 2
        }
    );
}
