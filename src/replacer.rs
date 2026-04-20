use std::{collections::VecDeque};

#[derive(Debug)]
pub struct LRUReplacer {
    pub list: VecDeque<usize>,
}
impl LRUReplacer {
    pub fn new() -> Self {
        Self {
            list: VecDeque::new(),
        }
    }

    pub fn victim(&mut self) -> Option<usize> {
        self.list.pop_front()
    }

    pub fn pin(&mut self, frame_id: usize) {
        self.list.retain(|&x| x != frame_id);
    }

    pub fn unpin(&mut self, frame_id: usize) {
        if !self.list.contains(&frame_id) {
            self.list.push_back(frame_id);
        }
    }
}