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