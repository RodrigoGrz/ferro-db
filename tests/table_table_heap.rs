use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::{Catalog, CatalogError};
use ferro_db::config::PAGE_SIZE;
use ferro_db::storage::buffer_pool::{BufferPoolError, BufferPoolManager};
use ferro_db::storage::heap_file::HeapFileError;
use ferro_db::storage::rid::Rid;
use ferro_db::table::{Column, Schema, TableError};
use ferro_db::types::{DataType, Date, Finite32, Finite64, Timestamp, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-table-it-{}-{id}.db", std::process::id())))
    }

    fn open(&self, pool_size: usize) -> BufferPoolManager {
        BufferPoolManager::new(pool_size, PAGE_SIZE, &self.0).unwrap()
    }

    fn flip_byte(&self, page_id: u32, offset: usize) {
        let mut file = OpenOptions::new().write(true).open(&self.0).unwrap();
        file.seek(SeekFrom::Start(
            u64::from(page_id) * PAGE_SIZE as u64 + offset as u64,
        ))
        .unwrap();
        file.write_all(&[0xA5]).unwrap();
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

const TITLE_MAX_CHARS: u16 = 200;

fn events() -> Schema {
    Schema::new(vec![
        Column::not_null("id", DataType::Integer),
        Column::new(
            "title",
            DataType::Varchar(NonZeroU16::new(TITLE_MAX_CHARS).unwrap()),
        ),
        Column::new("done", DataType::Boolean),
        Column::new("day", DataType::Date),
        Column::new("at", DataType::Timestamp),
        Column::new("score", DataType::Double),
        Column::new("weight", DataType::Real),
    ])
    .unwrap()
}

/// Every type in one row, with a NULL somewhere in every fifth one.
fn event(id: i32, title: &str) -> Vec<Value> {
    let n = id.unsigned_abs();
    let day = Date::from_ymd(2024, 1 + n % 12, 1 + n % 28).unwrap();
    let at = Timestamp::new(day, n % 24, n % 60, (n * 7) % 60, (n * 7_919) % 1_000_000).unwrap();
    let mut row = vec![
        Value::Integer(id),
        Value::Text(title.to_string()),
        Value::Boolean(id % 2 == 0),
        Value::Date(day),
        Value::Timestamp(at),
        Value::Double(Finite64::new(f64::from(id) * 0.5).unwrap()),
        Value::Real(Finite32::new(id as f32 / 4.0).unwrap()),
    ];
    if id % 5 == 0 {
        row[1 + (n as usize / 5) % 6] = Value::Null;
    }
    row
}

fn create_events(bpm: &mut BufferPoolManager) -> Catalog {
    let mut catalog = Catalog::open(bpm).unwrap();
    catalog.create_table(bpm, "events", events()).unwrap();
    catalog
}

fn scanned(bpm: &mut BufferPoolManager, catalog: &mut Catalog) -> HashMap<Rid, Vec<Value>> {
    let heap = catalog.table_heap(bpm, "events").unwrap();
    heap.scan(bpm).map(|entry| entry.unwrap()).collect()
}

#[test]
fn rows_of_every_type_should_survive_updates_deletes_and_reopening() {
    let db = TempDb::new();
    let mut model: HashMap<Rid, Vec<Value>> = HashMap::new();
    let mut moved = 0;
    {
        let mut bpm = db.open(2);
        let mut catalog = create_events(&mut bpm);
        let heap = catalog.table_heap(&mut bpm, "events").unwrap();
        let rids: Vec<Rid> = (0..2_000)
            .map(|id| {
                let row = event(id, &format!("event {id}"));
                let rid = heap.insert(&mut bpm, &row).unwrap();
                model.insert(rid, row);
                rid
            })
            .collect();
        for (id, &rid) in rids.iter().enumerate().step_by(4) {
            let id = id as i32;
            let grown = event(id, &format!("{id} {}", "long title ".repeat(15)));
            let new_rid = heap.update(&mut bpm, rid, &grown).unwrap();
            moved += usize::from(new_rid != rid);
            model.remove(&rid);
            model.insert(new_rid, grown);
        }
        for (id, &rid) in rids.iter().enumerate().skip(1).step_by(7) {
            if model.remove(&rid).is_some() {
                heap.delete(&mut bpm, rid).unwrap();
            } else {
                assert_eq!(id % 4, 0, "only an updated row can have moved away");
            }
        }
        assert_eq!(scanned(&mut bpm, &mut catalog), model);
    }

    let mut bpm = db.open(2);
    let mut catalog = Catalog::open(&mut bpm).unwrap();
    let rows = scanned(&mut bpm, &mut catalog);

    let pages: HashSet<u32> = model.keys().map(|rid| rid.page_id).collect();
    assert!(pages.len() > 10, "the rows must span many pages");
    assert!(
        moved > 0,
        "some updates must move their row to another page"
    );
    assert_eq!(rows, model);
    let heap = catalog.table_heap(&mut bpm, "events").unwrap();
    for (&rid, row) in &model {
        assert_eq!(&heap.get(&mut bpm, rid).unwrap(), row);
    }
}

#[test]
fn rejected_rows_should_leave_no_trace_even_after_reopening() {
    let db = TempDb::new();
    let mut kept: HashMap<Rid, Vec<Value>> = HashMap::new();
    {
        let mut bpm = db.open(2);
        let mut catalog = create_events(&mut bpm);
        let heap = catalog.table_heap(&mut bpm, "events").unwrap();
        let rid = heap.insert(&mut bpm, &event(1, "first")).unwrap();
        kept.insert(rid, event(1, "first"));

        let too_long = "x".repeat(usize::from(TITLE_MAX_CHARS) + 1);
        let mut null_id = event(2, "second");
        null_id[0] = Value::Null;
        let mut text_in_boolean = event(3, "third");
        text_in_boolean[2] = Value::Text("maybe".to_string());
        let rejections = [
            heap.insert(&mut bpm, &event(2, &too_long)).unwrap_err(),
            heap.insert(&mut bpm, &null_id).unwrap_err(),
            heap.insert(&mut bpm, &text_in_boolean).unwrap_err(),
            heap.insert(&mut bpm, &event(4, "short")[..6]).unwrap_err(),
            heap.update(&mut bpm, rid, &null_id).unwrap_err(),
        ];

        let sqlstates: Vec<&str> = rejections.iter().map(TableError::sqlstate).collect();
        assert_eq!(sqlstates, ["22001", "23502", "42804", "42601", "23502"]);
        let rid = heap.insert(&mut bpm, &event(5, "after")).unwrap();
        kept.insert(rid, event(5, "after"));
        assert_eq!(scanned(&mut bpm, &mut catalog), kept);
    }

    let mut bpm = db.open(2);
    let mut catalog = Catalog::open(&mut bpm).unwrap();

    assert_eq!(scanned(&mut bpm, &mut catalog), kept);
    assert!(matches!(
        catalog.table_heap(&mut bpm, "missing").unwrap_err(),
        CatalogError::UndefinedTable { .. }
    ));
}

fn is_checksum_mismatch_on(err: &TableError, damaged_page: u32) -> bool {
    matches!(
        err,
        TableError::Heap(HeapFileError::BufferPool(BufferPoolError::ChecksumMismatch { page_id }))
            if *page_id == damaged_page
    )
}

#[test]
fn a_damaged_data_page_should_be_reported_as_corrupted() {
    let db = TempDb::new();
    let first_page_id = {
        let mut bpm = db.open(2);
        let mut catalog = create_events(&mut bpm);
        let heap = catalog.table_heap(&mut bpm, "events").unwrap();
        for id in 0..1_000 {
            heap.insert(&mut bpm, &event(id, "event")).unwrap();
        }
        // With 2 frames the first page was evicted long ago: the scan has to read it from disk.
        db.flip_byte(heap.first_page_id(), PAGE_SIZE - 10);

        let err = heap.scan(&mut bpm).find_map(Result::err).unwrap();

        assert!(
            is_checksum_mismatch_on(&err, heap.first_page_id()),
            "{err:?}"
        );
        assert_eq!(err.sqlstate(), "XX001");
        heap.first_page_id()
    };

    let mut bpm = db.open(2);
    let mut catalog = Catalog::open(&mut bpm).unwrap();
    let err = catalog.table_heap(&mut bpm, "events").unwrap_err();

    assert!(
        matches!(&err, CatalogError::Table(inner) if is_checksum_mismatch_on(inner, first_page_id)),
        "{err:?}"
    );
    assert_eq!(err.sqlstate(), "XX001");
}
