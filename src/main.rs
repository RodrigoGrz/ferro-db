mod disk_manager;
mod page;
mod replacer;

use std::{collections::HashMap};
use page::Page;
use disk_manager::DiskManager;
use replacer::LRUReplacer;

struct BufferPoolManager {
    pages: Vec<Page>,
    page_table: HashMap<u32, usize>,
    free_list: Vec<usize>,
    replacer: LRUReplacer,
    disk_manager: DiskManager,
}

impl BufferPoolManager {
    fn new(pool_size: usize, page_size: usize) -> Self {
        let pages = (0..pool_size)
            .map(|_| Page::new(page_size))
            .collect();

        let free_list = (0..pool_size).collect();

        Self {
            pages,
            page_table: HashMap::new(),
            free_list,
            replacer: LRUReplacer::new(),
            disk_manager: DiskManager::new("data.db", page_size),
        }
    }

    fn fetch_page(&mut self, page_id: u32) -> Option<&mut Page> {
        if let Some(&frame_id) = self.page_table.get(&page_id) {
            let page = &mut self.pages[frame_id];

            page.pin_count += 1;

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

        page.page_id = Some(page_id);
        page.data = data;
        page.pin_count = 1;
        page.is_dirty = false;

        self.page_table.insert(page_id, frame_id);

        self.replacer.pin(frame_id);

        Some(page)
    }

    fn unpin_page(&mut self, page_id: u32, is_dirty: bool) -> bool {
        if let Some(&frame_id) = self.page_table.get(&page_id) {
            let page = &mut self.pages[frame_id];

            if page.pin_count > 0 {
                page.pin_count -= 1;
            }

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

fn main() {
    let mut bpm = BufferPoolManager::new(2, 4096);

    {
        let page = bpm.fetch_page(1).unwrap();
        page.data[0] = 42;
    }
    bpm.unpin_page(1, true);

    {
        let page = bpm.fetch_page(2).unwrap();
        page.data[0] = 43;
    }
    bpm.unpin_page(2, true);

    {
        bpm.fetch_page(3);
    }
    bpm.unpin_page(3, false);

    {
        let page = bpm.fetch_page(2).unwrap();
        println!("{}", page.data[0]);
    }
    bpm.unpin_page(2, false);

    let page = bpm.fetch_page(1).unwrap();
    println!("{}", page.data[0]);
}
