use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::Path;

use crate::disk_manager::{DiskError, DiskManager};
use crate::page::{Page, PageError};
use crate::replacer::LRUKReplacer;

const LRU_K: usize = 2;

#[derive(Debug)]
pub enum BufferPoolError {
    InvalidPoolSize,
    InvalidPageSize,
    NoAvailableFrame,
    Disk(DiskError),
    Page(PageError),
}

impl fmt::Display for BufferPoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BufferPoolError::InvalidPoolSize => {
                write!(f, "pool size must be greater than zero")
            }
            BufferPoolError::InvalidPageSize => {
                write!(f, "page size must be greater than zero")
            }
            BufferPoolError::NoAvailableFrame => write!(f, "no available frame"),
            BufferPoolError::Disk(err) => write!(f, "{err}"),
            BufferPoolError::Page(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for BufferPoolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BufferPoolError::Disk(err) => Some(err),
            BufferPoolError::Page(err) => Some(err),
            _ => None,
        }
    }
}

impl From<DiskError> for BufferPoolError {
    fn from(err: DiskError) -> Self {
        BufferPoolError::Disk(err)
    }
}

impl From<PageError> for BufferPoolError {
    fn from(err: PageError) -> Self {
        BufferPoolError::Page(err)
    }
}

#[derive(Debug)]
pub struct BufferPoolManager {
    pages: Vec<Page>,
    page_table: HashMap<u32, usize>,
    free_list: Vec<usize>,
    replacer: LRUKReplacer,
    disk_manager: DiskManager,
}

impl BufferPoolManager {
    pub fn new(
        pool_size: usize,
        page_size: usize,
        path: impl AsRef<Path>,
    ) -> Result<Self, BufferPoolError> {
        if pool_size == 0 {
            return Err(BufferPoolError::InvalidPoolSize);
        }
        if page_size == 0 {
            return Err(BufferPoolError::InvalidPageSize);
        }

        let path = path_to_str(path.as_ref())?;
        let disk_manager = DiskManager::new(path, page_size)?;

        let mut pages = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            pages.push(Page::new(page_size)?);
        }

        let free_list = (0..pool_size).collect();
        let replacer = LRUKReplacer::new(LRU_K).expect("LRU-K constant is valid");

        Ok(Self {
            pages,
            page_table: HashMap::new(),
            free_list,
            replacer,
            disk_manager,
        })
    }

    pub fn fetch_page(&mut self, page_id: u32) -> Result<&mut Page, BufferPoolError> {
        if let Some(&frame_id) = self.page_table.get(&page_id) {
            self.pages[frame_id].pin();
            self.replacer.record_access(frame_id);
            self.replacer.pin(frame_id);
            return Ok(&mut self.pages[frame_id]);
        }

        let data = self.disk_manager.read_page(page_id)?;

        let (frame_id, from_free_list) = if let Some(&id) = self.free_list.last() {
            (id, true)
        } else if let Some(id) = self.replacer.select_victim() {
            (id, false)
        } else {
            return Err(BufferPoolError::NoAvailableFrame);
        };

        let old_page_id = self.pages[frame_id].page_id();
        if let Some(old_page_id) = old_page_id {
            if self.pages[frame_id].is_dirty() {
                self.write_frame(frame_id, old_page_id)?;
            }
        }

        self.pages[frame_id].load(page_id, data)?;

        if from_free_list {
            let popped = self.free_list.pop();
            debug_assert_eq!(popped, Some(frame_id));
        } else {
            self.replacer.forget(frame_id);
        }

        if let Some(old_page_id) = old_page_id {
            self.page_table.remove(&old_page_id);
        }
        self.page_table.insert(page_id, frame_id);

        self.replacer.record_access(frame_id);
        self.replacer.pin(frame_id);

        Ok(&mut self.pages[frame_id])
    }

    pub fn unpin_page(&mut self, page_id: u32, is_dirty: bool) -> bool {
        let Some(&frame_id) = self.page_table.get(&page_id) else {
            return false;
        };

        if !self.pages[frame_id].try_unpin(is_dirty) {
            return false;
        }

        if self.pages[frame_id].pin_count() == 0 {
            self.replacer.unpin(frame_id);
        }

        true
    }

    pub fn flush_page(&mut self, page_id: u32) -> Result<(), BufferPoolError> {
        let Some(&frame_id) = self.page_table.get(&page_id) else {
            return Ok(());
        };

        self.flush_frame(frame_id)
    }

    fn flush_frame(&mut self, frame_id: usize) -> Result<(), BufferPoolError> {
        let Some(page_id) = self.pages[frame_id].page_id() else {
            return Ok(());
        };
        if !self.pages[frame_id].is_dirty() {
            return Ok(());
        }

        self.write_frame(frame_id, page_id)?;
        self.pages[frame_id].mark_clean();
        Ok(())
    }

    fn write_frame(&mut self, frame_id: usize, page_id: u32) -> Result<(), BufferPoolError> {
        let BufferPoolManager {
            pages,
            disk_manager,
            ..
        } = self;

        disk_manager.write_page(page_id, pages[frame_id].data())?;
        Ok(())
    }
}

impl Drop for BufferPoolManager {
    fn drop(&mut self) {
        let page_ids: Vec<u32> = self.page_table.keys().copied().collect();
        for page_id in page_ids {
            let _ = self.flush_page(page_id);
        }
    }
}

fn path_to_str(path: &Path) -> Result<&str, BufferPoolError> {
    path.to_str().ok_or_else(|| {
        BufferPoolError::Disk(DiskError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "database path must be valid UTF-8",
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk_manager::DiskManager;
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestPool {
        path: PathBuf,
        bpm: Option<BufferPoolManager>,
    }

    impl TestPool {
        fn new(pool_size: usize, page_size: usize) -> Self {
            let path = temp_path();
            let bpm = BufferPoolManager::new(pool_size, page_size, &path).unwrap();
            Self {
                path,
                bpm: Some(bpm),
            }
        }

        fn bpm(&mut self) -> &mut BufferPoolManager {
            self.bpm.as_mut().unwrap()
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn close(&mut self) {
            self.bpm.take();
        }
    }

    impl Drop for TestPool {
        fn drop(&mut self) {
            self.bpm.take();
            let _ = fs::remove_file(&self.path);
        }
    }

    struct TempPath(PathBuf);

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn temp_path() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ferro-bpm-{}-{id}.db", std::process::id()))
    }

    fn read_page(path: &Path, page_id: u32) -> Vec<u8> {
        let mut dm = DiskManager::new(path.to_str().unwrap(), 4).unwrap();
        dm.read_page(page_id).unwrap()
    }

    #[test]
    fn fetch_page_should_load_and_store_in_page_table() {
        let mut pool = TestPool::new(2, 4);

        let page = pool.bpm().fetch_page(1).unwrap();

        assert_eq!(page.page_id(), Some(1));
        assert!(pool.bpm().page_table.contains_key(&1));
    }

    #[test]
    fn fetch_hit_should_keep_in_memory_bytes() {
        let mut pool = TestPool::new(2, 4);

        {
            let page = pool.bpm().fetch_page(1).unwrap();
            page.data_mut()[0] = 42;
        }
        pool.bpm().unpin_page(1, false);

        let page = pool.bpm().fetch_page(1).unwrap();

        assert_eq!(page.data()[0], 42);
    }

    #[test]
    fn fetch_page_should_increment_pin_count() {
        let mut pool = TestPool::new(2, 4);

        pool.bpm().fetch_page(1).unwrap();
        let page = pool.bpm().fetch_page(1).unwrap();

        assert_eq!(page.pin_count(), 2);
    }

    #[test]
    fn unpin_should_decrease_pin_count() {
        let mut pool = TestPool::new(2, 4);

        pool.bpm().fetch_page(1).unwrap();
        pool.bpm().fetch_page(1).unwrap();

        assert!(pool.bpm().unpin_page(1, false));

        let page = pool.bpm().fetch_page(1).unwrap();
        assert_eq!(page.pin_count(), 2);
    }

    #[test]
    fn unpin_should_mark_page_dirty() {
        let mut pool = TestPool::new(2, 4);

        pool.bpm().fetch_page(1).unwrap();
        pool.bpm().unpin_page(1, true);

        let page = pool.bpm().fetch_page(1).unwrap();

        assert!(page.is_dirty());
    }

    #[test]
    fn pinned_page_should_not_be_evicted() {
        let mut pool = TestPool::new(1, 4);

        pool.bpm().fetch_page(1).unwrap();
        pool.bpm().fetch_page(1).unwrap();
        assert!(pool.bpm().unpin_page(1, false));

        let err = pool.bpm().fetch_page(2).unwrap_err();

        assert!(matches!(err, BufferPoolError::NoAvailableFrame));
        assert!(pool.bpm().page_table.contains_key(&1));
    }

    #[test]
    fn full_pool_of_pinned_pages_should_return_no_available_frame() {
        let mut pool = TestPool::new(2, 4);

        pool.bpm().fetch_page(1).unwrap();
        pool.bpm().fetch_page(2).unwrap();

        let err = pool.bpm().fetch_page(3).unwrap_err();

        assert!(matches!(err, BufferPoolError::NoAvailableFrame));
        assert_eq!(pool.bpm().page_table.len(), 2);
    }

    #[test]
    fn should_evict_older_cold_page_when_pool_is_full() {
        let mut pool = TestPool::new(2, 4);

        pool.bpm().fetch_page(1).unwrap();
        pool.bpm().unpin_page(1, false);

        pool.bpm().fetch_page(2).unwrap();
        pool.bpm().unpin_page(2, false);

        pool.bpm().fetch_page(3).unwrap();

        assert!(!pool.bpm().page_table.contains_key(&1));
        assert!(pool.bpm().page_table.contains_key(&2));
        assert!(pool.bpm().page_table.contains_key(&3));
    }

    #[test]
    fn should_use_lruk_for_eviction() {
        let mut pool = TestPool::new(2, 4);

        pool.bpm().fetch_page(1).unwrap();
        pool.bpm().unpin_page(1, false);
        pool.bpm().fetch_page(1).unwrap();
        pool.bpm().unpin_page(1, false);

        pool.bpm().fetch_page(2).unwrap();
        pool.bpm().unpin_page(2, false);

        pool.bpm().fetch_page(3).unwrap();

        assert!(pool.bpm().page_table.contains_key(&1));
        assert!(!pool.bpm().page_table.contains_key(&2));
        assert!(pool.bpm().page_table.contains_key(&3));
    }

    #[test]
    fn dirty_page_should_be_written_to_disk_on_eviction() {
        let mut pool = TestPool::new(1, 4);

        {
            let page = pool.bpm().fetch_page(1).unwrap();
            page.data_mut()[0] = 99;
        }
        pool.bpm().unpin_page(1, true);
        pool.bpm().fetch_page(2).unwrap();

        assert_eq!(read_page(pool.path(), 1)[0], 99);
    }

    #[test]
    fn dirty_page_should_be_written_to_disk_on_drop() {
        let mut pool = TestPool::new(1, 4);

        {
            let page = pool.bpm().fetch_page(1).unwrap();
            page.data_mut()[0] = 77;
        }
        pool.bpm().unpin_page(1, true);
        let path = pool.path().to_path_buf();
        pool.close();

        assert_eq!(read_page(&path, 1)[0], 77);
    }

    #[test]
    fn clean_modification_should_not_be_persisted() {
        let mut pool = TestPool::new(1, 4);

        {
            let page = pool.bpm().fetch_page(1).unwrap();
            page.data_mut()[0] = 55;
        }
        pool.bpm().unpin_page(1, false);
        pool.bpm().fetch_page(2).unwrap();

        assert_eq!(read_page(pool.path(), 1), vec![0; 4]);
    }

    #[test]
    fn unpin_nonexistent_page_should_return_false() {
        let mut pool = TestPool::new(2, 4);

        assert!(!pool.bpm().unpin_page(999, false));
    }

    #[test]
    fn unpin_when_pin_count_is_zero_should_return_false() {
        let mut pool = TestPool::new(2, 4);

        pool.bpm().fetch_page(1).unwrap();
        assert!(pool.bpm().unpin_page(1, false));
        assert!(!pool.bpm().unpin_page(1, false));
    }

    #[test]
    fn zero_pool_or_page_size_should_fail_without_creating_a_file() {
        let path = TempPath(temp_path());

        let pool_err = BufferPoolManager::new(0, 4, &path.0).unwrap_err();
        assert!(matches!(pool_err, BufferPoolError::InvalidPoolSize));
        assert!(!path.0.exists());

        let page_err = BufferPoolManager::new(2, 0, &path.0).unwrap_err();
        assert!(matches!(page_err, BufferPoolError::InvalidPageSize));
        assert!(!path.0.exists());
    }

    #[test]
    fn failed_read_should_keep_the_free_list_usable() {
        let path = TempPath(temp_path());
        {
            let mut file = File::create(&path.0).unwrap();
            file.write_all(&[1, 2]).unwrap();
        }

        let mut bpm = BufferPoolManager::new(1, 4, &path.0).unwrap();
        let err = bpm.fetch_page(0).unwrap_err();

        assert!(matches!(
            err,
            BufferPoolError::Disk(DiskError::TornPage { .. })
        ));

        let page = bpm.fetch_page(1).unwrap();
        assert_eq!(page.page_id(), Some(1));
    }
}
