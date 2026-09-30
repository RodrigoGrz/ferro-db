use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::config::PAGE_SIZE;
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::storage::rid::Rid;
use ferro_db::storage::slotted_page::{SlottedPage, SlottedPageError};
use ferro_db::storage::tuple::{ColumnLayout, TupleError, TupleLayout, TupleRef};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-tuple-{}-{id}.db", std::process::id())))
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

type Row = (u32, Option<&'static str>, Option<u8>);

/// `(id INTEGER, name VARCHAR, age SMALLINT-ish)` as raw column widths.
fn people_layout() -> TupleLayout {
    TupleLayout::new(vec![
        ColumnLayout::Fixed(4),
        ColumnLayout::Variable,
        ColumnLayout::Fixed(1),
    ])
    .unwrap()
}

fn encode(layout: &TupleLayout, (id, name, age): Row) -> Vec<u8> {
    let id = id.to_le_bytes();
    let age = age.map(|age| [age]);
    layout
        .encode(&[
            Some(&id),
            name.map(str::as_bytes),
            age.as_ref().map(|a| &a[..]),
        ])
        .unwrap()
}

fn decode(layout: &TupleLayout, bytes: &[u8]) -> (u32, Option<String>, Option<u8>) {
    let tuple = TupleRef::new(layout, bytes).unwrap();
    let id = u32::from_le_bytes(tuple.field(0).unwrap().unwrap().try_into().unwrap());
    let name = tuple
        .field(1)
        .unwrap()
        .map(|bytes| String::from_utf8(bytes.to_vec()).unwrap());
    let age = tuple.field(2).unwrap().map(|bytes| bytes[0]);
    (id, name, age)
}

fn insert_rows(bpm: &mut BufferPoolManager, page_id: u32, rows: &[Row]) -> Vec<Rid> {
    let layout = people_layout();
    let page = bpm.fetch_page(page_id).unwrap();
    let mut slotted = SlottedPage::init(page.data_mut()).unwrap();
    let rids = rows
        .iter()
        .map(|&row| Rid::new(page_id, slotted.insert(&encode(&layout, row)).unwrap()))
        .collect();
    assert!(bpm.unpin_page(page_id, true));
    rids
}

fn read_row(bpm: &mut BufferPoolManager, rid: Rid) -> (u32, Option<String>, Option<u8>) {
    let page = bpm.fetch_page(rid.page_id).unwrap();
    let row = decode(
        &people_layout(),
        SlottedPage::new(page.data())
            .unwrap()
            .get(rid.slot_id)
            .unwrap(),
    );
    assert!(bpm.unpin_page(rid.page_id, false));
    row
}

fn owned(row: Row) -> (u32, Option<String>, Option<u8>) {
    (row.0, row.1.map(String::from), row.2)
}

const ROWS: [Row; 3] = [
    (1, Some("alice"), Some(30)),
    (2, None, Some(41)),
    (3, Some(""), None),
];

#[test]
fn tuples_should_survive_eviction() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let rids = insert_rows(&mut bpm, 1, &ROWS);

    for page_id in 100..102 {
        bpm.fetch_page(page_id).unwrap();
        bpm.unpin_page(page_id, false);
    }

    for (rid, row) in rids.into_iter().zip(ROWS) {
        assert_eq!(read_row(&mut bpm, rid), owned(row));
    }
}

#[test]
fn tuples_should_survive_reopen() {
    let db = TempDb::new();
    let rids = {
        let mut bpm = db.open(2);
        insert_rows(&mut bpm, 0, &ROWS)
    };

    let mut bpm = db.open(2);

    for (rid, row) in rids.into_iter().zip(ROWS) {
        assert_eq!(read_row(&mut bpm, rid), owned(row));
    }
}

#[test]
fn update_with_larger_variable_field_should_keep_slot_id() {
    let db = TempDb::new();
    let layout = people_layout();
    let renamed = (2, Some("bartholomew the second"), Some(41));
    let rids = {
        let mut bpm = db.open(2);
        let rids = insert_rows(&mut bpm, 0, &ROWS);
        let page = bpm.fetch_page(0).unwrap();
        SlottedPage::new(page.data_mut())
            .unwrap()
            .update(rids[1].slot_id, &encode(&layout, renamed))
            .unwrap();
        bpm.unpin_page(0, true);
        rids
    };

    let mut bpm = db.open(2);

    assert_eq!(read_row(&mut bpm, rids[1]), owned(renamed));
    assert_eq!(read_row(&mut bpm, rids[0]), owned(ROWS[0]));
    assert_eq!(read_row(&mut bpm, rids[2]), owned(ROWS[2]));
}

#[test]
fn page_full_of_tuples_should_fail_with_page_full_and_keep_every_row() {
    let layout = people_layout();
    let mut page = vec![0; PAGE_SIZE];
    let mut slotted = SlottedPage::init(page.as_mut_slice()).unwrap();
    let row: Row = (
        7,
        Some("a name long enough to fill the page quickly"),
        Some(1),
    );
    let tuple = encode(&layout, row);

    let mut inserted = 0;
    let err = loop {
        match slotted.insert(&tuple) {
            Ok(_) => inserted += 1,
            Err(err) => break err,
        }
    };

    assert_eq!(err, SlottedPageError::PageFull);
    assert!(inserted > 0);
    for (_, bytes) in slotted.iter() {
        assert_eq!(decode(&layout, bytes), owned(row));
    }
    assert_eq!(slotted.iter().count(), inserted);
}

#[test]
fn tuple_larger_than_a_page_should_fail_on_insert() {
    let layout = TupleLayout::new(vec![ColumnLayout::Variable]).unwrap();
    let mut page = vec![0; PAGE_SIZE];
    let mut slotted = SlottedPage::init(page.as_mut_slice()).unwrap();
    let big = vec![b'x'; slotted.max_tuple_size()];

    let tuple = layout.encode(&[Some(&big)]).unwrap();
    let err = slotted.insert(&tuple).unwrap_err();

    assert_eq!(
        err,
        SlottedPageError::TupleTooLarge {
            size: tuple.len(),
            max: slotted.max_tuple_size()
        }
    );
    assert_eq!(slotted.slot_count(), 0);
}

#[test]
fn tuple_read_with_the_wrong_layout_should_fail() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let rids = insert_rows(&mut bpm, 0, &ROWS);
    let two_columns =
        TupleLayout::new(vec![ColumnLayout::Fixed(4), ColumnLayout::Variable]).unwrap();

    let page = bpm.fetch_page(0).unwrap();
    let slotted = SlottedPage::new(page.data()).unwrap();
    let bytes = slotted.get(rids[0].slot_id).unwrap();

    assert_eq!(
        TupleRef::new(&two_columns, bytes).unwrap_err(),
        TupleError::ColumnCountMismatch {
            expected: 2,
            actual: 3
        }
    );
}
