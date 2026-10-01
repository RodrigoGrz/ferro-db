use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::config::PAGE_SIZE;
use ferro_db::storage::buffer_pool::{BufferPoolError, BufferPoolManager};
use ferro_db::storage::heap_file::{HeapFile, HeapFileError};
use ferro_db::storage::rid::Rid;

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-heap-it-{}-{id}.db", std::process::id())))
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

/// Three of these fill an 8 KB page, so a few rows already span several pages.
fn row(i: usize) -> Vec<u8> {
    let mut row = vec![i as u8; 2_500];
    row[..8].copy_from_slice(&(i as u64).to_le_bytes());
    row
}

fn insert_rows(heap: &mut HeapFile, bpm: &mut BufferPoolManager, rows: usize) -> Vec<Rid> {
    (0..rows)
        .map(|i| heap.insert(bpm, &row(i)).unwrap())
        .collect()
}

fn scan_all(heap: &HeapFile, bpm: &mut BufferPoolManager) -> Vec<(Rid, Vec<u8>)> {
    heap.scan(bpm).collect::<Result<_, _>>().unwrap()
}

#[test]
fn heap_should_survive_eviction() {
    let db = TempDb::new();
    let mut bpm = db.open(2);
    let mut heap = HeapFile::create(&mut bpm).unwrap();

    let rids = insert_rows(&mut heap, &mut bpm, 20);

    assert!(heap.page_count() > 2);
    for (i, rid) in rids.into_iter().enumerate() {
        assert_eq!(heap.get(&mut bpm, rid).unwrap(), row(i));
    }
}

#[test]
fn heap_should_survive_reopening_the_file() {
    let db = TempDb::new();
    let (first_page_id, rids) = {
        let mut bpm = db.open(2);
        let mut heap = HeapFile::create(&mut bpm).unwrap();
        let rids = insert_rows(&mut heap, &mut bpm, 10);
        heap.delete(&mut bpm, rids[3]).unwrap();
        (heap.first_page_id(), rids)
    };

    let mut bpm = db.open(2);
    let heap = HeapFile::open(&mut bpm, first_page_id).unwrap();

    let expected: Vec<(Rid, Vec<u8>)> = rids
        .into_iter()
        .enumerate()
        .filter(|&(i, _)| i != 3)
        .map(|(i, rid)| (rid, row(i)))
        .collect();
    assert_eq!(scan_all(&heap, &mut bpm), expected);
}

#[test]
fn two_heaps_in_one_file_should_not_mix() {
    let db = TempDb::new();
    let mut bpm = db.open(3);
    let mut left = HeapFile::create(&mut bpm).unwrap();
    let mut right = HeapFile::create(&mut bpm).unwrap();

    let mut left_rids = Vec::new();
    let mut right_rids = Vec::new();
    for i in 0..12 {
        left_rids.push(left.insert(&mut bpm, &row(i)).unwrap());
        right_rids.push(right.insert(&mut bpm, &row(100 + i)).unwrap());
    }

    let left_scan: Vec<Rid> = scan_all(&left, &mut bpm)
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    let right_scan: Vec<Rid> = scan_all(&right, &mut bpm)
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    assert_eq!(left_scan, left_rids);
    assert_eq!(right_scan, right_rids);
    assert!(matches!(
        left.get(&mut bpm, right_rids[0]).unwrap_err(),
        HeapFileError::PageNotInHeap { .. }
    ));
}

#[test]
fn pages_allocated_after_reopening_should_not_reuse_heap_pages() {
    let db = TempDb::new();
    let (first_page_id, heap_pages) = {
        let mut bpm = db.open(2);
        let mut heap = HeapFile::create(&mut bpm).unwrap();
        let rids = insert_rows(&mut heap, &mut bpm, 9);
        let pages: HashSet<u32> = rids.iter().map(|rid| rid.page_id).collect();
        (heap.first_page_id(), pages)
    };

    let mut bpm = db.open(2);
    let mut other = HeapFile::create(&mut bpm).unwrap();
    other.insert(&mut bpm, b"other table").unwrap();
    let heap = HeapFile::open(&mut bpm, first_page_id).unwrap();

    assert!(!heap_pages.contains(&other.first_page_id()));
    assert_eq!(scan_all(&heap, &mut bpm).len(), 9);
}

#[test]
fn corruption_of_a_middle_page_should_be_detected_on_open() {
    let db = TempDb::new();
    let (first_page_id, middle_page_id) = {
        let mut bpm = db.open(2);
        let mut heap = HeapFile::create(&mut bpm).unwrap();
        let rids = insert_rows(&mut heap, &mut bpm, 9);
        (heap.first_page_id(), rids[4].page_id)
    };
    db.flip_byte(middle_page_id, PAGE_SIZE - 100);

    let mut bpm = db.open(2);
    let err = HeapFile::open(&mut bpm, first_page_id).unwrap_err();

    assert!(matches!(
        err,
        HeapFileError::BufferPool(BufferPoolError::ChecksumMismatch { page_id })
            if page_id == middle_page_id
    ));
}
