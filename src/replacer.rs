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
        let mut inf_candidate: Option<(usize, usize)> = None;
        let mut best_candidate: Option<(usize, usize)> = None;

        for (&frame_id, timestamps) in &self.history {
            if !self.evictable.contains(&frame_id) {
                continue;
            }

            if timestamps.len() < self.k {
                let oldest = *timestamps.front().unwrap();

                match inf_candidate {
                    None => inf_candidate = Some((frame_id, oldest)),
                    Some((fid, t)) => {
                        if oldest < t || (oldest == t && frame_id < fid) {
                            inf_candidate = Some((frame_id, oldest));
                        }
                    }
                }
            } else {
                let kth_time = *timestamps.front().unwrap();
                let distance = self.current_timestamp - kth_time;

                match best_candidate {
                    None => best_candidate = Some((frame_id, distance)),
                    Some((fid, d)) => {
                        if distance > d || (distance == d && frame_id < fid) {
                            best_candidate = Some((frame_id, distance));
                        }
                    }
                }
            }
        }

        let victim = inf_candidate
            .map(|(fid, _)| fid)
            .or_else(|| best_candidate.map(|(fid, _)| fid));

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