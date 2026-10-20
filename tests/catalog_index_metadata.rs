use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::{Catalog, CatalogError, DatabaseHeader};
use ferro_db::config::PAGE_SIZE;
use ferro_db::index::IndexError;
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::storage::page::RESERVED_PAGE_ID;
use ferro_db::table::{Column, Schema};
use ferro_db::types::{DataType, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "ferro-index-metadata-it-{}-{id}.db",
            std::process::id()
        )))
    }

    fn open(&self) -> BufferPoolManager {
        BufferPoolManager::new(8, PAGE_SIZE, &self.0).unwrap()
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

fn accounts() -> Schema {
    Schema::new(vec![
        Column::not_null("id", DataType::Integer),
        Column::new("balance", DataType::BigInt),
    ])
    .unwrap()
}

fn users() -> Schema {
    Schema::new(vec![
        Column::not_null("email", DataType::Varchar(NonZeroU16::new(80).unwrap())),
        Column::new("age", DataType::SmallInt),
    ])
    .unwrap()
}

fn email(id: usize) -> String {
    format!("user{:05}@example.com", (id * 7_919) % 3_000)
}

/// 2,000 accounts and 3,000 users, with a balance that repeats and ages that are sometimes NULL.
fn fill(bpm: &mut BufferPoolManager, catalog: &mut Catalog) {
    catalog.create_table(bpm, "accounts", accounts()).unwrap();
    catalog.create_table(bpm, "users", users()).unwrap();
    for id in 0..2_000 {
        let row = [Value::Integer(id), Value::BigInt(i64::from(id % 10))];
        let heap = catalog.table_heap(bpm, "accounts").unwrap();
        heap.insert(bpm, &row).unwrap();
    }
    for id in 0..3_000 {
        let age = if id % 4 == 0 {
            Value::Null
        } else {
            Value::SmallInt(id as i16)
        };
        let row = [Value::Text(email(id)), age];
        let heap = catalog.table_heap(bpm, "users").unwrap();
        heap.insert(bpm, &row).unwrap();
    }
}

/// The row an index points to for `key`, read back from the table.
fn row_by_key(
    bpm: &mut BufferPoolManager,
    catalog: &mut Catalog,
    index: &str,
    table: &str,
    key: &Value,
) -> Option<Vec<Value>> {
    let rid = catalog
        .index_tree(bpm, index)
        .unwrap()
        .get(bpm, key)
        .unwrap()?;
    Some(
        catalog
            .table_heap(bpm, table)
            .unwrap()
            .get(bpm, rid)
            .unwrap(),
    )
}

#[test]
fn indexes_should_survive_reopening_and_lead_to_their_rows() {
    let db = TempDb::new();
    {
        let mut bpm = db.open();
        let mut catalog = Catalog::open(&mut bpm).unwrap();
        fill(&mut bpm, &mut catalog);
        for (name, table, column) in [
            ("accounts_id", "accounts", "id"),
            ("users_email", "users", "email"),
            ("users_age", "users", "age"),
        ] {
            catalog.create_index(&mut bpm, name, table, column).unwrap();
        }
    }

    let mut bpm = db.open();
    let mut catalog = Catalog::open(&mut bpm).unwrap();

    let names: Vec<&str> = catalog
        .indexes()
        .iter()
        .map(|index| index.name.as_str())
        .collect();
    assert_eq!(names, ["accounts_id", "users_email", "users_age"]);
    assert_eq!(catalog.table_indexes("users").unwrap().len(), 2);
    for id in (0..2_000).step_by(97) {
        let row = row_by_key(
            &mut bpm,
            &mut catalog,
            "accounts_id",
            "accounts",
            &Value::Integer(id),
        );
        assert_eq!(row.unwrap()[0], Value::Integer(id));
    }
    for id in (0..3_000).step_by(89) {
        let key = Value::Text(email(id));
        let row = row_by_key(&mut bpm, &mut catalog, "users_email", "users", &key).unwrap();
        assert_eq!(row[0], key);
    }
    let missing = row_by_key(
        &mut bpm,
        &mut catalog,
        "users_age",
        "users",
        &Value::SmallInt(4),
    );
    assert!(
        missing.is_none(),
        "age 4 belongs to a row whose age is NULL"
    );
}

#[test]
fn a_failed_index_should_leave_no_trace_after_reopening() {
    let db = TempDb::new();
    {
        let mut bpm = db.open();
        let mut catalog = Catalog::open(&mut bpm).unwrap();
        fill(&mut bpm, &mut catalog);

        let err = catalog
            .create_index(&mut bpm, "accounts_balance", "accounts", "balance")
            .unwrap_err();

        assert!(
            matches!(err, CatalogError::Index(IndexError::DuplicateKey { .. })),
            "{err:?}"
        );
        assert_eq!(err.sqlstate(), "23505");
    }

    let mut bpm = db.open();
    let mut catalog = Catalog::open(&mut bpm).unwrap();

    assert!(catalog.indexes().is_empty());
    catalog
        .create_index(&mut bpm, "accounts_balance", "accounts", "id")
        .unwrap();
}

#[test]
fn a_database_from_before_indexes_should_take_its_first_index() {
    let db = TempDb::new();
    {
        let mut bpm = db.open();
        let mut catalog = Catalog::open(&mut bpm).unwrap();
        fill(&mut bpm, &mut catalog);
    }
    // A database written before indexes existed has these bytes zeroed, like this one.
    let header = {
        let mut bpm = db.open();
        let header = DatabaseHeader::read(bpm.fetch_page(RESERVED_PAGE_ID).unwrap().data())
            .unwrap()
            .unwrap();
        bpm.unpin_page(RESERVED_PAGE_ID, false);
        header
    };
    assert_eq!(header.indexes_first_page_id, 0);

    let mut bpm = db.open();
    let mut catalog = Catalog::open(&mut bpm).unwrap();
    catalog
        .create_index(&mut bpm, "accounts_id", "accounts", "id")
        .unwrap();
    drop(catalog);
    drop(bpm);

    let mut bpm = db.open();
    let catalog = Catalog::open(&mut bpm).unwrap();
    assert_eq!(catalog.index("accounts_id").unwrap().column, 0);
}

#[test]
fn a_damaged_page_of_the_indexes_heap_should_be_detected_on_open() {
    let db = TempDb::new();
    let indexes_page = {
        let mut bpm = db.open();
        let mut catalog = Catalog::open(&mut bpm).unwrap();
        fill(&mut bpm, &mut catalog);
        catalog
            .create_index(&mut bpm, "accounts_id", "accounts", "id")
            .unwrap();
        drop(catalog);
        let header = DatabaseHeader::read(bpm.fetch_page(RESERVED_PAGE_ID).unwrap().data())
            .unwrap()
            .unwrap();
        bpm.unpin_page(RESERVED_PAGE_ID, false);
        header.indexes_first_page_id
    };
    db.flip_byte(indexes_page, PAGE_SIZE - 10);

    let mut bpm = db.open();
    let err = Catalog::open(&mut bpm).unwrap_err();

    assert_eq!(err.sqlstate(), "XX001", "{err:?}");
}
