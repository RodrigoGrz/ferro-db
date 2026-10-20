use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::ops::Bound;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::config::PAGE_SIZE;
use ferro_db::index::{BPlusTree, IndexError};
use ferro_db::storage::buffer_pool::{BufferPoolError, BufferPoolManager};
use ferro_db::storage::rid::Rid;
use ferro_db::types::{DataType, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("ferro-index-it-{}-{id}.db", std::process::id())))
    }

    fn open(&self, pool_size: usize) -> BufferPoolManager {
        BufferPoolManager::new(pool_size, PAGE_SIZE, &self.0).unwrap()
    }

    fn file_len(&self) -> u64 {
        fs::metadata(&self.0).unwrap().len()
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

/// Enough frames for a split that climbs a few levels; far fewer than the tree's pages.
const POOL_SIZE: usize = 12;

/// Long keys (about 200 bytes), so 20,000 of them make a tree several levels deep.
fn text_key(id: usize) -> Value {
    Value::Text(format!(
        "{:06}-{}",
        (id * 7_919) % 20_000,
        "k".repeat(190 + id % 10)
    ))
}

fn rid(id: usize) -> Rid {
    Rid::new(id as u32 / 100 + 1, (id % 100) as u16)
}

fn scan(bpm: &mut BufferPoolManager, tree: &BPlusTree) -> Vec<(Value, Rid)> {
    tree.range(bpm, Bound::Unbounded, Bound::Unbounded)
        .unwrap()
        .map(|entry| entry.unwrap())
        .collect()
}

fn model_of(ids: impl Iterator<Item = usize>) -> Vec<(Value, Rid)> {
    let model: BTreeMap<String, Rid> = ids
        .map(|id| {
            let Value::Text(text) = text_key(id) else {
                unreachable!()
            };
            (text, rid(id))
        })
        .collect();
    model
        .into_iter()
        .map(|(text, rid)| (Value::Text(text), rid))
        .collect()
}

#[test]
fn a_large_tree_should_survive_eviction_and_reopening() {
    let db = TempDb::new();
    let header_page_id = {
        let mut bpm = db.open(POOL_SIZE);
        let mut tree = BPlusTree::create(&mut bpm, DataType::VARCHAR_MAX).unwrap();
        for id in 0..20_000 {
            tree.insert(&mut bpm, &text_key(id), rid(id)).unwrap();
        }
        tree.header_page_id()
    };

    let mut bpm = db.open(POOL_SIZE);
    let mut tree = BPlusTree::open(&mut bpm, DataType::VARCHAR_MAX, header_page_id).unwrap();
    assert_eq!(scan(&mut bpm, &tree), model_of(0..20_000));
    for id in (0..20_000).step_by(997) {
        assert_eq!(tree.get(&mut bpm, &text_key(id)).unwrap(), Some(rid(id)));
    }
    for id in (0..20_000).step_by(2) {
        assert_eq!(tree.remove(&mut bpm, &text_key(id)).unwrap(), Some(rid(id)));
    }
    drop(bpm);

    let mut bpm = db.open(POOL_SIZE);
    let tree = BPlusTree::open(&mut bpm, DataType::VARCHAR_MAX, header_page_id).unwrap();
    assert_eq!(scan(&mut bpm, &tree), model_of((1..20_000).step_by(2)));
    assert_eq!(tree.get(&mut bpm, &text_key(0)).unwrap(), None);
}

#[test]
fn a_damaged_page_on_disk_should_be_detected_by_the_checksum() {
    let db = TempDb::new();
    let header_page_id = {
        let mut bpm = db.open(POOL_SIZE);
        let mut tree = BPlusTree::create(&mut bpm, DataType::BigInt).unwrap();
        for value in 0..2_000 {
            tree.insert(&mut bpm, &Value::BigInt(value), Rid::new(1, 0))
                .unwrap();
        }
        tree.header_page_id()
    };
    // The first root, allocated right after the header, is the leftmost leaf after splits.
    let first_leaf = header_page_id + 1;
    db.flip_byte(first_leaf, PAGE_SIZE - 10);

    let mut bpm = db.open(POOL_SIZE);
    let tree = BPlusTree::open(&mut bpm, DataType::BigInt, header_page_id).unwrap();
    let err = tree.get(&mut bpm, &Value::BigInt(0)).unwrap_err();
    let scanned: Vec<_> = tree
        .range(&mut bpm, Bound::Unbounded, Bound::Unbounded)
        .err()
        .into_iter()
        .collect();

    assert!(
        matches!(err, IndexError::BufferPool(BufferPoolError::ChecksumMismatch { page_id }) if page_id == first_leaf),
        "{err:?}"
    );
    assert_eq!(err.sqlstate(), "XX001");
    assert_eq!(scanned.len(), 1, "the scan starts at the damaged leaf");
    assert_eq!(
        tree.get(&mut bpm, &Value::BigInt(1_999)).unwrap(),
        Some(Rid::new(1, 0))
    );
}

#[test]
fn a_damaged_header_should_stop_the_tree_from_opening() {
    let db = TempDb::new();
    let header_page_id = {
        let mut bpm = db.open(POOL_SIZE);
        BPlusTree::create(&mut bpm, DataType::BigInt)
            .unwrap()
            .header_page_id()
    };
    db.flip_byte(header_page_id, 17);

    let mut bpm = db.open(POOL_SIZE);
    let err = BPlusTree::open(&mut bpm, DataType::BigInt, header_page_id).unwrap_err();

    assert!(
        matches!(
            err,
            IndexError::BufferPool(BufferPoolError::ChecksumMismatch { .. })
        ),
        "{err:?}"
    );
}

#[test]
fn filling_and_emptying_the_tree_again_should_not_grow_the_file() {
    let db = TempDb::new();
    let ids = 0..5_000;
    let mut bpm = db.open(POOL_SIZE);
    let mut tree = BPlusTree::create(&mut bpm, DataType::VARCHAR_MAX).unwrap();
    let mut lengths = Vec::new();

    for _ in 0..3 {
        for id in ids.clone() {
            tree.insert(&mut bpm, &text_key(id), rid(id)).unwrap();
        }
        for id in ids.clone() {
            tree.remove(&mut bpm, &text_key(id)).unwrap();
        }
        let header_page_id = tree.header_page_id();
        drop(bpm);
        lengths.push(db.file_len());
        bpm = db.open(POOL_SIZE);
        tree = BPlusTree::open(&mut bpm, DataType::VARCHAR_MAX, header_page_id).unwrap();
    }

    assert!(scan(&mut bpm, &tree).is_empty());
    assert_eq!(lengths[1], lengths[0], "{lengths:?}");
    assert_eq!(lengths[2], lengths[0], "{lengths:?}");
}
