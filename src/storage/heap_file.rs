//! Heap file: the unordered tuples of one table in a linked list of slotted pages
//! (`SlottedPage::next_page_id`). Inserts go to the last page; when it is full, a page
//! from `BufferPoolManager::new_page` is linked after it.
//!
//! The heap is identified by its first page. `open` walks the list once and keeps the page
//! ids in memory, which also tells whether a `Rid` belongs to this heap.
//!
//! Every page access goes through `read_page`/`write_page`, which unpin on every path, so a
//! failed operation never leaks a pin.

use std::collections::HashSet;
use std::fmt;
use std::num::NonZeroU32;

use crate::storage::buffer_pool::{BufferPoolError, BufferPoolManager};
use crate::storage::page::RESERVED_PAGE_ID;
use crate::storage::rid::Rid;
use crate::storage::slotted_page::{SlottedPage, SlottedPageError, check_page_size};

#[derive(Debug)]
pub enum HeapFileError {
    BufferPool(BufferPoolError),
    Page {
        page_id: u32,
        source: SlottedPageError,
    },
    TupleTooLarge {
        size: usize,
        max: usize,
    },
    /// The slot does not exist or its tuple was deleted.
    TupleNotFound {
        rid: Rid,
    },
    /// The `Rid` points to a page of another heap (or the reserved page).
    PageNotInHeap {
        page_id: u32,
    },
    CorruptedChain {
        page_id: u32,
        reason: &'static str,
    },
    /// The buffer pool's page size cannot hold a slotted page.
    UnsupportedPageSize {
        size: usize,
    },
}

impl fmt::Display for HeapFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeapFileError::BufferPool(err) => write!(f, "{err}"),
            HeapFileError::Page { page_id, source } => write!(f, "heap page {page_id}: {source}"),
            HeapFileError::TupleTooLarge { size, max } => {
                write!(f, "tuple of {size} bytes exceeds maximum of {max} bytes")
            }
            HeapFileError::TupleNotFound { rid } => {
                write!(f, "no tuple at page {}, slot {}", rid.page_id, rid.slot_id)
            }
            HeapFileError::PageNotInHeap { page_id } => {
                write!(f, "page {page_id} does not belong to this heap file")
            }
            HeapFileError::CorruptedChain { page_id, reason } => {
                write!(f, "heap page list is corrupted at page {page_id}: {reason}")
            }
            HeapFileError::UnsupportedPageSize { size } => {
                write!(f, "page size {size} is not supported by heap files")
            }
        }
    }
}

impl std::error::Error for HeapFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HeapFileError::BufferPool(err) => Some(err),
            HeapFileError::Page { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn page_error(page_id: u32, source: SlottedPageError) -> HeapFileError {
    match source {
        SlottedPageError::TupleTooLarge { size, max } => HeapFileError::TupleTooLarge { size, max },
        source => HeapFileError::Page { page_id, source },
    }
}

fn slot_error(rid: Rid, source: SlottedPageError) -> HeapFileError {
    match source {
        SlottedPageError::InvalidSlot { .. } | SlottedPageError::SlotDeleted { .. } => {
            HeapFileError::TupleNotFound { rid }
        }
        source => page_error(rid.page_id, source),
    }
}

fn read_page<T>(
    bpm: &mut BufferPoolManager,
    page_id: u32,
    read: impl FnOnce(&SlottedPage<&[u8]>) -> Result<T, HeapFileError>,
) -> Result<T, HeapFileError> {
    let page = bpm.fetch_page(page_id)?;
    let result = SlottedPage::new(page.data())
        .map_err(|source| page_error(page_id, source))
        .and_then(|slotted| read(&slotted));
    bpm.unpin_page(page_id, false);
    result
}

/// Marks the page dirty only when `write` succeeds: failed operations leave pages untouched.
fn write_page<T>(
    bpm: &mut BufferPoolManager,
    page_id: u32,
    write: impl FnOnce(&mut SlottedPage<&mut [u8]>) -> Result<T, HeapFileError>,
) -> Result<T, HeapFileError> {
    let page = bpm.fetch_page(page_id)?;
    let result = SlottedPage::new(page.data_mut())
        .map_err(|source| page_error(page_id, source))
        .and_then(|mut slotted| write(&mut slotted));
    bpm.unpin_page(page_id, result.is_ok());
    result
}

fn allocate_page(bpm: &mut BufferPoolManager) -> Result<u32, HeapFileError> {
    let (page_id, page) = bpm.new_page()?;
    let formatted = SlottedPage::init(page.data_mut()).map(drop);
    bpm.unpin_page(page_id, true);
    formatted.map_err(|source| page_error(page_id, source))?;
    Ok(page_id)
}

/// `None` when the page has no room for the tuple.
fn try_insert(
    bpm: &mut BufferPoolManager,
    page_id: u32,
    tuple: &[u8],
) -> Result<Option<Rid>, HeapFileError> {
    write_page(bpm, page_id, |page| match page.insert(tuple) {
        Ok(slot_id) => Ok(Some(Rid::new(page_id, slot_id))),
        Err(SlottedPageError::PageFull) => Ok(None),
        Err(source) => Err(page_error(page_id, source)),
    })
}

impl From<BufferPoolError> for HeapFileError {
    fn from(err: BufferPoolError) -> Self {
        HeapFileError::BufferPool(err)
    }
}

/// Live tuples in page order, then slot order. Each page is pinned once: its tuples are
/// copied out and the page is unpinned before the first one is yielded.
pub struct HeapScan<'a> {
    heap: &'a HeapFile,
    bpm: &'a mut BufferPoolManager,
    next_page_index: usize,
    current_page: std::vec::IntoIter<(Rid, Vec<u8>)>,
}

impl Iterator for HeapScan<'_> {
    type Item = Result<(Rid, Vec<u8>), HeapFileError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(entry) = self.current_page.next() {
                return Some(Ok(entry));
            }
            let page_id = *self.heap.page_ids.get(self.next_page_index)?;
            self.next_page_index += 1;
            match read_page(self.bpm, page_id, |page| Ok(live_tuples(page_id, page))) {
                Ok(tuples) => self.current_page = tuples.into_iter(),
                Err(err) => {
                    self.next_page_index = self.heap.page_ids.len();
                    return Some(Err(err));
                }
            }
        }
    }
}

fn live_tuples(page_id: u32, page: &SlottedPage<&[u8]>) -> Vec<(Rid, Vec<u8>)> {
    page.iter()
        .map(|(slot_id, tuple)| (Rid::new(page_id, slot_id), tuple.to_vec()))
        .collect()
}

#[derive(Debug)]
pub struct HeapFile {
    first_page_id: u32,
    page_ids: Vec<u32>,
}

impl HeapFile {
    /// Checks the page size first, so an unusable pool never consumes a page id.
    pub fn create(bpm: &mut BufferPoolManager) -> Result<Self, HeapFileError> {
        let size = bpm.page_size();
        check_page_size(size).map_err(|_| HeapFileError::UnsupportedPageSize { size })?;
        let first_page_id = allocate_page(bpm)?;
        Ok(Self {
            first_page_id,
            page_ids: vec![first_page_id],
        })
    }

    /// Walks the page list from disk, rejecting cycles and pages that are not slotted.
    pub fn open(bpm: &mut BufferPoolManager, first_page_id: u32) -> Result<Self, HeapFileError> {
        if first_page_id == RESERVED_PAGE_ID {
            return Err(HeapFileError::CorruptedChain {
                page_id: first_page_id,
                reason: "heap cannot start at the reserved page",
            });
        }

        let mut heap = Self {
            first_page_id,
            page_ids: vec![first_page_id],
        };
        heap.extend_to_tail(bpm)?;
        Ok(heap)
    }

    /// Adopts the pages linked after the last known one: all of them on `open`, or the
    /// ones another handle of the same heap appended. Returns whether any was added.
    fn extend_to_tail(&mut self, bpm: &mut BufferPoolManager) -> Result<bool, HeapFileError> {
        let known_pages = self.page_ids.len();
        // Built only when there is a next page, so the common case costs one read.
        let mut visited: Option<HashSet<u32>> = None;
        let mut current = self.last_page_id();
        while let Some(next) = read_page(bpm, current, |page| Ok(page.next_page_id()))? {
            let next = next.get();
            let visited = visited.get_or_insert_with(|| self.page_ids.iter().copied().collect());
            if !visited.insert(next) {
                return Err(HeapFileError::CorruptedChain {
                    page_id: next,
                    reason: "page list has a cycle",
                });
            }
            self.page_ids.push(next);
            current = next;
        }
        Ok(self.page_ids.len() > known_pages)
    }

    pub fn first_page_id(&self) -> u32 {
        self.first_page_id
    }

    pub fn page_count(&self) -> usize {
        self.page_ids.len()
    }

    pub fn scan<'a>(&'a self, bpm: &'a mut BufferPoolManager) -> HeapScan<'a> {
        HeapScan {
            heap: self,
            bpm,
            next_page_index: 0,
            current_page: Vec::new().into_iter(),
        }
    }

    pub fn insert(
        &mut self,
        bpm: &mut BufferPoolManager,
        tuple: &[u8],
    ) -> Result<Rid, HeapFileError> {
        loop {
            if let Some(rid) = try_insert(bpm, self.last_page_id(), tuple)? {
                return Ok(rid);
            }
            // Linking after a page that already has a next would orphan that page.
            if !self.extend_to_tail(bpm)? {
                break;
            }
        }

        let last_page_id = self.last_page_id();
        let new_page_id = allocate_page(bpm)?;
        let rid = try_insert(bpm, new_page_id, tuple)?.ok_or(HeapFileError::Page {
            page_id: new_page_id,
            source: SlottedPageError::PageFull,
        })?;
        // Linked only after the tuple is in place: a failure before this leaves an
        // unreachable page, never a reachable half-written one.
        write_page(bpm, last_page_id, |page| {
            page.set_next_page_id(NonZeroU32::new(new_page_id));
            Ok(())
        })?;
        self.page_ids.push(new_page_id);
        Ok(rid)
    }

    pub fn get(&self, bpm: &mut BufferPoolManager, rid: Rid) -> Result<Vec<u8>, HeapFileError> {
        self.check_owned(rid.page_id)?;
        read_page(bpm, rid.page_id, |page| {
            page.get(rid.slot_id)
                .map(<[u8]>::to_vec)
                .map_err(|source| slot_error(rid, source))
        })
    }

    /// Keeps the `Rid` while the tuple fits its page; otherwise moves the tuple and returns
    /// its new `Rid`. The old version is deleted only after the new one is written.
    pub fn update(
        &mut self,
        bpm: &mut BufferPoolManager,
        rid: Rid,
        tuple: &[u8],
    ) -> Result<Rid, HeapFileError> {
        self.check_owned(rid.page_id)?;
        let fits_in_place = write_page(bpm, rid.page_id, |page| {
            match page.update(rid.slot_id, tuple) {
                Ok(()) => Ok(true),
                Err(SlottedPageError::PageFull) => Ok(false),
                Err(source) => Err(slot_error(rid, source)),
            }
        })?;
        if fits_in_place {
            return Ok(rid);
        }

        let moved = self.insert(bpm, tuple)?;
        if let Err(err) = self.delete(bpm, rid) {
            // Best effort: without a WAL the two writes cannot be atomic, but this keeps a
            // failed update from leaving two live versions of the row.
            let _ = self.delete(bpm, moved);
            return Err(err);
        }
        Ok(moved)
    }

    pub fn delete(&mut self, bpm: &mut BufferPoolManager, rid: Rid) -> Result<(), HeapFileError> {
        self.check_owned(rid.page_id)?;
        write_page(bpm, rid.page_id, |page| {
            page.delete(rid.slot_id)
                .map_err(|source| slot_error(rid, source))
        })
    }

    fn last_page_id(&self) -> u32 {
        self.page_ids.last().copied().unwrap_or(self.first_page_id)
    }

    fn check_owned(&self, page_id: u32) -> Result<(), HeapFileError> {
        if self.page_ids.contains(&page_id) {
            Ok(())
        } else {
            Err(HeapFileError::PageNotInHeap { page_id })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MAX_PAGE_SIZE;
    use crate::storage::slotted_page::{HEADER_SIZE, SLOT_SIZE};
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Small pages fill quickly: the largest tuple is 128 - 24 - 4 = 100 bytes.
    const TEST_PAGE_SIZE: usize = 128;
    const MAX_TUPLE: usize = TEST_PAGE_SIZE - HEADER_SIZE - SLOT_SIZE;

    pub(super) struct TestPool {
        path: PathBuf,
        bpm: Option<BufferPoolManager>,
    }

    impl TestPool {
        pub(super) fn new(pool_size: usize) -> Self {
            Self::with_page_size(pool_size, TEST_PAGE_SIZE)
        }

        fn with_page_size(pool_size: usize, page_size: usize) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("ferro-heap-{}-{id}.db", std::process::id()));
            let bpm = BufferPoolManager::new(pool_size, page_size, &path).unwrap();
            Self {
                path,
                bpm: Some(bpm),
            }
        }

        pub(super) fn bpm(&mut self) -> &mut BufferPoolManager {
            self.bpm.as_mut().unwrap()
        }

        pub(super) fn reopen(&mut self, pool_size: usize) {
            self.bpm.take();
            self.bpm = Some(BufferPoolManager::new(pool_size, TEST_PAGE_SIZE, &self.path).unwrap());
        }
    }

    impl Drop for TestPool {
        fn drop(&mut self) {
            self.bpm.take();
            let _ = fs::remove_file(&self.path);
        }
    }

    fn tuple(len: usize, fill: u8) -> Vec<u8> {
        vec![fill; len]
    }

    /// A pin leak would only surface much later as `NoAvailableFrame`, so check it directly.
    fn assert_unpinned(bpm: &mut BufferPoolManager, heap: &HeapFile) {
        for &page_id in &heap.page_ids {
            let pin_count = bpm.fetch_page(page_id).unwrap().pin_count();
            bpm.unpin_page(page_id, false);
            assert_eq!(pin_count, 1, "page {page_id} left pinned");
        }
    }

    fn next_page_of(bpm: &mut BufferPoolManager, page_id: u32) -> Option<u32> {
        let next = SlottedPage::new(bpm.fetch_page(page_id).unwrap().data())
            .unwrap()
            .next_page_id();
        bpm.unpin_page(page_id, false);
        next.map(|id| id.get())
    }

    /// Two 60-byte tuples never share a page: 24 + 2 * (4 + 60) > 128.
    fn fill_pages(heap: &mut HeapFile, bpm: &mut BufferPoolManager, pages: usize) -> Vec<Rid> {
        (0..pages)
            .map(|i| heap.insert(bpm, &tuple(60, i as u8)).unwrap())
            .collect()
    }

    #[test]
    fn create_should_start_with_one_empty_page() {
        let mut pool = TestPool::new(4);

        let heap = HeapFile::create(pool.bpm()).unwrap();

        assert_eq!(heap.first_page_id(), 1);
        assert_eq!(heap.page_count(), 1);
        assert_eq!(next_page_of(pool.bpm(), 1), None);
        assert_unpinned(pool.bpm(), &heap);
    }

    #[test]
    fn insert_then_get_should_return_the_tuple() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();

        let rid = heap.insert(pool.bpm(), b"hello").unwrap();

        assert_eq!(rid, Rid::new(heap.first_page_id(), 0));
        assert_eq!(heap.get(pool.bpm(), rid).unwrap(), b"hello");
        assert_unpinned(pool.bpm(), &heap);
    }

    #[test]
    fn insert_of_an_empty_tuple_should_work() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();

        let rid = heap.insert(pool.bpm(), b"").unwrap();

        assert_eq!(heap.get(pool.bpm(), rid).unwrap(), b"");
    }

    #[test]
    fn insert_into_a_full_page_should_allocate_and_link_a_new_page() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();

        let rids = fill_pages(&mut heap, pool.bpm(), 2);

        assert_eq!(heap.page_count(), 2);
        assert_ne!(rids[0].page_id, rids[1].page_id);
        assert_eq!(
            next_page_of(pool.bpm(), rids[0].page_id),
            Some(rids[1].page_id)
        );
        assert_eq!(next_page_of(pool.bpm(), rids[1].page_id), None);
        assert_eq!(heap.get(pool.bpm(), rids[0]).unwrap(), tuple(60, 0));
        assert_eq!(heap.get(pool.bpm(), rids[1]).unwrap(), tuple(60, 1));
        assert_unpinned(pool.bpm(), &heap);
    }

    #[test]
    fn insert_of_the_largest_tuple_should_fit_one_page() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();

        let rid = heap.insert(pool.bpm(), &tuple(MAX_TUPLE, 7)).unwrap();

        assert_eq!(heap.page_count(), 1);
        assert_eq!(heap.get(pool.bpm(), rid).unwrap(), tuple(MAX_TUPLE, 7));
    }

    #[test]
    fn insert_larger_than_a_page_should_fail_without_allocating() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        heap.insert(pool.bpm(), &tuple(60, 1)).unwrap();

        let err = heap
            .insert(pool.bpm(), &tuple(MAX_TUPLE + 1, 7))
            .unwrap_err();

        assert!(matches!(
            err,
            HeapFileError::TupleTooLarge { size, max } if size == MAX_TUPLE + 1 && max == MAX_TUPLE
        ));
        assert_eq!(heap.page_count(), 1);
        assert_eq!(pool.bpm().new_page().unwrap().0, heap.first_page_id() + 1);
    }

    #[test]
    fn get_of_a_missing_slot_should_fail() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        heap.insert(pool.bpm(), b"a").unwrap();
        let rid = Rid::new(heap.first_page_id(), 5);

        let err = heap.get(pool.bpm(), rid).unwrap_err();

        assert!(matches!(err, HeapFileError::TupleNotFound { rid: r } if r == rid));
        assert_unpinned(pool.bpm(), &heap);
    }

    #[test]
    fn get_from_a_page_outside_the_heap_should_fail() {
        let mut pool = TestPool::new(4);
        let heap = HeapFile::create(pool.bpm()).unwrap();
        let mut other = HeapFile::create(pool.bpm()).unwrap();
        let other_rid = other.insert(pool.bpm(), b"other").unwrap();

        for rid in [Rid::new(0, 0), other_rid] {
            let err = heap.get(pool.bpm(), rid).unwrap_err();

            assert!(matches!(
                err,
                HeapFileError::PageNotInHeap { page_id } if page_id == rid.page_id
            ));
        }
    }

    #[test]
    fn open_should_rebuild_the_page_list() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        let rids = fill_pages(&mut heap, pool.bpm(), 3);

        let reopened = HeapFile::open(pool.bpm(), heap.first_page_id()).unwrap();

        assert_eq!(reopened.page_ids, heap.page_ids);
        for (i, rid) in rids.into_iter().enumerate() {
            assert_eq!(reopened.get(pool.bpm(), rid).unwrap(), tuple(60, i as u8));
        }
        assert_unpinned(pool.bpm(), &reopened);
    }

    #[test]
    fn open_after_reopening_the_file_should_keep_inserting_at_the_end() {
        let mut pool = TestPool::new(4);
        let first_page_id = {
            let mut heap = HeapFile::create(pool.bpm()).unwrap();
            fill_pages(&mut heap, pool.bpm(), 2);
            heap.first_page_id()
        };
        pool.reopen(4);

        let mut heap = HeapFile::open(pool.bpm(), first_page_id).unwrap();
        let rid = heap.insert(pool.bpm(), &tuple(60, 9)).unwrap();

        assert_eq!(heap.page_count(), 3);
        assert_eq!(heap.get(pool.bpm(), rid).unwrap(), tuple(60, 9));
    }

    #[test]
    fn errors_should_describe_the_problem() {
        let cases = [
            (
                HeapFileError::TupleTooLarge { size: 9, max: 4 },
                "tuple of 9 bytes exceeds maximum of 4 bytes",
            ),
            (
                HeapFileError::TupleNotFound {
                    rid: Rid::new(3, 7),
                },
                "no tuple at page 3, slot 7",
            ),
            (
                HeapFileError::PageNotInHeap { page_id: 4 },
                "page 4 does not belong to this heap file",
            ),
            (
                HeapFileError::CorruptedChain {
                    page_id: 2,
                    reason: "page list has a cycle",
                },
                "heap page list is corrupted at page 2: page list has a cycle",
            ),
            (
                HeapFileError::Page {
                    page_id: 5,
                    source: SlottedPageError::Uninitialized,
                },
                "heap page 5: slotted page is not initialized",
            ),
            (
                HeapFileError::UnsupportedPageSize { size: 70_000 },
                "page size 70000 is not supported by heap files",
            ),
        ];

        for (err, message) in cases {
            assert_eq!(err.to_string(), message);
        }
    }

    #[test]
    fn wrapped_errors_should_expose_their_source() {
        let page = HeapFileError::Page {
            page_id: 1,
            source: SlottedPageError::PageFull,
        };
        let pool = HeapFileError::from(BufferPoolError::NoAvailableFrame);

        assert!(std::error::Error::source(&page).is_some());
        assert!(std::error::Error::source(&pool).is_some());
        assert!(std::error::Error::source(&HeapFileError::PageNotInHeap { page_id: 1 }).is_none());
    }

    fn is_not_found(err: &HeapFileError, expected: Rid) -> bool {
        matches!(err, HeapFileError::TupleNotFound { rid } if *rid == expected)
    }

    #[test]
    fn delete_should_make_the_tuple_unreadable() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        let rid = heap.insert(pool.bpm(), b"bye").unwrap();
        let kept = heap.insert(pool.bpm(), b"stay").unwrap();

        heap.delete(pool.bpm(), rid).unwrap();

        assert!(is_not_found(&heap.get(pool.bpm(), rid).unwrap_err(), rid));
        assert_eq!(heap.get(pool.bpm(), kept).unwrap(), b"stay");
        assert_unpinned(pool.bpm(), &heap);
    }

    #[test]
    fn delete_twice_should_fail_with_tuple_not_found() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        let rid = heap.insert(pool.bpm(), b"bye").unwrap();
        heap.delete(pool.bpm(), rid).unwrap();

        let err = heap.delete(pool.bpm(), rid).unwrap_err();

        assert!(is_not_found(&err, rid));
        assert_unpinned(pool.bpm(), &heap);
    }

    #[test]
    fn delete_outside_the_heap_should_fail() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();

        let err = heap.delete(pool.bpm(), Rid::new(0, 0)).unwrap_err();

        assert!(matches!(err, HeapFileError::PageNotInHeap { page_id: 0 }));
    }

    #[test]
    fn update_that_fits_the_page_should_keep_the_rid() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        let rid = heap.insert(pool.bpm(), &tuple(10, 1)).unwrap();

        for len in [5, 10, 50] {
            let updated = heap.update(pool.bpm(), rid, &tuple(len, 2)).unwrap();

            assert_eq!(updated, rid);
            assert_eq!(heap.get(pool.bpm(), rid).unwrap(), tuple(len, 2));
        }
        assert_eq!(heap.page_count(), 1);
        assert_unpinned(pool.bpm(), &heap);
    }

    #[test]
    fn update_that_does_not_fit_should_move_the_tuple() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        let rid = heap.insert(pool.bpm(), &tuple(40, 1)).unwrap();
        let neighbour = heap.insert(pool.bpm(), &tuple(40, 2)).unwrap();

        let moved = heap.update(pool.bpm(), rid, &tuple(60, 3)).unwrap();

        assert_ne!(moved.page_id, rid.page_id);
        assert_eq!(heap.get(pool.bpm(), moved).unwrap(), tuple(60, 3));
        assert!(is_not_found(&heap.get(pool.bpm(), rid).unwrap_err(), rid));
        assert_eq!(heap.get(pool.bpm(), neighbour).unwrap(), tuple(40, 2));
        assert_unpinned(pool.bpm(), &heap);
    }

    #[test]
    fn update_moving_past_a_full_last_page_should_allocate_another() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        let rid = heap.insert(pool.bpm(), &tuple(30, 1)).unwrap();
        heap.insert(pool.bpm(), &tuple(30, 2)).unwrap();
        heap.insert(pool.bpm(), &tuple(60, 3)).unwrap();
        assert_eq!(heap.page_count(), 2);

        let moved = heap.update(pool.bpm(), rid, &tuple(80, 4)).unwrap();

        assert_eq!(heap.page_count(), 3);
        assert_eq!(moved.page_id, heap.last_page_id());
        assert_eq!(heap.get(pool.bpm(), moved).unwrap(), tuple(80, 4));
    }

    #[test]
    fn update_larger_than_a_page_should_fail_and_keep_the_old_tuple() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        let rid = heap.insert(pool.bpm(), b"old").unwrap();

        let err = heap
            .update(pool.bpm(), rid, &tuple(MAX_TUPLE + 1, 9))
            .unwrap_err();

        assert!(matches!(err, HeapFileError::TupleTooLarge { .. }));
        assert_eq!(heap.get(pool.bpm(), rid).unwrap(), b"old");
        assert_eq!(heap.page_count(), 1);
        assert_unpinned(pool.bpm(), &heap);
    }

    #[test]
    fn update_of_a_missing_or_foreign_rid_should_fail() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        let missing = Rid::new(heap.first_page_id(), 3);

        let not_found = heap.update(pool.bpm(), missing, b"x").unwrap_err();
        let foreign = heap.update(pool.bpm(), Rid::new(0, 0), b"x").unwrap_err();

        assert!(is_not_found(&not_found, missing));
        assert!(matches!(
            foreign,
            HeapFileError::PageNotInHeap { page_id: 0 }
        ));
        assert_unpinned(pool.bpm(), &heap);
    }

    fn scan_all(heap: &HeapFile, bpm: &mut BufferPoolManager) -> Vec<(Rid, Vec<u8>)> {
        heap.scan(bpm).collect::<Result<_, _>>().unwrap()
    }

    #[test]
    fn scan_of_an_empty_heap_should_yield_nothing() {
        let mut pool = TestPool::new(4);
        let heap = HeapFile::create(pool.bpm()).unwrap();

        assert!(scan_all(&heap, pool.bpm()).is_empty());
    }

    #[test]
    fn scan_should_yield_every_tuple_in_page_then_slot_order() {
        let mut pool = TestPool::new(2);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        let tuples: Vec<Vec<u8>> = (0..7).map(|i| tuple(30, i)).collect();
        let rids: Vec<Rid> = tuples
            .iter()
            .map(|t| heap.insert(pool.bpm(), t).unwrap())
            .collect();

        let scanned = scan_all(&heap, pool.bpm());

        assert!(heap.page_count() > 2);
        assert_eq!(scanned, rids.into_iter().zip(tuples).collect::<Vec<_>>());
        assert_unpinned(pool.bpm(), &heap);
    }

    #[test]
    fn scan_should_skip_deleted_tuples() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        let rids = fill_pages(&mut heap, pool.bpm(), 3);
        heap.delete(pool.bpm(), rids[1]).unwrap();

        let scanned: Vec<Rid> = scan_all(&heap, pool.bpm())
            .into_iter()
            .map(|(rid, _)| rid)
            .collect();

        assert_eq!(scanned, [rids[0], rids[2]]);
    }

    #[test]
    fn scan_after_a_moving_update_should_yield_the_tuple_once() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        let rid = heap.insert(pool.bpm(), &tuple(40, 1)).unwrap();
        heap.insert(pool.bpm(), &tuple(40, 2)).unwrap();

        let moved = heap.update(pool.bpm(), rid, &tuple(60, 3)).unwrap();

        let scanned = scan_all(&heap, pool.bpm());
        assert_eq!(scanned.len(), 2);
        assert_eq!(
            scanned.iter().filter(|(_, t)| *t == tuple(60, 3)).count(),
            1
        );
        assert!(scanned.iter().any(|(r, _)| *r == moved));
        assert!(scanned.iter().all(|(r, _)| *r != rid));
    }

    #[test]
    fn dropping_a_scan_midway_should_leave_no_page_pinned() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        fill_pages(&mut heap, pool.bpm(), 3);

        let first = heap.scan(pool.bpm()).next();

        assert!(matches!(first, Some(Ok(_))));
        assert_unpinned(pool.bpm(), &heap);
    }

    fn set_next_page(bpm: &mut BufferPoolManager, page_id: u32, next: u32) {
        let page = bpm.fetch_page(page_id).unwrap();
        SlottedPage::new(page.data_mut())
            .unwrap()
            .set_next_page_id(NonZeroU32::new(next));
        bpm.unpin_page(page_id, true);
    }

    fn is_cycle(err: &HeapFileError, expected_page: u32) -> bool {
        matches!(
            err,
            HeapFileError::CorruptedChain { page_id, reason: "page list has a cycle" }
                if *page_id == expected_page
        )
    }

    #[test]
    fn open_of_a_list_with_a_cycle_should_fail() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        fill_pages(&mut heap, pool.bpm(), 3);
        set_next_page(pool.bpm(), heap.last_page_id(), heap.first_page_id());

        let err = HeapFile::open(pool.bpm(), heap.first_page_id()).unwrap_err();

        assert!(is_cycle(&err, heap.first_page_id()));
        assert_unpinned(pool.bpm(), &heap);
    }

    #[test]
    fn open_of_a_page_pointing_to_itself_should_fail() {
        let mut pool = TestPool::new(4);
        let heap = HeapFile::create(pool.bpm()).unwrap();
        set_next_page(pool.bpm(), heap.first_page_id(), heap.first_page_id());

        let err = HeapFile::open(pool.bpm(), heap.first_page_id()).unwrap_err();

        assert!(is_cycle(&err, heap.first_page_id()));
    }

    #[test]
    fn open_of_a_list_pointing_past_the_end_of_the_file_should_fail() {
        let mut pool = TestPool::new(4);
        let heap = HeapFile::create(pool.bpm()).unwrap();
        set_next_page(pool.bpm(), heap.first_page_id(), 999);

        let err = HeapFile::open(pool.bpm(), heap.first_page_id()).unwrap_err();

        assert!(matches!(
            err,
            HeapFileError::Page {
                page_id: 999,
                source: SlottedPageError::Uninitialized
            }
        ));
    }

    #[test]
    fn open_at_the_reserved_page_should_fail() {
        let mut pool = TestPool::new(4);

        let err = HeapFile::open(pool.bpm(), RESERVED_PAGE_ID).unwrap_err();

        assert!(matches!(
            err,
            HeapFileError::CorruptedChain {
                page_id: RESERVED_PAGE_ID,
                reason: "heap cannot start at the reserved page"
            }
        ));
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }

        fn tuple(&mut self) -> Vec<u8> {
            let len = self.below(MAX_TUPLE + 1);
            (0..len).map(|_| self.next() as u8).collect()
        }
    }

    fn assert_scan_matches(
        heap: &HeapFile,
        bpm: &mut BufferPoolManager,
        model: &HashMap<Rid, Vec<u8>>,
    ) {
        let scanned = scan_all(heap, bpm);
        let as_map: HashMap<Rid, Vec<u8>> = scanned.iter().cloned().collect();

        assert_eq!(scanned.len(), as_map.len(), "scan yielded a duplicate rid");
        assert_eq!(&as_map, model);
    }

    #[test]
    fn random_operations_should_match_model() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut pool = TestPool::new(3);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        let mut model: HashMap<Rid, Vec<u8>> = HashMap::new();
        let mut live: Vec<Rid> = Vec::new();
        let mut deleted: Vec<Rid> = Vec::new();

        for step in 0..5_000 {
            match (rng.below(10), live.is_empty()) {
                (0..=3, _) | (_, true) => {
                    let tuple = rng.tuple();
                    let rid = heap.insert(pool.bpm(), &tuple).unwrap();
                    assert!(
                        model.insert(rid, tuple).is_none(),
                        "{rid:?} handed out twice"
                    );
                    live.push(rid);
                }
                (4..=5, false) => {
                    let index = rng.below(live.len());
                    let tuple = rng.tuple();
                    let new_rid = heap.update(pool.bpm(), live[index], &tuple).unwrap();
                    model.remove(&live[index]);
                    model.insert(new_rid, tuple);
                    live[index] = new_rid;
                }
                (6, false) => {
                    let rid = live.swap_remove(rng.below(live.len()));
                    heap.delete(pool.bpm(), rid).unwrap();
                    model.remove(&rid);
                    deleted.push(rid);
                }
                _ => {
                    let rid = live[rng.below(live.len())];
                    assert_eq!(heap.get(pool.bpm(), rid).unwrap(), model[&rid]);
                }
            }
            if let Some(&gone) = deleted.last()
                && !model.contains_key(&gone)
            {
                assert!(is_not_found(&heap.get(pool.bpm(), gone).unwrap_err(), gone));
            }
            if step % 500 == 0 {
                assert_scan_matches(&heap, pool.bpm(), &model);
                assert_unpinned(pool.bpm(), &heap);
            }
        }

        let reopened = HeapFile::open(pool.bpm(), heap.first_page_id()).unwrap();
        assert_eq!(reopened.page_ids, heap.page_ids);
        assert_scan_matches(&reopened, pool.bpm(), &model);
    }

    #[test]
    fn insert_through_a_stale_handle_should_not_orphan_pages() {
        let mut pool = TestPool::new(4);
        let mut fresh = HeapFile::create(pool.bpm()).unwrap();
        fresh.insert(pool.bpm(), &tuple(60, 1)).unwrap();
        let mut stale = HeapFile::open(pool.bpm(), fresh.first_page_id()).unwrap();
        let fresh_rid = fresh.insert(pool.bpm(), &tuple(60, 2)).unwrap();

        let stale_rid = stale.insert(pool.bpm(), &tuple(60, 3)).unwrap();

        let reopened = HeapFile::open(pool.bpm(), fresh.first_page_id()).unwrap();
        let scanned: Vec<Rid> = scan_all(&reopened, pool.bpm())
            .into_iter()
            .map(|(rid, _)| rid)
            .collect();
        assert_eq!(reopened.page_count(), 3);
        assert!(scanned.contains(&fresh_rid), "{fresh_rid:?} was orphaned");
        assert!(scanned.contains(&stale_rid));
        assert_eq!(stale.page_ids, reopened.page_ids);
    }

    #[test]
    fn create_with_an_unsupported_page_size_should_fail_without_allocating() {
        let page_size = MAX_PAGE_SIZE + 1;
        let mut pool = TestPool::with_page_size(1, page_size);

        let err = HeapFile::create(pool.bpm()).unwrap_err();

        assert!(matches!(
            err,
            HeapFileError::UnsupportedPageSize { size } if size == page_size
        ));
        assert_eq!(pool.bpm().new_page().unwrap().0, 1);
    }

    #[test]
    fn update_whose_delete_fails_should_not_leave_two_versions() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        let rid = heap.insert(pool.bpm(), &tuple(30, 1)).unwrap();
        heap.insert(pool.bpm(), &tuple(30, 2)).unwrap();
        heap.insert(pool.bpm(), &tuple(60, 3)).unwrap();
        // The in-place attempt reads the page once; the delete of the old version is next.
        pool.bpm().fail_fetch_after(rid.page_id, 1);

        let err = heap.update(pool.bpm(), rid, &tuple(80, 4)).unwrap_err();

        assert!(matches!(
            err,
            HeapFileError::BufferPool(BufferPoolError::NoAvailableFrame)
        ));
        assert_eq!(heap.get(pool.bpm(), rid).unwrap(), tuple(30, 1));
        let tuples: Vec<Vec<u8>> = scan_all(&heap, pool.bpm())
            .into_iter()
            .map(|(_, tuple)| tuple)
            .collect();
        assert_eq!(tuples, [tuple(30, 1), tuple(30, 2), tuple(60, 3)]);
        assert_unpinned(pool.bpm(), &heap);
    }

    #[test]
    fn insert_without_a_free_frame_should_fail_and_leave_no_pin() {
        let mut pool = TestPool::new(1);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        pool.bpm().fetch_page(50).unwrap();

        let err = heap.insert(pool.bpm(), b"x").unwrap_err();
        pool.bpm().unpin_page(50, false);

        assert!(matches!(
            err,
            HeapFileError::BufferPool(BufferPoolError::NoAvailableFrame)
        ));
        assert_eq!(heap.page_count(), 1);
        assert!(scan_all(&heap, pool.bpm()).is_empty());
        assert_unpinned(pool.bpm(), &heap);
    }

    #[test]
    fn scan_should_stop_after_a_page_error() {
        let mut pool = TestPool::new(4);
        let mut heap = HeapFile::create(pool.bpm()).unwrap();
        let rids = fill_pages(&mut heap, pool.bpm(), 3);
        pool.bpm().fail_fetch_after(rids[1].page_id, 0);

        let mut scan = heap.scan(pool.bpm());
        let first = scan.next();
        let failed = scan.next();
        let after_error = scan.next();

        assert!(matches!(first, Some(Ok((rid, _))) if rid == rids[0]));
        assert!(matches!(
            failed,
            Some(Err(HeapFileError::BufferPool(
                BufferPoolError::NoAvailableFrame
            )))
        ));
        assert!(after_error.is_none());
        assert_unpinned(pool.bpm(), &heap);
    }
}
