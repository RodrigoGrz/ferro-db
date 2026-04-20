use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::{collections::{HashMap, VecDeque}};

#[derive(Debug)]
struct Page {
    page_id: Option<u32>,
    data: Vec<u8>,
    pin_count: u32,
    is_dirty: bool,
}

impl Page {
    fn new(page_size: usize) -> Self {
        Self {
            page_id: None,
            data: vec![0; page_size],
            pin_count: 0,
            is_dirty: false,
        }
    }

    fn reset(&mut self) {
        self.page_id = None;
        self.data.fill(0);
        self.pin_count = 0;
        self.is_dirty = false;
    }
}

#[derive(Debug)]
struct LRUReplacer {
    list: VecDeque<usize>,
}
impl LRUReplacer {
    fn new() -> Self {
        Self {
            list: VecDeque::new(),
        }
    }

    fn victim(&mut self) -> Option<usize> {
        self.list.pop_front()
    }

    fn pin(&mut self, frame_id: usize) {
        self.list.retain(|&x| x != frame_id);
    }

    fn unpin(&mut self, frame_id: usize) {
        if !self.list.contains(&frame_id) {
            self.list.push_back(frame_id);
        }
    }
}

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

struct DiskManager {
    file: File,
    page_size: usize,
}

impl DiskManager {
    fn new(path: &str, page_size: usize) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .expect("Failed to open file");

        Self { file, page_size }
    }

    fn read_page(&mut self, page_id: u32) -> Vec<u8> {
        let mut buffer = vec![0; self.page_size];

        let offset = page_id as u64 * self.page_size as u64;

        self.file
            .seek(SeekFrom::Start(offset))
            .expect("Seek failed");

        let _ = self.file.read(&mut buffer);

        buffer
    }

    fn write_page(&mut self, page_id: u32, data: &[u8]) {
        let offset = page_id as u64 * self.page_size as u64;

        self.file
            .seek(SeekFrom::Start(offset))
            .expect("Seek failed");

        self.file
            .write_all(data)
            .expect("Write failed");

        self.file.flush().expect("Flush failed");
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
        bpm.fetch_page(2);
    }
    bpm.unpin_page(2, false);

    {
        bpm.fetch_page(3);
    }
    bpm.unpin_page(3, false);

    let page = bpm.fetch_page(1).unwrap();
    println!("{}", page.data[0]);
}
