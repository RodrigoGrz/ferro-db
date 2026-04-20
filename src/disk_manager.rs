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