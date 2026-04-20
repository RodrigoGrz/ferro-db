use std::{collections::{HashMap, HashSet, VecDeque}};

#[derive(Debug)]
pub struct LRUKReplacer {
    k: usize,
    current_timestamp: usize,
    evictable: HashSet<usize>,
    pub history: HashMap<usize, VecDeque<usize>>,
}
impl LRUKReplacer {
    pub fn new(k: usize) -> Self {
        Self {
            k,
            current_timestamp: 0,
            evictable: HashSet::new(),
            history: HashMap::new(),
        }
    }

    pub fn record_access(&mut self, frame_id: usize) {
        self.current_timestamp += 1;

        let entry = self.history
            .entry(frame_id)
            .or_insert(VecDeque::new());

        entry.push_back(self.current_timestamp);

        if entry.len() > self.k {
            entry.pop_front();
        }
    }

    pub fn victim(&mut self) -> Option<usize> {
        let mut victim = None;
        let mut max_distance = 0;

        let mut oldest = usize::MAX;

        for (&frame_id, timestamps) in &self.history {
            if !self.evictable.contains(&frame_id) {
                continue;
            }

            if timestamps.len() < self.k {
                let first = timestamps.front().unwrap();
                if *first < oldest {
                    oldest = *first;
                    victim = Some(frame_id);
                }
            }
        }

        if let Some(v) = victim {
            self.evictable.remove(&v);
            self.history.remove(&v);
            return Some(v);
        }

        for (&frame_id, timestamps) in &self.history {
            if !self.evictable.contains(&frame_id) {
                continue;
            }

            if let Some(&kth_time) = timestamps.front() {
                let distance = self.current_timestamp - kth_time;
                
                if distance > max_distance {
                    max_distance = distance;
                    victim = Some(frame_id);
                }
            }
        }

        if let Some(v) = victim {
            self.evictable.remove(&v);
            self.history.remove(&v);
        }

        victim
    }

    pub fn pin(&mut self, frame_id: usize) {
        self.evictable.remove(&frame_id);
    }

    pub fn unpin(&mut self, frame_id: usize) {
        self.evictable.insert(frame_id);
    }
}