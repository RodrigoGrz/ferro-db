use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

pub struct DiskManager {
    pub file: File,
    pub page_size: usize,
}

impl DiskManager {
    pub fn new(path: &str, page_size: usize) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .expect("Failed to open file");

        Self { file, page_size }
    }

    pub fn read_page(&mut self, page_id: u32) -> Vec<u8> {
        let mut buffer = vec![0; self.page_size];

        let offset = page_id as u64 * self.page_size as u64;

        self.file
            .seek(SeekFrom::Start(offset))
            .expect("Seek failed");

        let _ = self.file.read(&mut buffer);

        buffer
    }

    pub fn write_page(&mut self, page_id: u32, data: &[u8]) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn write_and_read_page_should_be_consistent() {
        let path = "test.db";

        let mut dm = DiskManager::new(path, 4);

        let data = vec![1, 2, 3, 4];

        dm.write_page(0, &data);

        let result = dm.read_page(0);

        assert_eq!(result, data);

        fs::remove_file(path).ok();
    }

    #[test]
    fn pages_should_not_overwrite_each_other() {
        let path = "test.db";
        let mut dm = DiskManager::new(path, 4);

        dm.write_page(0, &[1, 1, 1, 1]);
        dm.write_page(1, &[2, 2, 2, 2]);

        assert_eq!(dm.read_page(0), vec![1, 1, 1, 1]);
        assert_eq!(dm.read_page(1), vec![2, 2, 2, 2]);

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn reading_empty_page_should_return_zeroed_buffer() {
        let path = "test.db";
        let mut dm = DiskManager::new(path, 4);

        let data = dm.read_page(99);

        assert_eq!(data, vec![0; 4]);

        std::fs::remove_file(path).ok();
    }
}