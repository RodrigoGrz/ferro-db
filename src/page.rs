#[derive(Debug)]
pub struct Page {
    pub page_id: Option<u32>,
    pub data: Vec<u8>,
    pub pin_count: u32,
    pub is_dirty: bool,
}

impl Page {
    pub fn new(page_size: usize) -> Self {
        Self {
            page_id: None,
            data: vec![0; page_size],
            pin_count: 0,
            is_dirty: false,
        }
    }

    pub fn reset_metadata(&mut self) {
        self.page_id = None;
        self.pin_count = 0;
        self.is_dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_new_should_initialize_correctly() {
        let page = Page::new(4);

        assert_eq!(page.page_id, None);
        assert_eq!(page.pin_count, 0);
        assert_eq!(page.is_dirty, false);
        assert_eq!(page.data.len(), 4);
        assert!(page.data.iter().all(|&b| b == 0));
    }

    #[test]
    fn reset_metadata_should_clear_fields() {
        let mut page = Page::new(4);

        page.page_id = Some(10);
        page.pin_count = 3;
        page.is_dirty = true;

        page.reset_metadata();

        assert_eq!(page.page_id, None);
        assert_eq!(page.pin_count, 0);
        assert_eq!(page.is_dirty, false);
    }
}