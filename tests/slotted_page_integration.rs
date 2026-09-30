use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::buffer_pool::{BufferPoolError, BufferPoolManager};
use ferro_db::config::PAGE_SIZE;
use ferro_db::page::COMMON_HEADER_SIZE;
use ferro_db::rid::Rid;
use ferro_db::slotted_page::{HEADER_SIZE, SlottedPage, SlottedPageError};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-it-{}-{id}.db", std::process::id())))
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

fn flip_byte(path: &Path, position: u64) {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut byte = [0];
    file.seek(SeekFrom::Start(position)).unwrap();
    file.read_exact(&mut byte).unwrap();
    file.seek(SeekFrom::Start(position)).unwrap();
    file.write_all(&[byte[0] ^ 0xFF]).unwrap();
}

fn insert_all(bpm: &mut BufferPoolManager, page_id: u32, tuples: &[&[u8]]) -> Vec<Rid> {
    let page = bpm.fetch_page(page_id).unwrap();
    let mut slotted = SlottedPage::init(page.data_mut()).unwrap();
    let rids = tuples
        .iter()
        .map(|tuple| Rid::new(page_id, slotted.insert(tuple).unwrap()))
        .collect();
    assert!(bpm.unpin_page(page_id, true));
    rids
}

fn read(bpm: &mut BufferPoolManager, rid: Rid) -> Result<Vec<u8>, SlottedPageError> {
    let page = bpm.fetch_page(rid.page_id).unwrap();
    let result = SlottedPage::new(page.data())
        .unwrap()
        .get(rid.slot_id)
        .map(<[u8]>::to_vec);
    assert!(bpm.unpin_page(rid.page_id, false));
    result
}

fn evict_all(bpm: &mut BufferPoolManager, pool_size: u32) {
    for page_id in 100..100 + pool_size {
        bpm.fetch_page(page_id).unwrap();
        bpm.unpin_page(page_id, false);
    }
}

#[test]
fn tuples_should_survive_eviction() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let tuples: [&[u8]; 3] = [b"alice", b"bob", b"carol"];
    let rids = insert_all(&mut bpm, 1, &tuples);

    evict_all(&mut bpm, 2);

    for (rid, expected) in rids.iter().zip(tuples) {
        assert_eq!(read(&mut bpm, *rid).unwrap(), expected);
    }
}

#[test]
fn tuples_should_survive_reopen() {
    let db = TempDb::new();
    let tuples: [&[u8]; 3] = [b"alice", b"", b"carol"];
    let rids = {
        let mut bpm = db.open(2);
        insert_all(&mut bpm, 3, &tuples)
    };

    let mut bpm = db.open(2);

    for (rid, expected) in rids.iter().zip(tuples) {
        assert_eq!(read(&mut bpm, *rid).unwrap(), expected);
    }
}

#[test]
fn tuples_on_different_pages_should_be_independent() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let first = insert_all(&mut bpm, 0, &[b"page zero"]);
    let second = insert_all(&mut bpm, 1, &[b"page one"]);
    let third = insert_all(&mut bpm, 2, &[b"page two"]);

    assert_eq!(first[0].slot_id, second[0].slot_id);
    assert_eq!(read(&mut bpm, first[0]).unwrap(), b"page zero");
    assert_eq!(read(&mut bpm, second[0]).unwrap(), b"page one");
    assert_eq!(read(&mut bpm, third[0]).unwrap(), b"page two");
}

#[test]
fn deleted_slot_should_stay_deleted_after_reopen() {
    let db = TempDb::new();
    let rids = {
        let mut bpm = db.open(2);
        let rids = insert_all(&mut bpm, 0, &[b"keep", b"remove"]);
        let page = bpm.fetch_page(0).unwrap();
        SlottedPage::new(page.data_mut())
            .unwrap()
            .delete(rids[1].slot_id)
            .unwrap();
        bpm.unpin_page(0, true);
        rids
    };

    let mut bpm = db.open(2);

    assert_eq!(read(&mut bpm, rids[0]).unwrap(), b"keep");
    assert_eq!(
        read(&mut bpm, rids[1]),
        Err(SlottedPageError::SlotDeleted { slot_id: 1 })
    );
}

#[test]
fn update_and_compaction_should_survive_reopen() {
    let db = TempDb::new();
    let big = vec![b'x'; 3000];
    let rids = {
        let mut bpm = db.open(2);
        let rids = insert_all(&mut bpm, 0, &[&[b'a'; 3000], &[b'b'; 3000], b"c"]);
        let page = bpm.fetch_page(0).unwrap();
        let mut slotted = SlottedPage::new(page.data_mut()).unwrap();
        slotted.delete(rids[0].slot_id).unwrap();
        // Only fits after reclaiming the 3000 bytes freed by the delete.
        slotted.update(rids[2].slot_id, &big).unwrap();
        bpm.unpin_page(0, true);
        rids
    };

    let mut bpm = db.open(2);

    assert!(read(&mut bpm, rids[0]).is_err());
    assert_eq!(read(&mut bpm, rids[1]).unwrap(), vec![b'b'; 3000]);
    assert_eq!(read(&mut bpm, rids[2]).unwrap(), big);
}

#[test]
fn corrupted_slotted_page_on_disk_should_fail_on_checksum() {
    let db = TempDb::new();
    {
        let mut bpm = db.open(2);
        insert_all(&mut bpm, 1, &[b"alice"]);
    }

    // Corrupt slot_count, the first field after the common header: the checksum
    // must catch it before the slotted page ever sees the bad header.
    flip_byte(&db.0, (PAGE_SIZE + COMMON_HEADER_SIZE) as u64);
    let mut bpm = db.open(2);

    assert!(matches!(
        bpm.fetch_page(1).unwrap_err(),
        BufferPoolError::ChecksumMismatch { page_id: 1 }
    ));
}

#[test]
fn fresh_page_from_disk_should_need_init() {
    let db = TempDb::new();
    let mut bpm = db.open(2);

    let page = bpm.fetch_page(7).unwrap();

    assert_eq!(
        SlottedPage::new(page.data()).unwrap_err(),
        SlottedPageError::Uninitialized
    );
    let slotted = SlottedPage::init(page.data_mut()).unwrap();
    assert_eq!(slotted.total_free_space(), PAGE_SIZE - HEADER_SIZE);
}
