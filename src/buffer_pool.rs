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
            .map(|_| Page::new(page_size))
            .collect();

        let free_list = (0..pool_size).collect();

        Self {
            pages,
            page_table: HashMap::new(),
            free_list,
            replacer: LRUKReplacer::new(2),
            disk_manager: DiskManager::new("data.db", page_size),
        }
    }

    pub fn fetch_page(&mut self, page_id: u32) -> Option<&mut Page> {
        if let Some(&frame_id) = self.page_table.get(&page_id) {
            let page = &mut self.pages[frame_id];

            page.pin_count += 1;

            self.replacer.record_access(frame_id);
            self.replacer.pin(frame_id);

            return Some(page);
        }

        let frame_id = self.free_list.pop().or_else(|| self.replacer.victim())?;

        let old_page = &mut self.pages[frame_id];

        if let Some(old_page_id) = old_page.page_id {
            if old_page.is_dirty {
                self.disk_manager
                    .write_page(old_page_id, &old_page.data);
            }

            self.page_table.remove(&old_page_id);
        }

        let data = self.disk_manager.read_page(page_id);

        let page = &mut self.pages[frame_id];

        page.reset_metadata();

        page.page_id = Some(page_id);
        page.data = data;
        page.pin_count = 1;
        page.is_dirty = false;

        self.page_table.insert(page_id, frame_id);

        self.replacer.record_access(frame_id);
        self.replacer.pin(frame_id);

        Some(page)
    }

    pub fn unpin_page(&mut self, page_id: u32, is_dirty: bool) -> bool {
        if let Some(&frame_id) = self.page_table.get(&page_id) {
            let page = &mut self.pages[frame_id];

            if page.pin_count == 0 {
                return false;
            }

            page.pin_count -= 1;

            if is_dirty {
                page.is_dirty = true;
            }

            if page.pin_count == 0 {
                self.replacer.unpin(frame_id);
            }

            return true;
        }

        false
    }
}