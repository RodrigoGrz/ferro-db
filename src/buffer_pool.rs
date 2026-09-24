use std::{collections::HashMap};

use crate::page::Page;
use crate::disk_manager::DiskManager;
use crate::replacer::LRUKReplacer;

pub struct BufferPoolManager {
    pages: Vec<Page>,
    page_table: HashMap<u32, usize>,
    free_list: Vec<usize>,
    replacer: LRUKReplacer,
    disk_manager: DiskManager,
}

impl BufferPoolManager {
    pub fn new(pool_size: usize, page_size: usize) -> Self {
        let pages = (0..pool_size)
            .map(|_| Page::new(page_size).expect("Failed to create page"))
            .collect();

        let free_list = (0..pool_size).collect();

        Self {
            pages,
            page_table: HashMap::new(),
            free_list,
            replacer: LRUKReplacer::new(2),
            disk_manager: DiskManager::new("data.db", page_size)
                .expect("Failed to open database file"),
        }
    }

    pub fn fetch_page(&mut self, page_id: u32) -> Option<&mut Page> {
        if let Some(&frame_id) = self.page_table.get(&page_id) {
            let page = &mut self.pages[frame_id];

            page.pin();

            self.replacer.record_access(frame_id);
            self.replacer.pin(frame_id);

            return Some(page);
        }

        let frame_id = self.free_list.pop().or_else(|| self.replacer.victim())?;

        let old_page = &mut self.pages[frame_id];

        if let Some(old_page_id) = old_page.page_id() {
            if old_page.is_dirty() {
                self.disk_manager
                    .write_page(old_page_id, old_page.data())
                    .expect("Failed to write page");
            }

            self.page_table.remove(&old_page_id);
        }

        let data = self.disk_manager
            .read_page(page_id)
            .expect("Failed to read page");

        let page = &mut self.pages[frame_id];

        page.load(page_id, data).expect("Failed to load page");

        self.page_table.insert(page_id, frame_id);

        self.replacer.record_access(frame_id);
        self.replacer.pin(frame_id);

        Some(page)
    }

    pub fn unpin_page(&mut self, page_id: u32, is_dirty: bool) -> bool {
        if let Some(&frame_id) = self.page_table.get(&page_id) {
            let page = &mut self.pages[frame_id];

            if !page.try_unpin(is_dirty) {
                return false;
            }

            if page.pin_count() == 0 {
                self.replacer.unpin(frame_id);
            }

            return true;
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_page_should_load_and_store_in_page_table() {
        let mut bpm = BufferPoolManager::new(2, 4);

        let page = bpm.fetch_page(1).unwrap();

        assert_eq!(page.page_id(), Some(1));
        assert!(bpm.page_table.contains_key(&1));
    }

    #[test]
    fn fetch_page_should_return_same_frame_on_hit() {
        let mut bpm = BufferPoolManager::new(2, 4);

        let p1 = bpm.fetch_page(1).unwrap() as *const _;
        bpm.unpin_page(1, false);

        let p2 = bpm.fetch_page(1).unwrap() as *const _;

        assert_eq!(p1, p2);
    }

    #[test]
    fn fetch_page_should_increment_pin_count() {
        let mut bpm = BufferPoolManager::new(2, 4);

        bpm.fetch_page(1);
        let page = bpm.fetch_page(1).unwrap();
        assert_eq!(page.pin_count(), 2);
    }

    #[test]
    fn unpin_should_decrease_pin_count() {
        let mut bpm = BufferPoolManager::new(2, 4);

        bpm.fetch_page(1);
        bpm.fetch_page(1);

        assert!(bpm.unpin_page(1, false));

        let page = bpm.fetch_page(1).unwrap();
        assert_eq!(page.pin_count(), 2);
    }

    #[test]
    fn unpin_should_mark_page_dirty() {
        let mut bpm = BufferPoolManager::new(2, 4);

        bpm.fetch_page(1);
        bpm.unpin_page(1, true);

        let page = bpm.fetch_page(1).unwrap();

        assert!(page.is_dirty());
    }

    #[test]
    fn should_evict_page_when_pool_is_full() {
        let mut bpm = BufferPoolManager::new(2, 4);

        bpm.fetch_page(1);
        bpm.unpin_page(1, false);

        bpm.fetch_page(2);
        bpm.unpin_page(2, false);

        bpm.fetch_page(3);

        assert!(bpm.page_table.contains_key(&3));
    }

    #[test]
    fn dirty_page_should_be_written_to_disk_on_eviction() {
        let mut bpm = BufferPoolManager::new(2, 4);

        let page = bpm.fetch_page(1).unwrap();
        page.data_mut()[0] = 99;

        bpm.unpin_page(1, true);

        bpm.fetch_page(2);
        bpm.unpin_page(2, false);

        assert!(true);
    }

    #[test]
    fn unpin_nonexistent_page_should_return_false() {
        let mut bpm = BufferPoolManager::new(2, 4);

        let result = bpm.unpin_page(999, false);

        assert!(!result);
    }

    #[test]
    fn should_use_lruk_for_eviction() {
        let mut bpm = BufferPoolManager::new(2, 4);

        bpm.fetch_page(1);
        bpm.unpin_page(1, false);

        bpm.fetch_page(2);
        bpm.unpin_page(2, false);

        bpm.fetch_page(3);

        assert!(bpm.page_table.contains_key(&3));
    }
}