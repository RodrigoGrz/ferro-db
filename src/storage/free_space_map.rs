//! Free space of every page of a heap file, indexed by the page's position in the heap's
//! page list. Each value is an exclusive limit: tuples of `0..value` bytes fit the page, so
//! 0 means full and a page that only fits an empty tuple stores 1. A plain "largest tuple"
//! value could not tell those two apart.
//!
//! Stored as a max tree over a power-of-two number of leaves:
//!
//! ```text
//! tree[1]                      max of everything (root)
//! tree[i]                      max(tree[2i], tree[2i + 1])
//! tree[capacity + index]       limit of page `index` (leaves past `len` are 0, i.e. full)
//! ```
//!
//! `find` walks from the root always taking the left child when it has enough room, so it
//! returns the first page that fits in O(log n); `set` updates one leaf and its ancestors.

use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FreeSpaceMapError {
    IndexOutOfRange { index: usize, len: usize },
}

impl fmt::Display for FreeSpaceMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FreeSpaceMapError::IndexOutOfRange { index, len } => {
                write!(
                    f,
                    "free space map has {len} pages, index {index} is out of range"
                )
            }
        }
    }
}

impl std::error::Error for FreeSpaceMapError {}

#[derive(Debug, Clone, Default)]
pub(crate) struct FreeSpaceMap {
    tree: Vec<u16>,
    len: usize,
}

impl FreeSpaceMap {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Appends a page and returns its index. Doubles the leaves when full.
    pub(crate) fn push(&mut self, free: u16) -> usize {
        if self.len == self.capacity() {
            self.grow();
        }
        let index = self.len;
        self.len += 1;
        self.update_leaf(index, free);
        index
    }

    #[cfg(test)]
    pub(crate) fn get(&self, index: usize) -> Option<u16> {
        if index >= self.len {
            return None;
        }
        self.tree.get(self.capacity() + index).copied()
    }

    pub(crate) fn set(&mut self, index: usize, free: u16) -> Result<(), FreeSpaceMapError> {
        if index >= self.len {
            return Err(FreeSpaceMapError::IndexOutOfRange {
                index,
                len: self.len,
            });
        }
        self.update_leaf(index, free);
        Ok(())
    }

    /// First page (smallest index) that fits a tuple of `len` bytes.
    pub(crate) fn find(&self, len: usize) -> Option<usize> {
        let needed = len.checked_add(1)?;
        if self.len == 0 || usize::from(self.node(1)) < needed {
            return None;
        }
        let mut node = 1;
        while node < self.capacity() {
            let left = 2 * node;
            node = if usize::from(self.node(left)) >= needed {
                left
            } else {
                left + 1
            };
        }
        let index = node - self.capacity();
        // Padding leaves hold 0, so only `needed == 0` could reach them, and page 0 wins first.
        (index < self.len).then_some(index)
    }

    fn capacity(&self) -> usize {
        self.tree.len() / 2
    }

    fn node(&self, node: usize) -> u16 {
        self.tree.get(node).copied().unwrap_or_default()
    }

    fn grow(&mut self) {
        let old_capacity = self.capacity();
        let new_capacity = (old_capacity * 2).max(1);
        let mut tree = vec![0; 2 * new_capacity];
        if let (Some(new_leaves), Some(old_leaves)) = (
            tree.get_mut(new_capacity..new_capacity + old_capacity),
            self.tree.get(old_capacity..),
        ) {
            new_leaves.copy_from_slice(old_leaves);
        }
        self.tree = tree;
        for node in (1..new_capacity).rev() {
            self.recompute(node);
        }
    }

    fn update_leaf(&mut self, index: usize, free: u16) {
        let mut node = self.capacity() + index;
        if let Some(leaf) = self.tree.get_mut(node) {
            *leaf = free;
        }
        while node > 1 {
            node /= 2;
            self.recompute(node);
        }
    }

    fn recompute(&mut self, node: usize) {
        let max = self.node(2 * node).max(self.node(2 * node + 1));
        if let Some(value) = self.tree.get_mut(node) {
            *value = max;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_of(values: &[u16]) -> FreeSpaceMap {
        let mut map = FreeSpaceMap::new();
        for &free in values {
            map.push(free);
        }
        map
    }

    #[test]
    fn empty_map_should_find_nothing() {
        let map = FreeSpaceMap::new();

        assert_eq!(map.len(), 0);
        assert_eq!(map.find(0), None);
        assert_eq!(map.get(0), None);
    }

    #[test]
    fn push_should_return_increasing_indexes() {
        let mut map = FreeSpaceMap::new();

        let indexes: Vec<usize> = [5, 0, 9].into_iter().map(|free| map.push(free)).collect();

        assert_eq!(indexes, vec![0, 1, 2]);
        assert_eq!(map.len(), 3);
        assert_eq!(
            (map.get(0), map.get(1), map.get(2)),
            (Some(5), Some(0), Some(9))
        );
    }

    #[test]
    fn find_should_return_the_first_page_whose_limit_exceeds_the_length() {
        let map = map_of(&[10, 50, 20, 80, 50]);

        assert_eq!(map.find(0), Some(0));
        assert_eq!(map.find(10), Some(1));
        assert_eq!(map.find(49), Some(1));
        assert_eq!(map.find(50), Some(3));
        assert_eq!(map.find(79), Some(3));
    }

    #[test]
    fn find_should_return_none_when_no_page_fits() {
        let map = map_of(&[10, 50, 20]);

        assert_eq!(map.find(50), None);
    }

    #[test]
    fn find_zero_should_skip_full_pages() {
        let map = map_of(&[0, 0, 7]);

        assert_eq!(map.find(0), Some(2));
    }

    #[test]
    fn find_above_u16_max_should_return_none() {
        let map = map_of(&[u16::MAX]);

        assert_eq!(map.find(u16::MAX as usize - 1), Some(0));
        assert_eq!(map.find(u16::MAX as usize), None);
    }

    #[test]
    fn find_should_return_none_for_lengths_that_cannot_fit() {
        let map = map_of(&[u16::MAX, 10]);

        assert_eq!(map.find(usize::MAX), None);
    }

    #[test]
    fn set_should_raise_and_lower_a_page() {
        let mut map = map_of(&[10, 20, 30]);

        map.set(0, 40).unwrap();
        assert_eq!(map.find(39), Some(0));

        map.set(0, 0).unwrap();
        map.set(2, 5).unwrap();
        assert_eq!(map.find(20), None);
        assert_eq!(map.find(19), Some(1));
        assert_eq!(map.get(0), Some(0));
    }

    #[test]
    fn set_out_of_range_should_fail_without_changing_the_map() {
        let mut map = map_of(&[10, 20, 30]);

        let err = map.set(3, 99).unwrap_err();

        assert_eq!(err, FreeSpaceMapError::IndexOutOfRange { index: 3, len: 3 });
        assert_eq!(map.find(30), None);
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn padding_leaves_should_never_be_found() {
        // 3 pages -> 4 leaves: a full map must not answer from the padding leaf.
        let map = map_of(&[0, 0, 0]);

        assert_eq!(map.find(0), None);
        assert_eq!(map.get(3), None);
    }

    #[test]
    fn map_should_keep_every_value_while_growing() {
        for count in [1, 2, 3, 4, 5, 8, 9, 1000] {
            let values: Vec<u16> = (0..count).map(|i| (i * 7 % 101) as u16).collect();

            let map = map_of(&values);

            assert_eq!(map.len(), count);
            for (index, &free) in values.iter().enumerate() {
                assert_eq!(map.get(index), Some(free), "{count} pages, index {index}");
            }
            assert_eq!(map.find(100), None);
            assert_eq!(map.find(99), values.iter().position(|&free| free == 100));
        }
    }

    #[test]
    fn errors_should_describe_the_problem() {
        let err = FreeSpaceMapError::IndexOutOfRange { index: 7, len: 3 };

        assert_eq!(
            err.to_string(),
            "free space map has 3 pages, index 7 is out of range"
        );
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    fn first_fit(model: &[u16], needed: usize) -> Option<usize> {
        model.iter().position(|&free| usize::from(free) > needed)
    }

    #[test]
    fn random_operations_should_match_model() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut map = FreeSpaceMap::new();
        let mut model: Vec<u16> = Vec::new();

        for step in 0..10_000 {
            match rng.below(3) {
                0 => {
                    let free = rng.below(200) as u16;
                    assert_eq!(map.push(free), model.len());
                    model.push(free);
                }
                1 if !model.is_empty() => {
                    let index = rng.below(model.len());
                    let free = rng.below(200) as u16;
                    map.set(index, free).unwrap();
                    model[index] = free;
                }
                _ => {
                    let needed = rng.below(210);
                    assert_eq!(map.find(needed), first_fit(&model, needed), "step {step}");
                }
            }
            assert_eq!(map.len(), model.len());
        }
        for (index, &free) in model.iter().enumerate() {
            assert_eq!(map.get(index), Some(free));
        }
    }
}
