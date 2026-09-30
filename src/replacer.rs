use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

#[derive(Debug)]
pub enum ReplacerError {
    InvalidK,
}

impl fmt::Display for ReplacerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplacerError::InvalidK => write!(f, "replacer k must be greater than zero"),
        }
    }
}

impl std::error::Error for ReplacerError {}

#[derive(Debug)]
pub struct LRUKReplacer {
    k: usize,
    current_timestamp: usize,
    evictable: HashSet<usize>,
    history: HashMap<usize, VecDeque<usize>>,
}

impl LRUKReplacer {
    pub fn new(k: usize) -> Result<Self, ReplacerError> {
        if k == 0 {
            return Err(ReplacerError::InvalidK);
        }

        Ok(Self {
            k,
            current_timestamp: 0,
            evictable: HashSet::new(),
            history: HashMap::new(),
        })
    }

    pub fn record_access(&mut self, frame_id: usize) {
        self.current_timestamp += 1;

        let entry = self.history.entry(frame_id).or_default();

        entry.push_back(self.current_timestamp);

        if entry.len() > self.k {
            entry.pop_front();
        }
    }

    pub fn select_victim(&self) -> Option<usize> {
        preferred_victim(
            &self.history,
            &self.evictable,
            self.k,
            self.current_timestamp,
        )
    }

    pub fn forget(&mut self, frame_id: usize) {
        self.evictable.remove(&frame_id);
        self.history.remove(&frame_id);
    }

    pub fn victim(&mut self) -> Option<usize> {
        let frame_id = self.select_victim()?;
        self.forget(frame_id);
        Some(frame_id)
    }

    pub fn pin(&mut self, frame_id: usize) {
        self.evictable.remove(&frame_id);
    }

    pub fn unpin(&mut self, frame_id: usize) {
        if self.history.contains_key(&frame_id) {
            self.evictable.insert(frame_id);
        }
    }
}

fn preferred_victim(
    history: &HashMap<usize, VecDeque<usize>>,
    evictable: &HashSet<usize>,
    k: usize,
    current_timestamp: usize,
) -> Option<usize> {
    let mut inf_candidate: Option<(usize, usize)> = None;
    let mut best_candidate: Option<(usize, usize)> = None;

    for (&frame_id, timestamps) in history {
        if !evictable.contains(&frame_id) {
            continue;
        }

        let Some(&oldest) = timestamps.front() else {
            continue;
        };

        if timestamps.len() < k {
            match inf_candidate {
                None => inf_candidate = Some((frame_id, oldest)),
                Some((fid, t)) => {
                    if oldest < t || (oldest == t && frame_id < fid) {
                        inf_candidate = Some((frame_id, oldest));
                    }
                }
            }
        } else {
            let distance = current_timestamp.saturating_sub(oldest);

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

    inf_candidate
        .map(|(fid, _)| fid)
        .or_else(|| best_candidate.map(|(fid, _)| fid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_should_reject_zero_k() {
        let err = LRUKReplacer::new(0).unwrap_err();

        assert!(matches!(err, ReplacerError::InvalidK));
    }

    #[test]
    fn record_access_should_store_history_up_to_k() {
        let mut replacer = LRUKReplacer::new(2).unwrap();

        replacer.record_access(1);
        replacer.record_access(1);
        replacer.record_access(1);

        let history = replacer.history.get(&1).unwrap();

        assert_eq!(history.len(), 2);
    }

    #[test]
    fn victim_should_ignore_pinned_frames() {
        let mut replacer = LRUKReplacer::new(2).unwrap();

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
        let mut replacer = LRUKReplacer::new(2).unwrap();

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
        let mut replacer = LRUKReplacer::new(2).unwrap();

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
    fn victim_with_k_one_should_evict_least_recently_used() {
        let mut replacer = LRUKReplacer::new(1).unwrap();

        replacer.record_access(1);
        replacer.record_access(2);

        replacer.unpin(1);
        replacer.unpin(2);

        assert_eq!(replacer.victim(), Some(1));
    }

    #[test]
    fn victim_should_evict_cold_page_with_older_first_access() {
        let mut replacer = LRUKReplacer::new(3).unwrap();

        replacer.record_access(1);
        replacer.record_access(2);
        replacer.record_access(1);

        replacer.unpin(1);
        replacer.unpin(2);

        assert_eq!(replacer.victim(), Some(1));
    }

    #[test]
    fn preferred_victim_should_break_timestamp_tie_by_lower_frame_id() {
        let mut history = HashMap::new();
        history.insert(2, VecDeque::from([5, 9]));
        history.insert(1, VecDeque::from([5, 8]));

        let evictable = HashSet::from([1, 2]);

        assert_eq!(preferred_victim(&history, &evictable, 2, 10), Some(1));

        let mut cold = HashMap::new();
        cold.insert(4, VecDeque::from([3]));
        cold.insert(2, VecDeque::from([3]));

        let cold_evictable = HashSet::from([4, 2]);

        assert_eq!(preferred_victim(&cold, &cold_evictable, 2, 10), Some(2));
    }

    #[test]
    fn victim_should_remove_frame_from_history_and_evictable() {
        let mut replacer = LRUKReplacer::new(2).unwrap();

        replacer.record_access(1);
        replacer.unpin(1);

        let victim = replacer.victim().unwrap();

        assert_eq!(victim, 1);
        assert!(!replacer.history.contains_key(&1));
        assert!(!replacer.evictable.contains(&1));
    }

    #[test]
    fn second_victim_should_not_return_removed_frame() {
        let mut replacer = LRUKReplacer::new(2).unwrap();

        replacer.record_access(1);
        replacer.unpin(1);

        assert_eq!(replacer.victim(), Some(1));
        assert_eq!(replacer.victim(), None);
    }

    #[test]
    fn select_victim_should_not_remove_frame() {
        let mut replacer = LRUKReplacer::new(2).unwrap();

        replacer.record_access(1);
        replacer.unpin(1);

        assert_eq!(replacer.select_victim(), Some(1));
        assert_eq!(replacer.select_victim(), Some(1));

        replacer.forget(1);

        assert_eq!(replacer.select_victim(), None);
    }

    #[test]
    fn victim_should_return_none_when_no_evictable_frames() {
        let mut replacer = LRUKReplacer::new(2).unwrap();

        replacer.record_access(1);
        replacer.record_access(2);

        let victim = replacer.victim();

        assert_eq!(victim, None);
    }

    #[test]
    fn unpin_without_access_should_not_be_evictable() {
        let mut replacer = LRUKReplacer::new(2).unwrap();

        replacer.unpin(1);

        assert!(replacer.evictable.is_empty());
        assert_eq!(replacer.victim(), None);
    }

    #[test]
    fn pin_and_unpin_should_control_evictable_state() {
        let mut replacer = LRUKReplacer::new(2).unwrap();

        replacer.record_access(1);

        replacer.unpin(1);
        assert!(replacer.evictable.contains(&1));

        replacer.pin(1);
        assert!(!replacer.evictable.contains(&1));
    }
}
