use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::catalog::{Catalog, CatalogError, DatabaseHeader};
use ferro_db::config::PAGE_SIZE;
use ferro_db::storage::buffer_pool::{BufferPoolError, BufferPoolManager};
use ferro_db::storage::heap_file::{HeapFile, HeapFileError};
use ferro_db::storage::page::RESERVED_PAGE_ID;
use ferro_db::storage::slotted_page::SlottedPage;
use ferro_db::table::{Column, Schema};
use ferro_db::types::{DataType, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-catalog-it-{}-{id}.db", std::process::id())))
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

fn accounts() -> Schema {
    Schema::new(vec![
        Column::not_null("id", DataType::Integer),
        Column::new("balance", DataType::BigInt),
    ])
    .unwrap()
}

fn products() -> Schema {
    Schema::new(vec![
        Column::not_null("sku", DataType::BigInt),
        Column::new("stock", DataType::SmallInt),
        Column::new("price", DataType::Integer),
    ])
    .unwrap()
}

fn account_rows() -> Vec<Vec<Value>> {
    (0..500)
        .map(|id| vec![Value::Integer(id), Value::BigInt(i64::from(id) * 10)])
        .collect()
}

fn product_rows() -> Vec<Vec<Value>> {
    (0..300)
        .map(|sku| {
            let stock = if sku % 7 == 0 {
                Value::Null
            } else {
                Value::SmallInt((sku % 50) as i16)
            };
            vec![Value::BigInt(sku), stock, Value::Integer(sku as i32 * 3)]
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

fn read_rows(bpm: &mut BufferPoolManager, catalog: &Catalog, table: &str) -> Vec<Vec<Value>> {
    let table = catalog.table(table).unwrap();
    let heap = HeapFile::open(bpm, table.first_page_id).unwrap();
    heap.scan(bpm)
        .map(|entry| table.schema.decode_row(&entry.unwrap().1).unwrap())
        .collect()
}

#[test]
fn tables_and_their_rows_should_survive_reopening_the_file() {
    let db = TempDb::new();
    {
        let mut bpm = db.open(2);
        let mut catalog = Catalog::open(&mut bpm).unwrap();
        catalog
            .create_table(&mut bpm, "accounts", accounts())
            .unwrap();
        catalog
            .create_table(&mut bpm, "products", products())
            .unwrap();
        insert_rows(&mut bpm, &catalog, "accounts", &account_rows());
        insert_rows(&mut bpm, &catalog, "products", &product_rows());
    }

    let mut bpm = db.open(2);
    let catalog = Catalog::open(&mut bpm).unwrap();

    let names: Vec<&str> = catalog.tables().iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["accounts", "products"]);
    assert_eq!(catalog.table("products").unwrap().schema, products());
    assert_eq!(read_rows(&mut bpm, &catalog, "accounts"), account_rows());
    assert_eq!(read_rows(&mut bpm, &catalog, "products"), product_rows());
}

#[test]
fn a_file_whose_page_0_is_not_a_database_header_should_not_open() {
    let db = TempDb::new();
    {
        let mut bpm = db.open(2);
        let page = bpm.fetch_page(RESERVED_PAGE_ID).unwrap();
        SlottedPage::init(page.data_mut()).unwrap();
        bpm.unpin_page(RESERVED_PAGE_ID, true);
    }

    let mut bpm = db.open(2);
    let err = Catalog::open(&mut bpm).unwrap_err();

    assert!(matches!(err, CatalogError::NotADatabase { .. }));
}

#[test]
fn corruption_in_the_columns_heap_should_be_detected_on_open() {
    let db = TempDb::new();
    let columns_page = {
        let mut bpm = db.open(2);
        let mut catalog = Catalog::open(&mut bpm).unwrap();
        catalog
            .create_table(&mut bpm, "accounts", accounts())
            .unwrap();
        let header = DatabaseHeader::read(bpm.fetch_page(RESERVED_PAGE_ID).unwrap().data())
            .unwrap()
            .unwrap();
        bpm.unpin_page(RESERVED_PAGE_ID, false);
        header.columns_first_page_id
    };
    db.flip_byte(columns_page, PAGE_SIZE - 10);

    let mut bpm = db.open(2);
    let err = Catalog::open(&mut bpm).unwrap_err();

    assert!(matches!(
        err,
        CatalogError::Heap(HeapFileError::BufferPool(BufferPoolError::ChecksumMismatch { page_id }))
            if page_id == columns_page
    ));
}
