use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};

#[derive(Debug)]
pub enum DiskError {
    Io(io::Error),
    InvalidPageSize,
    InvalidPageData { expected: usize, actual: usize },
    OffsetOverflow { page_id: u32 },
    TornPage { page_id: u32, file_len: u64 },
}

impl fmt::Display for DiskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiskError::Io(err) => write!(f, "disk i/o error: {err}"),
            DiskError::InvalidPageSize => write!(f, "page size must be greater than zero"),
            DiskError::InvalidPageData { expected, actual } => {
                write!(
                    f,
                    "page data length {actual} does not match page size {expected}"
                )
            }
            DiskError::OffsetOverflow { page_id } => {
                write!(f, "page {page_id} offset overflows")
            }
            DiskError::TornPage { page_id, file_len } => {
                write!(f, "page {page_id} is torn; file length is {file_len}")
            }
        }
    }
}

impl std::error::Error for DiskError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DiskError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for DiskError {
    fn from(err: io::Error) -> Self {
        DiskError::Io(err)
    }
}

#[derive(Debug)]
pub struct DiskManager {
    file: File,
    page_size: usize,
}

impl DiskManager {
    pub fn new(path: &str, page_size: usize) -> Result<Self, DiskError> {
        if page_size == 0 {
            return Err(DiskError::InvalidPageSize);
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        Ok(Self { file, page_size })
    }

    pub fn read_page(&mut self, page_id: u32) -> Result<Vec<u8>, DiskError> {
        let offset = self.page_offset(page_id)?;
        let file_len = self.file.seek(SeekFrom::End(0))?;

        if offset >= file_len {
            return Ok(vec![0; self.page_size]);
        }

        let page_end = offset
            .checked_add(self.page_size as u64)
            .ok_or(DiskError::OffsetOverflow { page_id })?;
        if page_end > file_len {
            return Err(DiskError::TornPage { page_id, file_len });
        }

        self.file.seek(SeekFrom::Start(offset))?;
        let mut buffer = vec![0; self.page_size];
        self.file.read_exact(&mut buffer)?;
        Ok(buffer)
    }

    pub fn write_page(&mut self, page_id: u32, data: &[u8]) -> Result<(), DiskError> {
        if data.len() != self.page_size {
            return Err(DiskError::InvalidPageData {
                expected: self.page_size,
                actual: data.len(),
            });
        }

        let offset = self.page_offset(page_id)?;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)?;
        self.file.sync_data()?;
        Ok(())
    }

    pub fn num_pages(&mut self) -> Result<u64, DiskError> {
        let file_len = self.file.seek(SeekFrom::End(0))?;
        let page_size = self.page_size as u64;

        if file_len % page_size != 0 {
            let page_id = u32::try_from(file_len / page_size).unwrap_or(u32::MAX);
            return Err(DiskError::TornPage { page_id, file_len });
        }

        Ok(file_len / page_size)
    }

    fn page_offset(&self, page_id: u32) -> Result<u64, DiskError> {
        (page_id as u64)
            .checked_mul(self.page_size as u64)
            .ok_or(DiskError::OffsetOverflow { page_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestDb {
        path: PathBuf,
        dm: Option<DiskManager>,
    }

    impl TestDb {
        fn new(page_size: usize) -> Self {
            let path = temp_path();
            let dm = DiskManager::new(path.to_str().unwrap(), page_size).unwrap();
            Self { path, dm: Some(dm) }
        }

        fn dm(&mut self) -> &mut DiskManager {
            self.dm.as_mut().unwrap()
        }

        fn reopen(&mut self, page_size: usize) {
            self.dm.take();
            self.dm = Some(DiskManager::new(self.path.to_str().unwrap(), page_size).unwrap());
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            self.dm.take();
            let _ = fs::remove_file(&self.path);
        }
    }

    fn temp_path() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ferro-db-{}-{id}.db", std::process::id()))
    }

    fn write_raw(path: &Path, bytes: &[u8]) {
        let mut file = File::create(path).unwrap();
        file.write_all(bytes).unwrap();
    }

    #[test]
    fn write_and_read_page_should_be_consistent() {
        let mut db = TestDb::new(4);
        let data = vec![1, 2, 3, 4];

        db.dm().write_page(0, &data).unwrap();

        assert_eq!(db.dm().read_page(0).unwrap(), data);
    }

    #[test]
    fn write_should_persist_after_reopen() {
        let mut db = TestDb::new(4);
        let data = vec![9, 8, 7, 6];

        db.dm().write_page(0, &data).unwrap();
        db.reopen(4);

        assert_eq!(db.dm().read_page(0).unwrap(), data);
    }

    #[test]
    fn pages_should_not_overwrite_each_other() {
        let mut db = TestDb::new(4);

        db.dm().write_page(0, &[1, 1, 1, 1]).unwrap();
        db.dm().write_page(1, &[2, 2, 2, 2]).unwrap();
        db.dm().write_page(0, &[3, 3, 3, 3]).unwrap();

        assert_eq!(db.dm().read_page(0).unwrap(), vec![3, 3, 3, 3]);
        assert_eq!(db.dm().read_page(1).unwrap(), vec![2, 2, 2, 2]);
    }

    #[test]
    fn reading_empty_page_should_return_zeroed_buffer() {
        let mut db = TestDb::new(4);

        assert_eq!(db.dm().read_page(99).unwrap(), vec![0; 4]);
    }

    #[test]
    fn zero_page_size_should_fail_without_creating_a_file() {
        let path = temp_path();

        let err = DiskManager::new(path.to_str().unwrap(), 0).unwrap_err();

        assert!(matches!(err, DiskError::InvalidPageSize));
        assert!(!path.exists());
    }

    #[test]
    fn write_with_wrong_length_should_fail_without_writing() {
        let mut db = TestDb::new(4);

        let short = db.dm().write_page(0, &[1, 2, 3]).unwrap_err();
        let long = db.dm().write_page(1, &[1, 2, 3, 4, 5]).unwrap_err();

        assert!(matches!(
            short,
            DiskError::InvalidPageData {
                expected: 4,
                actual: 3
            }
        ));
        assert!(matches!(
            long,
            DiskError::InvalidPageData {
                expected: 4,
                actual: 5
            }
        ));
        assert_eq!(db.dm().num_pages().unwrap(), 0);
    }

    #[test]
    fn torn_file_should_fail_read_and_num_pages() {
        let path = temp_path();
        write_raw(&path, &[1, 2]);

        let mut dm = DiskManager::new(path.to_str().unwrap(), 4).unwrap();

        assert!(matches!(
            dm.read_page(0).unwrap_err(),
            DiskError::TornPage { file_len: 2, .. }
        ));
        assert!(matches!(
            dm.num_pages().unwrap_err(),
            DiskError::TornPage { file_len: 2, .. }
        ));

        drop(dm);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn num_pages_should_reflect_highest_written_page() {
        let mut db = TestDb::new(4);

        db.dm().write_page(0, &[1, 1, 1, 1]).unwrap();
        db.dm().write_page(2, &[2, 2, 2, 2]).unwrap();

        assert_eq!(db.dm().num_pages().unwrap(), 3);
        assert_eq!(db.dm().read_page(1).unwrap(), vec![0; 4]);
    }

    #[test]
    fn missing_parent_directory_should_fail_to_open() {
        let path = std::env::temp_dir().join(format!(
            "ferro-missing-{}-{}/db.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let err = DiskManager::new(path.to_str().unwrap(), 4).unwrap_err();

        assert!(matches!(err, DiskError::Io(_)));
    }

    #[test]
    fn offset_overflow_should_fail() {
        let mut db = TestDb::new(usize::MAX);

        let err = db.dm().read_page(2).unwrap_err();

        assert!(matches!(err, DiskError::OffsetOverflow { page_id: 2 }));
    }
}
