use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::buffer_pool::{BufferPoolError, BufferPoolManager};
use ferro_db::checksum;
use ferro_db::config::PAGE_SIZE;
use ferro_db::page::{
    PAGE_FORMAT_VERSION, PAGE_TYPE_OFFSET, PageError, PageHeader, PageType, VERSION_OFFSET,
};
use ferro_db::serialization::SerializationError;
use ferro_db::slotted_page::SlottedPage;

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-ser-{}-{id}.db", std::process::id())))
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

fn open_file(path: &Path, page_id: u32) -> fs::File {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.seek(SeekFrom::Start(page_id as u64 * PAGE_SIZE as u64))
        .unwrap();
    file
}

/// Edits a page directly on disk. With `restamp`, the checksum is recomputed so
/// only the header validation can catch the change.
fn edit_on_disk(path: &Path, page_id: u32, restamp: bool, edit: impl FnOnce(&mut [u8])) {
    let mut page = vec![0; PAGE_SIZE];
    open_file(path, page_id).read_exact(&mut page).unwrap();
    edit(&mut page);
    if restamp {
        checksum::stamp_page(&mut page);
    }
    open_file(path, page_id).write_all(&page).unwrap();
}

fn create_slotted_page(bpm: &mut BufferPoolManager, page_id: u32, lsn: u64) {
    let page = bpm.fetch_page(page_id).unwrap();
    page.set_lsn(lsn);
    SlottedPage::init(page.data_mut())
        .unwrap()
        .insert(b"alice")
        .unwrap();
    assert!(bpm.unpin_page(page_id, true));
}

fn read_header(bpm: &mut BufferPoolManager, page_id: u32) -> PageHeader {
    let header = PageHeader::read(bpm.fetch_page(page_id).unwrap().data()).unwrap();
    assert!(bpm.unpin_page(page_id, false));
    header
}

fn evict_all(bpm: &mut BufferPoolManager, pool_size: u32) {
    for page_id in 100..100 + pool_size {
        bpm.fetch_page(page_id).unwrap();
        bpm.unpin_page(page_id, false);
    }
}

#[test]
fn page_header_should_survive_eviction() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    create_slotted_page(&mut bpm, 1, 42);

    evict_all(&mut bpm, 2);

    let header = read_header(&mut bpm, 1);
    assert_eq!(header.page_type, PageType::Slotted);
    assert_eq!(header.version, PAGE_FORMAT_VERSION);
    assert_eq!(header.lsn, 42);
}

#[test]
fn page_header_should_survive_reopen_with_a_valid_checksum() {
    let db = TempDb::new();
    {
        let mut bpm = db.open(2);
        create_slotted_page(&mut bpm, 3, u64::MAX);
    }

    let mut bpm = db.open(2);

    let page = bpm.fetch_page(3).unwrap();
    let header = PageHeader::read(page.data()).unwrap();
    assert_eq!(header.page_type, PageType::Slotted);
    assert_eq!(header.lsn, u64::MAX);
    assert_eq!(header.checksum, checksum::page_checksum(page.data()));
    assert_eq!(
        SlottedPage::new(page.data()).unwrap().get(0).unwrap(),
        b"alice"
    );
}

#[test]
fn unknown_page_type_on_disk_should_be_rejected() {
    let db = TempDb::new();
    {
        let mut bpm = db.open(2);
        create_slotted_page(&mut bpm, 1, 0);
    }
    edit_on_disk(&db.0, 1, true, |page| page[PAGE_TYPE_OFFSET] = 7);
    let mut bpm = db.open(2);

    let err = bpm.fetch_page(1).unwrap_err();

    assert!(matches!(
        err,
        BufferPoolError::Page(PageError::Serialization(SerializationError::InvalidValue {
            field: "page_type",
            value: 7
        }))
    ));
}

#[test]
fn future_format_version_on_disk_should_be_rejected() {
    let db = TempDb::new();
    {
        let mut bpm = db.open(2);
        create_slotted_page(&mut bpm, 1, 0);
    }
    edit_on_disk(&db.0, 1, true, |page| {
        page[VERSION_OFFSET] = PAGE_FORMAT_VERSION + 1
    });
    let mut bpm = db.open(2);

    let err = bpm.fetch_page(1).unwrap_err();

    assert!(matches!(
        err,
        BufferPoolError::Page(PageError::UnsupportedVersion {
            expected: PAGE_FORMAT_VERSION,
            ..
        })
    ));
}

#[test]
fn rejected_page_should_leave_the_pool_usable() {
    let db = TempDb::new();
    {
        let mut bpm = db.open(1);
        create_slotted_page(&mut bpm, 1, 0);
    }
    edit_on_disk(&db.0, 1, true, |page| page[PAGE_TYPE_OFFSET] = 7);
    let mut bpm = db.open(1);

    assert!(bpm.fetch_page(1).is_err());

    create_slotted_page(&mut bpm, 2, 0);
    assert_eq!(read_header(&mut bpm, 2).page_type, PageType::Slotted);
}

#[test]
fn corrupted_page_type_byte_should_fail_on_checksum_first() {
    let db = TempDb::new();
    {
        let mut bpm = db.open(2);
        create_slotted_page(&mut bpm, 1, 0);
    }
    edit_on_disk(&db.0, 1, false, |page| page[PAGE_TYPE_OFFSET] = 7);
    let mut bpm = db.open(2);

    assert!(matches!(
        bpm.fetch_page(1).unwrap_err(),
        BufferPoolError::ChecksumMismatch { page_id: 1 }
    ));
}
