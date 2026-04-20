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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_access_should_store_history_up_to_k() {
        let mut replacer = LRUKReplacer::new(2);

        replacer.record_access(1);
        replacer.record_access(1);
        replacer.record_access(1);

        let history = replacer.history.get(&1).unwrap();

        assert_eq!(history.len(), 2);
    }

    #[test]
    fn victim_should_ignore_pinned_frames() {
        let mut replacer = LRUKReplacer::new(2);

        replacer.record_access(1);
        replacer.record_access(2);

        replacer.unpin(1);
        replacer.unpin(2);

        replacer.pin(1);

        let victim = replacer.victim();

        assert_eq!(victim, Some(2));
    }

    #[test]
    fn victim_should_prioritize_less_than_k_accesses() {
        let mut replacer = LRUKReplacer::new(2);

        replacer.record_access(1);

        replacer.record_access(2);
        replacer.record_access(2);

        replacer.unpin(1);
        replacer.unpin(2);

        let victim = replacer.victim();

        assert_eq!(victim, Some(1));
    }

    #[test]
    fn victim_should_use_k_distance_when_all_have_k() {
        let mut replacer = LRUKReplacer::new(2);

        replacer.record_access(1);
        replacer.record_access(1);

        replacer.record_access(2);
        replacer.record_access(2);

        replacer.unpin(1);
        replacer.unpin(2);

        let victim = replacer.victim();

        assert_eq!(victim, Some(1));
    }

    #[test]
    fn victim_should_remove_frame_from_history_and_evictable() {
        let mut replacer = LRUKReplacer::new(2);

        replacer.record_access(1);
        replacer.unpin(1);

        let victim = replacer.victim().unwrap();

        assert_eq!(victim, 1);
        assert!(!replacer.history.contains_key(&1));
        assert!(!replacer.evictable.contains(&1));
    }

    #[test]
    fn victim_should_return_none_when_no_evictable_frames() {
        let mut replacer = LRUKReplacer::new(2);

        replacer.record_access(1);
        replacer.record_access(2);

        let victim = replacer.victim();

        assert_eq!(victim, None);
    }

    #[test]
    fn victim_should_break_tie_by_lower_frame_id() {
        let mut replacer = LRUKReplacer::new(2);

        replacer.record_access(1);
        replacer.record_access(2);

        replacer.unpin(1);
        replacer.unpin(2);

        let victim = replacer.victim();

        assert_eq!(victim, Some(1));
    }

    #[test]
    fn pin_and_unpin_should_control_evictable_state() {
        let mut replacer = LRUKReplacer::new(2);

        replacer.record_access(1);

        replacer.unpin(1);
        assert!(replacer.evictable.contains(&1));

        replacer.pin(1);
        assert!(!replacer.evictable.contains(&1));
    }
}