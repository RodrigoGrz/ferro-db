use std::fmt;

#[derive(Debug)]
pub enum PageError {
    InvalidPageSize,
    InvalidPageData { expected: usize, actual: usize },
}

impl fmt::Display for PageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PageError::InvalidPageSize => write!(f, "page size must be greater than zero"),
            PageError::InvalidPageData { expected, actual } => {
                write!(
                    f,
                    "page data length {actual} does not match page size {expected}"
                )
            }
        }
    }
}

impl std::error::Error for PageError {}

#[derive(Debug)]
pub struct Page {
    page_id: Option<u32>,
    data: Vec<u8>,
    pin_count: u32,
    is_dirty: bool,
}

impl Page {
    pub fn new(page_size: usize) -> Result<Self, PageError> {
        if page_size == 0 {
            return Err(PageError::InvalidPageSize);
        }

        Ok(Self {
            page_id: None,
            data: vec![0; page_size],
            pin_count: 0,
            is_dirty: false,
        })
    }

    pub fn load(&mut self, page_id: u32, data: Vec<u8>) -> Result<(), PageError> {
        if data.len() != self.data.len() {
            return Err(PageError::InvalidPageData {
                expected: self.data.len(),
                actual: data.len(),
            });
        }

        self.page_id = Some(page_id);
        self.data = data;
        self.pin_count = 1;
        self.is_dirty = false;
        Ok(())
    }

    pub fn reset_metadata(&mut self) {
        self.page_id = None;
        self.pin_count = 0;
        self.is_dirty = false;
    }

    pub fn pin(&mut self) {
        self.pin_count += 1;
    }

    pub fn try_unpin(&mut self, is_dirty: bool) -> bool {
        if self.pin_count == 0 {
            return false;
        }

        self.pin_count -= 1;

        if is_dirty {
            self.is_dirty = true;
        }

        true
    }

    pub fn page_id(&self) -> Option<u32> {
        self.page_id
    }

    pub fn pin_count(&self) -> u32 {
        self.pin_count
    }

    pub fn is_dirty(&self) -> bool {
        self.is_dirty
    }

    pub fn mark_clean(&mut self) {
        self.is_dirty = false;
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_new_should_initialize_correctly() {
        let page = Page::new(4).unwrap();

        assert_eq!(page.page_id(), None);
        assert_eq!(page.pin_count(), 0);
        assert!(!page.is_dirty());
        assert_eq!(page.data().len(), 4);
        assert!(page.data().iter().all(|&b| b == 0));
    }

    #[test]
    fn page_new_should_reject_zero_size() {
        let err = Page::new(0).unwrap_err();

        assert!(matches!(err, PageError::InvalidPageSize));
    }

    #[test]
    fn reset_metadata_should_clear_fields_and_preserve_data() {
        let mut page = Page::new(4).unwrap();
        page.load(10, vec![1, 2, 3, 4]).unwrap();
        page.pin();
        page.pin();
        assert!(page.try_unpin(true));

        page.reset_metadata();

        assert_eq!(page.page_id(), None);
        assert_eq!(page.pin_count(), 0);
        assert!(!page.is_dirty());
        assert_eq!(page.data(), &[1, 2, 3, 4]);
    }

    #[test]
    fn load_should_reject_wrong_size_without_mutating() {
        let mut page = Page::new(4).unwrap();
        page.load(1, vec![9, 9, 9, 9]).unwrap();
        page.pin();
        assert!(page.try_unpin(true));

        let err = page.load(2, vec![1, 2, 3]).unwrap_err();

        assert!(matches!(
            err,
            PageError::InvalidPageData {
                expected: 4,
                actual: 3
            }
        ));
        assert_eq!(page.page_id(), Some(1));
        assert_eq!(page.pin_count(), 1);
        assert!(page.is_dirty());
        assert_eq!(page.data(), &[9, 9, 9, 9]);
    }

    #[test]
    fn load_should_replace_bytes_and_reset_pin_and_dirty() {
        let mut page = Page::new(4).unwrap();
        page.data_mut()[0] = 7;
        page.pin();
        assert!(page.try_unpin(true));

        page.load(5, vec![1, 2, 3, 4]).unwrap();

        assert_eq!(page.page_id(), Some(5));
        assert_eq!(page.pin_count(), 1);
        assert!(!page.is_dirty());
        assert_eq!(page.data(), &[1, 2, 3, 4]);
    }

    #[test]
    fn pin_and_unpin_should_track_count_and_sticky_dirty() {
        let mut page = Page::new(4).unwrap();

        page.pin();
        page.pin();
        assert_eq!(page.pin_count(), 2);

        assert!(page.try_unpin(true));
        assert_eq!(page.pin_count(), 1);
        assert!(page.is_dirty());

        assert!(page.try_unpin(false));
        assert_eq!(page.pin_count(), 0);
        assert!(page.is_dirty());

        assert!(!page.try_unpin(false));
        assert_eq!(page.pin_count(), 0);
        assert!(page.is_dirty());
    }
}
