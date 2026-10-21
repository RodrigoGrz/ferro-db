//! B+Tree (CMU 15-445): a unique index from keys of one type to row ids. Internal nodes only
//! guide the search; leaves hold every key with its `Rid`, in order, linked left to right.
//! Keys are compared as bytes (`key.rs`), and nodes have variable-size entries (`node.rs`).
//!
//! The tree is identified by its header page, which points to the root: the root changes on
//! splits and merges, the header never does.
//!
//! **Failing without side effects.** An operation computes every page it changes in memory
//! first (`PageWrites`), then pins all of them, encodes each image into a copy of its page and
//! only then copies the images into the frames. Until that last step nothing has changed; the
//! last step cannot fail, because every page is pinned and resident. The one exception is a
//! page taken from `BufferPoolManager::new_page` by an operation that then fails: it stays
//! allocated, zeroed, with no owner (as in the heap file).

use std::collections::BTreeMap;
use std::ops::Bound;

use crate::storage::buffer_pool::BufferPoolManager;
use crate::storage::page::RESERVED_PAGE_ID;
use crate::storage::rid::Rid;
use crate::types::{DataType, Value};

use super::IndexError;
use super::key::{decode_key, encode_key, key_type_code, key_type_from_code};
use super::node::{
    InternalEntry, LeafEntry, Node, TreeHeader, internal_entry_size, leaf_entry_size,
    max_key_size_for, read_free_page, write_free_page,
};

/// A node must hold four keys of the widest fixed type (`BIGINT`), so smaller pages are refused.
const MIN_KEY_SIZE: usize = 8;

#[derive(Debug)]
pub struct BPlusTree {
    header_page_id: u32,
    /// Cached; the header page is the source of truth on `open`.
    root_page_id: u32,
    key_type: DataType,
    max_key_size: usize,
}

impl BPlusTree {
    /// A header page and an empty leaf as the root.
    pub fn create(bpm: &mut BufferPoolManager, key_type: DataType) -> Result<Self, IndexError> {
        let max_key_size = supported_max_key_size(bpm)?;
        let header_page_id = allocate_new_page(bpm)?;
        let root_page_id = allocate_new_page(bpm)?;
        let mut writes = PageWrites::default();
        writes.node(
            root_page_id,
            Node::Leaf {
                next: 0,
                entries: Vec::new(),
            },
        );
        writes.header(
            header_page_id,
            TreeHeader {
                root_page_id,
                free_list_head: 0,
                key_type: key_type_code(key_type),
            },
        );
        writes.apply(bpm)?;
        Ok(Self {
            header_page_id,
            root_page_id,
            key_type,
            max_key_size,
        })
    }

    /// The caller names the key type, as it gives a table heap its schema; it must be the type
    /// the tree was created with (any `VARCHAR(n)` for a `VARCHAR` tree).
    pub fn open(
        bpm: &mut BufferPoolManager,
        key_type: DataType,
        header_page_id: u32,
    ) -> Result<Self, IndexError> {
        let max_key_size = supported_max_key_size(bpm)?;
        let header = read_page(bpm, header_page_id, |page| {
            TreeHeader::read(header_page_id, page)
        })?;
        let stored = key_type_from_code(header.key_type).ok_or(IndexError::Corrupted {
            page_id: header_page_id,
            reason: "unknown key type",
        })?;
        if header.key_type != key_type_code(key_type) {
            return Err(IndexError::KeyTypeMismatch {
                expected: stored,
                found: key_type,
            });
        }
        Ok(Self {
            header_page_id,
            root_page_id: header.root_page_id,
            key_type,
            max_key_size,
        })
    }

    pub fn header_page_id(&self) -> u32 {
        self.header_page_id
    }

    pub fn key_type(&self) -> DataType {
        self.key_type
    }

    pub fn max_key_size(&self) -> usize {
        self.max_key_size
    }

    /// A key too large for the tree cannot be in it: `None`, not an error.
    pub fn get(&self, bpm: &mut BufferPoolManager, key: &Value) -> Result<Option<Rid>, IndexError> {
        let key = encode_key(self.key_type, key)?;
        if key.len() > self.max_key_size {
            return Ok(None);
        }
        let lookup = self.find_in_leaf(bpm, &key)?;
        Ok(lookup
            .position
            .ok()
            .and_then(|index| lookup.entries.get(index))
            .map(|entry| entry.rid))
    }

    /// Keys in order within the bounds; `range(bpm, Unbounded, Unbounded)` is a full scan.
    pub fn range<'a>(
        &'a self,
        bpm: &'a mut BufferPoolManager,
        from: Bound<&Value>,
        to: Bound<&Value>,
    ) -> Result<BPlusTreeRange<'a>, IndexError> {
        let lower = self.encode_bound(from)?;
        let upper = self.encode_bound(to)?;
        let start: &[u8] = match &lower {
            Bound::Included(key) | Bound::Excluded(key) => key,
            Bound::Unbounded => &[],
        };
        let Some(PathStep {
            page_id,
            node: Node::Leaf { next, entries },
            ..
        }) = self.descend(bpm, start)?.pop()
        else {
            return Err(not_a_leaf(self.root_page_id));
        };
        let skipped = entries.partition_point(|entry| is_below(&entry.key, &lower));
        let last_key = entries.last().map(|entry| entry.key.clone());
        let mut entries = entries.into_iter();
        if let Some(last_skipped) = skipped.checked_sub(1) {
            entries.nth(last_skipped);
        }
        Ok(BPlusTreeRange {
            bpm,
            key_type: self.key_type,
            max_key_size: self.max_key_size,
            leaf_id: page_id,
            entries,
            leaf_last_key: last_key,
            next_leaf: next,
            upper,
            is_done: false,
        })
    }

    fn encode_bound(&self, bound: Bound<&Value>) -> Result<Bound<Vec<u8>>, IndexError> {
        Ok(match bound {
            Bound::Included(key) => Bound::Included(encode_key(self.key_type, key)?),
            Bound::Excluded(key) => Bound::Excluded(encode_key(self.key_type, key)?),
            Bound::Unbounded => Bound::Unbounded,
        })
    }

    /// `DuplicateKey` (23505) when the key is already there; nothing changes then.
    pub fn insert(
        &mut self,
        bpm: &mut BufferPoolManager,
        key: &Value,
        rid: Rid,
    ) -> Result<(), IndexError> {
        let encoded = self.insertable_key(key)?;
        let LeafLookup {
            path,
            leaf_id,
            next,
            mut entries,
            position,
        } = self.find_in_leaf(bpm, &encoded)?;
        let Err(position) = position else {
            return Err(IndexError::duplicate_key(key));
        };
        entries.insert(position, LeafEntry { key: encoded, rid });
        let leaf = Node::Leaf { next, entries };

        let mut writes = PageWrites::default();
        let mut header = None;
        let new_root = self.split_up(bpm, &mut writes, &mut header, path, leaf_id, leaf)?;
        if let Some(mut header) = header {
            if let Some(root_page_id) = new_root {
                header.root_page_id = root_page_id;
            }
            writes.header(self.header_page_id, header);
        }
        writes.apply(bpm)?;
        if let Some(root_page_id) = new_root {
            self.root_page_id = root_page_id;
        }
        Ok(())
    }

    /// Whether `key` could be inserted, without reading a page: not `NULL`, of the tree's
    /// type and not longer than `max_key_size`. A caller can check a whole batch before its
    /// first change.
    pub fn check_key(&self, key: &Value) -> Result<(), IndexError> {
        self.insertable_key(key).map(|_| ())
    }

    fn insertable_key(&self, key: &Value) -> Result<Vec<u8>, IndexError> {
        let encoded = encode_key(self.key_type, key)?;
        if encoded.len() > self.max_key_size {
            return Err(IndexError::KeyTooLarge {
                size: encoded.len(),
                max: self.max_key_size,
            });
        }
        Ok(encoded)
    }

    /// The removed `Rid`, or `None` when the key was not there (a key too large for the tree
    /// cannot be in it).
    pub fn remove(
        &mut self,
        bpm: &mut BufferPoolManager,
        key: &Value,
    ) -> Result<Option<Rid>, IndexError> {
        let encoded = encode_key(self.key_type, key)?;
        if encoded.len() > self.max_key_size {
            return Ok(None);
        }
        let LeafLookup {
            path,
            leaf_id,
            next,
            mut entries,
            position,
        } = self.find_in_leaf(bpm, &encoded)?;
        let Ok(position) = position else {
            return Ok(None);
        };
        let removed = entries.remove(position);
        let leaf = Node::Leaf { next, entries };

        let mut writes = PageWrites::default();
        let mut freed = Vec::new();
        let new_root = self.rebalance_up(bpm, &mut writes, &mut freed, path, leaf_id, leaf)?;
        if new_root.is_some() || !freed.is_empty() {
            let mut header = self.read_header(bpm)?;
            for page_id in freed {
                writes.free(page_id, header.free_list_head);
                header.free_list_head = page_id;
            }
            if let Some(root_page_id) = new_root {
                header.root_page_id = root_page_id;
            }
            writes.header(self.header_page_id, header);
        }
        writes.apply(bpm)?;
        if let Some(root_page_id) = new_root {
            self.root_page_id = root_page_id;
        }
        Ok(Some(removed.rid))
    }

    /// Writes `node` back after a removal. A node under a quarter full (other than the root) is
    /// joined with a sibling and the separator between them: if that fits one node, the two
    /// merge, the right page is freed and the parent loses a separator (and may underflow in
    /// turn); otherwise the joined entries are split again at half of their bytes, which moves
    /// entries from the sibling, and the parent gets the new separator. When that separator
    /// does not fit the parent, the node is left under a quarter full: it only costs space. A root
    /// left with one child is replaced by it. Returns the new root, if any.
    fn rebalance_up(
        &self,
        bpm: &mut BufferPoolManager,
        writes: &mut PageWrites,
        freed: &mut Vec<u32>,
        mut path: Vec<PathStep>,
        mut page_id: u32,
        mut node: Node,
    ) -> Result<Option<u32>, IndexError> {
        let page_size = bpm.page_size();
        loop {
            let Some(PathStep {
                page_id: parent_id,
                node: mut parent,
                child_index,
            }) = path.pop()
            else {
                if let Node::Internal {
                    first_child,
                    entries,
                    ..
                } = &node
                    && entries.is_empty()
                {
                    freed.push(page_id);
                    return Ok(Some(*first_child));
                }
                writes.node(page_id, node);
                return Ok(None);
            };
            if !node.is_underfull(page_size) {
                writes.node(page_id, node);
                return Ok(None);
            }
            let Some(siblings) = Siblings::of(&parent, child_index) else {
                writes.node(page_id, node);
                return Ok(None);
            };
            let sibling_id = siblings
                .sibling_page(&parent)
                .ok_or_else(|| not_internal(parent_id))?;
            let sibling = read_page(bpm, sibling_id, |page| {
                Node::read(sibling_id, page, self.max_key_size)
            })?;
            let underfull = node.clone();
            let (left_id, left, right_id, right) = if siblings.is_left {
                (sibling_id, sibling, page_id, node)
            } else {
                (page_id, node, sibling_id, sibling)
            };
            let separator = siblings
                .separator(&parent)
                .ok_or_else(|| not_internal(parent_id))?;
            let joined = join(left_id, left, separator, right)?;

            if joined.fits(page_size) {
                writes.node(left_id, joined);
                freed.push(right_id);
                siblings
                    .remove_separator(&mut parent)
                    .ok_or_else(|| not_internal(parent_id))?;
                page_id = parent_id;
                node = parent;
                continue;
            }
            let (new_left, new_separator, new_right) = split(left_id, joined, right_id)?;
            siblings
                .replace_separator(&mut parent, new_separator)
                .ok_or_else(|| not_internal(parent_id))?;
            if parent.fits(page_size) {
                writes.node(left_id, new_left);
                writes.node(right_id, new_right);
                writes.node(parent_id, parent);
            } else {
                writes.node(page_id, underfull);
            }
            return Ok(None);
        }
    }

    /// Writes `node` back, splitting it while it does not fit: each split sends a separator
    /// and the new right page up to the parent, and a split of the root makes a new root.
    /// Returns the new root, if any.
    fn split_up(
        &self,
        bpm: &mut BufferPoolManager,
        writes: &mut PageWrites,
        header: &mut Option<TreeHeader>,
        mut path: Vec<PathStep>,
        mut page_id: u32,
        mut node: Node,
    ) -> Result<Option<u32>, IndexError> {
        let page_size = bpm.page_size();
        let mut new_root = None;
        while !node.fits(page_size) {
            let level = node.level();
            let right_page_id = self.allocate(bpm, header)?;
            let (left, separator, right) = split(page_id, node, right_page_id)?;
            writes.node(page_id, left);
            writes.node(right_page_id, right);
            let entry = InternalEntry {
                key: separator,
                child: right_page_id,
            };
            match path.pop() {
                Some(PathStep {
                    page_id: parent_id,
                    node: mut parent,
                    child_index,
                }) => {
                    let Node::Internal { entries, .. } = &mut parent else {
                        return Err(not_internal(parent_id));
                    };
                    if child_index > entries.len() {
                        return Err(not_internal(parent_id));
                    }
                    entries.insert(child_index, entry);
                    page_id = parent_id;
                    node = parent;
                }
                None => {
                    let root_level = level_above(page_id, level)?;
                    let root_page_id = self.allocate(bpm, header)?;
                    node = Node::Internal {
                        level: root_level,
                        first_child: page_id,
                        entries: vec![entry],
                    };
                    page_id = root_page_id;
                    new_root = Some(root_page_id);
                }
            }
        }
        writes.node(page_id, node);
        Ok(new_root)
    }

    /// A page for a new node: the first free page of the tree, or a new one from the buffer
    /// pool. Taking a free page changes `header` (loaded on first use), which the caller writes.
    fn allocate(
        &self,
        bpm: &mut BufferPoolManager,
        header: &mut Option<TreeHeader>,
    ) -> Result<u32, IndexError> {
        let header = match header {
            Some(header) => header,
            None => header.insert(self.read_header(bpm)?),
        };
        let page_id = header.free_list_head;
        if page_id == RESERVED_PAGE_ID {
            return allocate_new_page(bpm);
        }
        header.free_list_head = read_page(bpm, page_id, |page| read_free_page(page_id, page))?;
        Ok(page_id)
    }

    fn read_header(&self, bpm: &mut BufferPoolManager) -> Result<TreeHeader, IndexError> {
        read_page(bpm, self.header_page_id, |page| {
            TreeHeader::read(self.header_page_id, page)
        })
    }

    /// The leaf where `key` belongs and where the key is (`Ok`) or would go (`Err`) in it.
    fn find_in_leaf(
        &self,
        bpm: &mut BufferPoolManager,
        key: &[u8],
    ) -> Result<LeafLookup, IndexError> {
        let mut path = self.descend(bpm, key)?;
        let Some(PathStep {
            page_id: leaf_id,
            node: Node::Leaf { next, entries },
            ..
        }) = path.pop()
        else {
            return Err(not_a_leaf(self.root_page_id));
        };
        let position = entries.binary_search_by(|entry| entry.key.as_slice().cmp(key));
        Ok(LeafLookup {
            path,
            leaf_id,
            next,
            entries,
            position,
        })
    }

    /// The nodes from the root down to the leaf where `key` belongs. Each child must be one
    /// level below its parent, so a damaged page can never send the descent into a loop.
    fn descend(
        &self,
        bpm: &mut BufferPoolManager,
        key: &[u8],
    ) -> Result<Vec<PathStep>, IndexError> {
        let mut path = Vec::new();
        let mut page_id = self.root_page_id;
        let mut expected_level = None;
        loop {
            let node = read_page(bpm, page_id, |page| {
                Node::read(page_id, page, self.max_key_size)
            })?;
            if expected_level.is_some_and(|level| level != node.level()) {
                return Err(IndexError::Corrupted {
                    page_id,
                    reason: "child is not one level below its parent",
                });
            }
            let next = match &node {
                Node::Leaf { .. } => None,
                Node::Internal {
                    level,
                    first_child,
                    entries,
                } => {
                    let child_index = entries.partition_point(|entry| entry.key.as_slice() <= key);
                    let child = match child_index.checked_sub(1) {
                        None => *first_child,
                        Some(entry) => entries.get(entry).map_or(*first_child, |entry| entry.child),
                    };
                    Some((child, level - 1, child_index))
                }
            };
            path.push(PathStep {
                page_id,
                node,
                child_index: next.map_or(0, |(_, _, child_index)| child_index),
            });
            let Some((child, level, _)) = next else {
                return Ok(path);
            };
            page_id = child;
            expected_level = Some(level);
        }
    }
}

/// Copies one leaf at a time and unpins it before yielding, so dropping the iterator midway
/// leaves no page pinned. An error is yielded once and ends the iteration.
pub struct BPlusTreeRange<'a> {
    bpm: &'a mut BufferPoolManager,
    key_type: DataType,
    max_key_size: usize,
    leaf_id: u32,
    entries: std::vec::IntoIter<LeafEntry>,
    /// The largest key of the current leaf: the next leaf must start after it, which also
    /// stops a damaged chain that loops back.
    leaf_last_key: Option<Vec<u8>>,
    next_leaf: u32,
    upper: Bound<Vec<u8>>,
    is_done: bool,
}

impl BPlusTreeRange<'_> {
    fn advance(&mut self) -> Result<Option<(Value, Rid)>, IndexError> {
        loop {
            if let Some(entry) = self.entries.next() {
                if !is_within(&entry.key, &self.upper) {
                    return Ok(None);
                }
                let key = decode_key(self.key_type, &entry.key).map_err(|reason| {
                    IndexError::Corrupted {
                        page_id: self.leaf_id,
                        reason,
                    }
                })?;
                return Ok(Some((key, entry.rid)));
            }
            if self.next_leaf == RESERVED_PAGE_ID {
                return Ok(None);
            }
            self.load_next_leaf()?;
        }
    }

    fn load_next_leaf(&mut self) -> Result<(), IndexError> {
        let page_id = self.next_leaf;
        let max_key_size = self.max_key_size;
        let node = read_page(self.bpm, page_id, |page| {
            Node::read(page_id, page, max_key_size)
        })?;
        let corrupted = |reason| IndexError::Corrupted { page_id, reason };
        let Node::Leaf { next, entries } = node else {
            return Err(corrupted("a leaf links to a page that is not a leaf"));
        };
        let first_key = entries
            .first()
            .map(|entry| entry.key.as_slice())
            .ok_or_else(|| corrupted("a linked leaf is empty"))?;
        if self
            .leaf_last_key
            .as_deref()
            .is_some_and(|last_key| first_key <= last_key)
        {
            return Err(corrupted("leaves are not linked in key order"));
        }
        self.leaf_last_key = entries.last().map(|entry| entry.key.clone());
        self.leaf_id = page_id;
        self.next_leaf = next;
        self.entries = entries.into_iter();
        Ok(())
    }
}

impl Iterator for BPlusTreeRange<'_> {
    type Item = Result<(Value, Rid), IndexError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.is_done {
            return None;
        }
        let result = self.advance().transpose();
        self.is_done = !matches!(result, Some(Ok(_)));
        result
    }
}

fn is_below(key: &[u8], lower: &Bound<Vec<u8>>) -> bool {
    match lower {
        Bound::Included(lower) => key < lower.as_slice(),
        Bound::Excluded(lower) => key <= lower.as_slice(),
        Bound::Unbounded => false,
    }
}

fn is_within(key: &[u8], upper: &Bound<Vec<u8>>) -> bool {
    match upper {
        Bound::Included(upper) => key <= upper.as_slice(),
        Bound::Excluded(upper) => key < upper.as_slice(),
        Bound::Unbounded => true,
    }
}

/// A leaf taken apart for a change, with the descent above it.
struct LeafLookup {
    path: Vec<PathStep>,
    leaf_id: u32,
    next: u32,
    entries: Vec<LeafEntry>,
    position: Result<usize, usize>,
}

/// One node of a descent and which of its children the descent took (`0` = the first child).
#[derive(Debug)]
struct PathStep {
    page_id: u32,
    node: Node,
    child_index: usize,
}

/// A child of an internal node, the sibling it rebalances with (the left one when there is
/// one) and the separator between the two.
struct Siblings {
    sibling_index: usize,
    separator_index: usize,
    is_left: bool,
}

impl Siblings {
    /// `None` when the parent has a single child.
    fn of(parent: &Node, child_index: usize) -> Option<Siblings> {
        let Node::Internal { entries, .. } = parent else {
            return None;
        };
        if entries.is_empty() {
            return None;
        }
        Some(match child_index.checked_sub(1) {
            Some(left) => Siblings {
                sibling_index: left,
                separator_index: left,
                is_left: true,
            },
            None => Siblings {
                sibling_index: 1,
                separator_index: 0,
                is_left: false,
            },
        })
    }

    fn sibling_page(&self, parent: &Node) -> Option<u32> {
        let Node::Internal {
            first_child,
            entries,
            ..
        } = parent
        else {
            return None;
        };
        match self.sibling_index.checked_sub(1) {
            None => Some(*first_child),
            Some(entry) => entries.get(entry).map(|entry| entry.child),
        }
    }

    fn separator(&self, parent: &Node) -> Option<Vec<u8>> {
        let Node::Internal { entries, .. } = parent else {
            return None;
        };
        entries
            .get(self.separator_index)
            .map(|entry| entry.key.clone())
    }

    /// Drops the separator and, with it, the pointer to the right sibling.
    fn remove_separator(&self, parent: &mut Node) -> Option<()> {
        let Node::Internal { entries, .. } = parent else {
            return None;
        };
        (self.separator_index < entries.len()).then(|| {
            entries.remove(self.separator_index);
        })
    }

    fn replace_separator(&self, parent: &mut Node, key: Vec<u8>) -> Option<()> {
        let Node::Internal { entries, .. } = parent else {
            return None;
        };
        entries
            .get_mut(self.separator_index)
            .map(|entry| entry.key = key)
    }
}

/// Two siblings as one node: leaves concatenate, keeping the right one's next; internal nodes
/// also take the separator down, pointing to the right one's first child.
fn join(left_id: u32, left: Node, separator: Vec<u8>, right: Node) -> Result<Node, IndexError> {
    match (left, right) {
        (
            Node::Leaf { mut entries, .. },
            Node::Leaf {
                next,
                entries: right,
            },
        ) => {
            entries.extend(right);
            Ok(Node::Leaf { next, entries })
        }
        (
            Node::Internal {
                level,
                first_child,
                mut entries,
            },
            Node::Internal {
                level: right_level,
                first_child: right_first_child,
                entries: right,
            },
        ) if level == right_level => {
            entries.push(InternalEntry {
                key: separator,
                child: right_first_child,
            });
            entries.extend(right);
            Ok(Node::Internal {
                level,
                first_child,
                entries,
            })
        }
        _ => Err(IndexError::Corrupted {
            page_id: left_id,
            reason: "siblings are not at the same level",
        }),
    }
}

/// Splits an overflowing node at half of its bytes. A leaf copies the first key of the right
/// half up as the separator and links left → right → old next; an internal node moves its
/// middle key up, and the middle child becomes the right node's first child.
fn split(
    page_id: u32,
    node: Node,
    right_page_id: u32,
) -> Result<(Node, Vec<u8>, Node), IndexError> {
    let too_few = || IndexError::Corrupted {
        page_id,
        reason: "an overflowing node has too few entries to split",
    };
    match node {
        Node::Leaf { next, mut entries } => {
            let sizes: Vec<usize> = entries
                .iter()
                .map(|entry| leaf_entry_size(entry.key.len()))
                .collect();
            let left_len = split_point(&sizes, 1).ok_or_else(too_few)?;
            let right_entries = entries.split_off(left_len);
            let separator = right_entries
                .first()
                .map(|entry| entry.key.clone())
                .ok_or_else(too_few)?;
            Ok((
                Node::Leaf {
                    next: right_page_id,
                    entries,
                },
                separator,
                Node::Leaf {
                    next,
                    entries: right_entries,
                },
            ))
        }
        Node::Internal {
            level,
            first_child,
            mut entries,
        } => {
            let sizes: Vec<usize> = entries
                .iter()
                .map(|entry| internal_entry_size(entry.key.len()))
                .collect();
            let middle = split_point(&sizes, 2).ok_or_else(too_few)?;
            let mut right_entries = entries.split_off(middle);
            if right_entries.is_empty() {
                return Err(too_few());
            }
            let middle_entry = right_entries.remove(0);
            Ok((
                Node::Internal {
                    level,
                    first_child,
                    entries,
                },
                middle_entry.key,
                Node::Internal {
                    level,
                    first_child: middle_entry.child,
                    entries: right_entries,
                },
            ))
        }
    }
}

/// How many entries go left: as many as fit in half of the bytes, keeping at least one on
/// the left and `right_min` on the right. `None` when there are too few entries for that.
fn split_point(sizes: &[usize], right_min: usize) -> Option<usize> {
    let max_left = sizes.len().checked_sub(right_min).filter(|&max| max >= 1)?;
    let half = sizes.iter().sum::<usize>() / 2;
    let mut taken = 0;
    let fitting = sizes
        .iter()
        .take_while(|&&size| {
            taken += size;
            taken <= half
        })
        .count();
    Some(fitting.clamp(1, max_left))
}

/// The level of a new root above `level`. Levels come from the disk, so even this is checked.
fn level_above(page_id: u32, level: u16) -> Result<u16, IndexError> {
    level.checked_add(1).ok_or(IndexError::Corrupted {
        page_id,
        reason: "the tree has no level left above its root",
    })
}

fn not_internal(page_id: u32) -> IndexError {
    IndexError::Corrupted {
        page_id,
        reason: "the parent of a node is not an internal node",
    }
}

fn not_a_leaf(page_id: u32) -> IndexError {
    IndexError::Corrupted {
        page_id,
        reason: "the descent did not end at a leaf",
    }
}

fn supported_max_key_size(bpm: &BufferPoolManager) -> Result<usize, IndexError> {
    let size = bpm.page_size();
    let max_key_size = max_key_size_for(size);
    if max_key_size < MIN_KEY_SIZE {
        return Err(IndexError::UnsupportedPageSize { size });
    }
    Ok(max_key_size)
}

/// A page from the buffer pool, unpinned right away: `PageWrites::apply` pins it again when it
/// is written.
fn allocate_new_page(bpm: &mut BufferPoolManager) -> Result<u32, IndexError> {
    let (page_id, _) = bpm.new_page()?;
    bpm.unpin_page(page_id, true);
    Ok(page_id)
}

fn read_page<T>(
    bpm: &mut BufferPoolManager,
    page_id: u32,
    read: impl FnOnce(&[u8]) -> Result<T, IndexError>,
) -> Result<T, IndexError> {
    let page = bpm.fetch_page(page_id)?;
    let result = read(page.data());
    bpm.unpin_page(page_id, false);
    result
}

#[derive(Debug)]
enum PageImage {
    Node(Node),
    Free { next: u32 },
    Header(TreeHeader),
}

impl PageImage {
    fn write(&self, page_id: u32, page: &mut [u8]) -> Result<(), IndexError> {
        match self {
            PageImage::Node(node) => node.write(page_id, page),
            PageImage::Free { next } => write_free_page(page_id, page, *next),
            PageImage::Header(header) => header.write(page_id, page),
        }
    }
}

/// Every page one operation changes, keyed by page id, so each page is written once.
#[derive(Debug, Default)]
struct PageWrites {
    pages: BTreeMap<u32, PageImage>,
}

impl PageWrites {
    fn node(&mut self, page_id: u32, node: Node) {
        self.pages.insert(page_id, PageImage::Node(node));
    }

    fn free(&mut self, page_id: u32, next: u32) {
        self.pages.insert(page_id, PageImage::Free { next });
    }

    fn header(&mut self, page_id: u32, header: TreeHeader) {
        self.pages.insert(page_id, PageImage::Header(header));
    }

    /// Pins every page and encodes each image into a copy of its page (keeping the LSN); only
    /// when all of that worked are the copies put into the frames.
    fn apply(self, bpm: &mut BufferPoolManager) -> Result<(), IndexError> {
        let mut pinned = Vec::with_capacity(self.pages.len());
        let result = self
            .encode_pinned(bpm, &mut pinned)
            .and_then(|encoded| commit(bpm, encoded));
        for page_id in pinned {
            bpm.unpin_page(page_id, false);
        }
        result
    }

    fn encode_pinned(
        &self,
        bpm: &mut BufferPoolManager,
        pinned: &mut Vec<u32>,
    ) -> Result<Vec<(u32, Vec<u8>)>, IndexError> {
        let mut encoded = Vec::with_capacity(self.pages.len());
        for (&page_id, image) in &self.pages {
            let page = bpm.fetch_page(page_id)?;
            pinned.push(page_id);
            let mut bytes = page.data().to_vec();
            image.write(page_id, &mut bytes)?;
            encoded.push((page_id, bytes));
        }
        Ok(encoded)
    }
}

/// Every page is pinned by `encode_pinned`, so each fetch is a cache hit.
fn commit(bpm: &mut BufferPoolManager, encoded: Vec<(u32, Vec<u8>)>) -> Result<(), IndexError> {
    for (page_id, bytes) in encoded {
        let page = bpm.fetch_page(page_id)?;
        // Same length: the copy was taken from this frame.
        page.data_mut().copy_from_slice(&bytes);
        bpm.unpin_page(page_id, true);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::key::encode_key;
    use crate::index::node::{InternalEntry, LeafEntry, Node, TreeHeader, read_free_page};
    use crate::storage::page::{PAGE_TYPE_OFFSET, PageType, RESERVED_PAGE_ID};
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    const TEST_PAGE_SIZE: usize = 512;

    struct TestPool {
        path: PathBuf,
        page_size: usize,
        bpm: Option<BufferPoolManager>,
    }

    impl TestPool {
        fn new(pool_size: usize) -> Self {
            Self::with_page_size(pool_size, TEST_PAGE_SIZE)
        }

        fn with_page_size(pool_size: usize, page_size: usize) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("ferro-btree-{}-{id}.db", std::process::id()));
            let bpm = BufferPoolManager::new(pool_size, page_size, &path).unwrap();
            Self {
                path,
                page_size,
                bpm: Some(bpm),
            }
        }

        fn bpm(&mut self) -> &mut BufferPoolManager {
            self.bpm.as_mut().unwrap()
        }

        /// Drops the pool (flushing every page) and opens the file again.
        fn reopen(&mut self, pool_size: usize) {
            self.bpm.take();
            self.bpm = Some(BufferPoolManager::new(pool_size, self.page_size, &self.path).unwrap());
        }

        fn page_type(&mut self, page_id: u32) -> u8 {
            let page_type = self.bpm().fetch_page(page_id).unwrap().data()[PAGE_TYPE_OFFSET];
            self.bpm().unpin_page(page_id, false);
            page_type
        }

        fn new_page(&mut self) -> u32 {
            let (page_id, _) = self.bpm().new_page().unwrap();
            self.bpm().unpin_page(page_id, true);
            page_id
        }

        fn page_bytes(&mut self, page_id: u32) -> Vec<u8> {
            let bytes = self.bpm().fetch_page(page_id).unwrap().data().to_vec();
            self.bpm().unpin_page(page_id, false);
            bytes
        }

        /// Pins held by someone else: fetching adds one, which is taken out again.
        fn pins_of(&mut self, page_id: u32) -> u32 {
            let pins = self.bpm().fetch_page(page_id).unwrap().pin_count() - 1;
            self.bpm().unpin_page(page_id, false);
            pins
        }

        fn write_node(&mut self, page_id: u32, node: &Node) {
            let page = self.bpm().fetch_page(page_id).unwrap();
            node.write(page_id, page.data_mut()).unwrap();
            self.bpm().unpin_page(page_id, true);
        }
    }

    impl Drop for TestPool {
        fn drop(&mut self) {
            self.bpm.take();
            let _ = fs::remove_file(&self.path);
        }
    }

    fn key(value: i64) -> Vec<u8> {
        encode_key(DataType::BigInt, &Value::BigInt(value)).unwrap()
    }

    fn leaf(next: u32, keys: &[i64]) -> Node {
        Node::Leaf {
            next,
            entries: keys
                .iter()
                .map(|&value| LeafEntry {
                    key: key(value),
                    rid: Rid::new(100, value as u16),
                })
                .collect(),
        }
    }

    fn reason_of(err: IndexError) -> (u32, &'static str) {
        match err {
            IndexError::Corrupted { page_id, reason } => (page_id, reason),
            other => panic!("expected Corrupted, got {other:?}"),
        }
    }

    #[test]
    fn create_should_make_an_empty_tree() {
        let mut pool = TestPool::new(4);

        let tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();

        assert_eq!(tree.key_type(), DataType::BigInt);
        assert_eq!(tree.max_key_size(), 111);
        assert_eq!(
            pool.page_type(tree.header_page_id()),
            PageType::BPlusTreeHeader as u8
        );
        assert_eq!(
            pool.page_type(tree.root_page_id),
            PageType::BPlusTreeNode as u8
        );
        assert_eq!(tree.get(pool.bpm(), &Value::BigInt(1)).unwrap(), None);
    }

    #[test]
    fn open_should_find_the_tree_again_after_reopening() {
        let mut pool = TestPool::new(4);
        let created = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        pool.reopen(4);

        let opened =
            BPlusTree::open(pool.bpm(), DataType::BigInt, created.header_page_id()).unwrap();

        assert_eq!(opened.header_page_id(), created.header_page_id());
        assert_eq!(opened.root_page_id, created.root_page_id);
        assert_eq!(opened.get(pool.bpm(), &Value::BigInt(1)).unwrap(), None);
    }

    #[test]
    fn open_of_a_page_that_is_not_a_header_should_fail() {
        let mut pool = TestPool::new(4);
        let tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();

        let root = BPlusTree::open(pool.bpm(), DataType::BigInt, tree.root_page_id).unwrap_err();
        let reserved = BPlusTree::open(pool.bpm(), DataType::BigInt, RESERVED_PAGE_ID).unwrap_err();

        assert_eq!(
            reason_of(root),
            (tree.root_page_id, "not a B+Tree header page")
        );
        assert_eq!(
            reason_of(reserved),
            (RESERVED_PAGE_ID, "not a B+Tree header page")
        );
    }

    #[test]
    fn open_with_another_key_type_should_fail() {
        let mut pool = TestPool::new(4);
        let bigints = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let texts = BPlusTree::create(
            pool.bpm(),
            DataType::Varchar(std::num::NonZeroU16::new(10).unwrap()),
        )
        .unwrap();

        let err =
            BPlusTree::open(pool.bpm(), DataType::Integer, bigints.header_page_id()).unwrap_err();
        let longer_varchar =
            BPlusTree::open(pool.bpm(), DataType::VARCHAR_MAX, texts.header_page_id());

        assert!(
            matches!(
                err,
                IndexError::KeyTypeMismatch {
                    expected: DataType::BigInt,
                    found: DataType::Integer
                }
            ),
            "{err:?}"
        );
        assert!(
            longer_varchar.is_ok(),
            "the length of a VARCHAR is not part of the key type"
        );
    }

    #[test]
    fn open_of_a_header_with_an_unknown_key_type_should_fail() {
        let mut pool = TestPool::new(4);
        let tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let page = pool.bpm().fetch_page(tree.header_page_id()).unwrap();
        let header = TreeHeader::read(tree.header_page_id(), page.data()).unwrap();
        TreeHeader {
            key_type: 99,
            ..header
        }
        .write(tree.header_page_id(), page.data_mut())
        .unwrap();
        pool.bpm().unpin_page(tree.header_page_id(), true);

        let err = BPlusTree::open(pool.bpm(), DataType::BigInt, tree.header_page_id()).unwrap_err();

        assert_eq!(reason_of(err), (tree.header_page_id(), "unknown key type"));
    }

    #[test]
    fn pages_too_small_for_four_bigint_keys_should_be_unsupported() {
        let mut too_small = TestPool::with_page_size(4, 99);
        let mut smallest = TestPool::with_page_size(4, 100);

        let err = BPlusTree::create(too_small.bpm(), DataType::BigInt).unwrap_err();
        let tree = BPlusTree::create(smallest.bpm(), DataType::BigInt).unwrap();

        assert!(
            matches!(err, IndexError::UnsupportedPageSize { size: 99 }),
            "{err:?}"
        );
        assert_eq!(tree.max_key_size(), 8);
    }

    #[test]
    fn get_should_find_the_keys_of_a_root_leaf() {
        let mut pool = TestPool::new(4);
        let tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        pool.write_node(tree.root_page_id, &leaf(0, &[-5, 3, 9]));

        let found: Vec<Option<Rid>> = [-5, 3, 9, -6, 0, 10]
            .iter()
            .map(|&value| tree.get(pool.bpm(), &Value::BigInt(value)).unwrap())
            .collect();

        let rid = |value: i64| Some(Rid::new(100, value as u16));
        assert_eq!(found, [rid(-5), rid(3), rid(9), None, None, None]);
    }

    /// Root (level 1) with separator 5: keys below 5 in the left leaf, 5 and above on the right.
    fn two_level_tree(pool: &mut TestPool, root_level: u16) -> BPlusTree {
        let tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let (left, right) = (pool.new_page(), pool.new_page());
        pool.write_node(left, &leaf(right, &[1, 3]));
        pool.write_node(right, &leaf(0, &[5, 7]));
        let root = Node::Internal {
            level: root_level,
            first_child: left,
            entries: vec![InternalEntry {
                key: key(5),
                child: right,
            }],
        };
        pool.write_node(tree.root_page_id, &root);
        tree
    }

    #[test]
    fn get_should_descend_through_internal_nodes() {
        let mut pool = TestPool::new(4);
        let tree = two_level_tree(&mut pool, 1);

        let found: Vec<bool> = [1, 3, 5, 7, 0, 4, 6, 8]
            .iter()
            .map(|&value| {
                tree.get(pool.bpm(), &Value::BigInt(value))
                    .unwrap()
                    .is_some()
            })
            .collect();

        assert_eq!(found, [true, true, true, true, false, false, false, false]);
    }

    #[test]
    fn get_through_a_child_at_the_wrong_level_should_fail() {
        let mut pool = TestPool::new(4);
        let tree = two_level_tree(&mut pool, 2);

        let err = tree.get(pool.bpm(), &Value::BigInt(1)).unwrap_err();

        assert_eq!(reason_of(err).1, "child is not one level below its parent");
    }

    fn root_keys(pool: &mut TestPool, tree: &BPlusTree) -> Vec<Vec<u8>> {
        let bytes = pool.page_bytes(tree.root_page_id);
        match Node::read(tree.root_page_id, &bytes, tree.max_key_size).unwrap() {
            Node::Leaf { entries, .. } => entries.into_iter().map(|entry| entry.key).collect(),
            Node::Internal { entries, .. } => entries.into_iter().map(|entry| entry.key).collect(),
        }
    }

    #[test]
    fn insert_then_get_should_find_every_key_in_order() {
        let mut pool = TestPool::new(4);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();

        for value in [5, -2, 9, 0, i64::MIN] {
            tree.insert(pool.bpm(), &Value::BigInt(value), Rid::new(1, value as u16))
                .unwrap();
        }

        for value in [5, -2, 9, 0, i64::MIN] {
            let rid = tree.get(pool.bpm(), &Value::BigInt(value)).unwrap();
            assert_eq!(rid, Some(Rid::new(1, value as u16)));
        }
        let expected: Vec<Vec<u8>> = [i64::MIN, -2, 0, 5, 9]
            .iter()
            .map(|&value| key(value))
            .collect();
        assert_eq!(root_keys(&mut pool, &tree), expected);
    }

    #[test]
    fn insert_of_a_duplicate_key_should_fail_and_change_nothing() {
        let mut pool = TestPool::new(4);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        tree.insert(pool.bpm(), &Value::BigInt(7), Rid::new(1, 1))
            .unwrap();
        let before = pool.page_bytes(tree.root_page_id);

        let err = tree
            .insert(pool.bpm(), &Value::BigInt(7), Rid::new(2, 2))
            .unwrap_err();

        assert!(
            matches!(&err, IndexError::DuplicateKey { key } if key == "7"),
            "{err:?}"
        );
        assert_eq!(err.sqlstate(), "23505");
        assert_eq!(pool.page_bytes(tree.root_page_id), before);
        assert_eq!(
            tree.get(pool.bpm(), &Value::BigInt(7)).unwrap(),
            Some(Rid::new(1, 1))
        );
    }

    #[test]
    fn a_duplicate_long_key_should_be_shown_shortened() {
        let mut pool = TestPool::new(4);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::VARCHAR_MAX).unwrap();
        let long = Value::Text("x".repeat(100));
        tree.insert(pool.bpm(), &long, Rid::new(1, 0)).unwrap();

        let err = tree.insert(pool.bpm(), &long, Rid::new(1, 1)).unwrap_err();

        let shown = format!("{}…", "x".repeat(64));
        assert!(
            matches!(&err, IndexError::DuplicateKey { key } if *key == shown),
            "{err:?}"
        );
    }

    #[test]
    fn check_key_should_refuse_what_insert_refuses_without_changing_anything() {
        let mut pool = TestPool::new(4);
        let tree = BPlusTree::create(pool.bpm(), DataType::VARCHAR_MAX).unwrap();
        let before = pool.page_bytes(tree.root_page_id);

        let largest = tree.check_key(&Value::Text("x".repeat(111)));
        let too_large = tree.check_key(&Value::Text("x".repeat(112))).unwrap_err();
        let null = tree.check_key(&Value::Null).unwrap_err();
        let mismatch = tree.check_key(&Value::Integer(1)).unwrap_err();

        assert!(largest.is_ok());
        assert!(matches!(
            too_large,
            IndexError::KeyTooLarge {
                size: 112,
                max: 111
            }
        ));
        assert!(matches!(null, IndexError::NullKey));
        assert!(matches!(mismatch, IndexError::KeyTypeMismatch { .. }));
        assert_eq!(pool.page_bytes(tree.root_page_id), before);
    }

    #[test]
    fn insert_should_take_keys_up_to_the_maximum_size() {
        let mut pool = TestPool::new(4);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::VARCHAR_MAX).unwrap();
        let largest = Value::Text("é".repeat(55) + "x");
        let too_large = Value::Text("x".repeat(112));
        let before = pool.page_bytes(tree.root_page_id);

        let err = tree
            .insert(pool.bpm(), &too_large, Rid::new(1, 0))
            .unwrap_err();
        let after_error = pool.page_bytes(tree.root_page_id);
        tree.insert(pool.bpm(), &largest, Rid::new(1, 1)).unwrap();

        assert!(
            matches!(
                err,
                IndexError::KeyTooLarge {
                    size: 112,
                    max: 111
                }
            ),
            "{err:?}"
        );
        assert_eq!(err.sqlstate(), "54000");
        assert_eq!(after_error, before);
        assert_eq!(
            tree.get(pool.bpm(), &largest).unwrap(),
            Some(Rid::new(1, 1))
        );
    }

    #[test]
    fn insert_of_a_null_or_mistyped_key_should_fail() {
        let mut pool = TestPool::new(4);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::Integer).unwrap();

        let null = tree
            .insert(pool.bpm(), &Value::Null, Rid::new(1, 0))
            .unwrap_err();
        let mismatch = tree
            .insert(pool.bpm(), &Value::BigInt(1), Rid::new(1, 0))
            .unwrap_err();

        assert!(matches!(null, IndexError::NullKey));
        assert!(matches!(
            mismatch,
            IndexError::KeyTypeMismatch {
                expected: DataType::Integer,
                found: DataType::BigInt
            }
        ));
        assert!(root_keys(&mut pool, &tree).is_empty());
    }

    #[test]
    fn insert_should_leave_no_page_pinned() {
        let mut pool = TestPool::new(4);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();

        tree.insert(pool.bpm(), &Value::BigInt(1), Rid::new(1, 0))
            .unwrap();
        let _ = tree.insert(pool.bpm(), &Value::BigInt(1), Rid::new(1, 0));

        assert_eq!(pool.pins_of(tree.root_page_id), 0);
        assert_eq!(pool.pins_of(tree.header_page_id()), 0);
    }

    /// Walks the whole tree and checks every invariant; returns the leaf entries in order.
    fn check_invariants(pool: &mut TestPool, tree: &BPlusTree) -> Vec<(Vec<u8>, Rid)> {
        let mut walk = Walk::default();
        walk.visit(pool, tree, tree.root_page_id, None, None, None);
        for pair in walk.leaves.windows(2) {
            assert_eq!(
                pair[0].1, pair[1].0,
                "leaf {} links to the next leaf in order",
                pair[0].0
            );
        }
        if let Some(&(last, next)) = walk.leaves.last() {
            assert_eq!(next, 0, "the last leaf {last} has no next");
        }
        let header =
            TreeHeader::read(tree.header_page_id, &pool.page_bytes(tree.header_page_id)).unwrap();
        assert_eq!(
            header.root_page_id, tree.root_page_id,
            "the header points to the root"
        );
        let mut free = HashSet::new();
        for page_id in free_pages(pool, tree) {
            assert!(free.insert(page_id), "page {page_id} is free twice");
            assert!(
                !walk.pages.contains(&page_id),
                "page {page_id} is free and in the tree"
            );
        }
        walk.entries
    }

    #[derive(Default)]
    struct Walk {
        pages: HashSet<u32>,
        /// (page, next) of every leaf, left to right.
        leaves: Vec<(u32, u32)>,
        entries: Vec<(Vec<u8>, Rid)>,
    }

    impl Walk {
        /// Keys of the subtree must be in `lower..upper` (lower inclusive).
        fn visit(
            &mut self,
            pool: &mut TestPool,
            tree: &BPlusTree,
            page_id: u32,
            lower: Option<&[u8]>,
            upper: Option<&[u8]>,
            expected_level: Option<u16>,
        ) -> u16 {
            assert!(
                self.pages.insert(page_id),
                "page {page_id} is reached twice"
            );
            let node = Node::read(page_id, &pool.page_bytes(page_id), tree.max_key_size).unwrap();
            if let Some(level) = expected_level {
                assert_eq!(
                    node.level(),
                    level,
                    "page {page_id} is one level below its parent"
                );
            }
            let in_bounds = |key: &[u8]| {
                lower.is_none_or(|lower| key >= lower) && upper.is_none_or(|upper| key < upper)
            };
            match node {
                Node::Leaf { next, entries } => {
                    assert!(
                        !entries.is_empty() || page_id == tree.root_page_id,
                        "leaf {page_id} is empty and not the root"
                    );
                    for entry in entries {
                        assert!(
                            in_bounds(&entry.key),
                            "a key of leaf {page_id} is out of its bounds"
                        );
                        self.entries.push((entry.key, entry.rid));
                    }
                    self.leaves.push((page_id, next));
                    0
                }
                Node::Internal {
                    level,
                    first_child,
                    entries,
                } => {
                    for entry in &entries {
                        assert!(
                            in_bounds(&entry.key),
                            "a separator of page {page_id} is out of its bounds"
                        );
                    }
                    let mut child_lower = lower.map(<[u8]>::to_vec);
                    let mut child = first_child;
                    for entry in &entries {
                        self.visit(
                            pool,
                            tree,
                            child,
                            child_lower.as_deref(),
                            Some(&entry.key),
                            Some(level - 1),
                        );
                        child_lower = Some(entry.key.clone());
                        child = entry.child;
                    }
                    self.visit(
                        pool,
                        tree,
                        child,
                        child_lower.as_deref(),
                        upper,
                        Some(level - 1),
                    );
                    level
                }
            }
        }
    }

    fn root_level(pool: &mut TestPool, tree: &BPlusTree) -> u16 {
        let bytes = pool.page_bytes(tree.root_page_id);
        Node::read(tree.root_page_id, &bytes, tree.max_key_size)
            .unwrap()
            .level()
    }

    fn insert_all(pool: &mut TestPool, tree: &mut BPlusTree, values: &[i64]) {
        for &value in values {
            tree.insert(pool.bpm(), &Value::BigInt(value), rid_of(value))
                .unwrap();
        }
    }

    fn rid_of(value: i64) -> Rid {
        Rid::new(value as u32 / 7 + 1, value as u16)
    }

    fn sorted_entries(values: &[i64]) -> Vec<(Vec<u8>, Rid)> {
        let mut sorted = values.to_vec();
        sorted.sort();
        sorted
            .into_iter()
            .map(|value| (key(value), rid_of(value)))
            .collect()
    }

    #[test]
    fn a_full_root_leaf_should_split_into_a_new_root() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let old_root = tree.root_page_id;
        // A BIGINT leaf entry takes 18 bytes: 26 fit the 484 usable bytes of a 512-byte node.
        let values: Vec<i64> = (0..27).collect();

        insert_all(&mut pool, &mut tree, &values);

        assert_ne!(tree.root_page_id, old_root);
        assert_eq!(root_level(&mut pool, &tree), 1);
        assert_eq!(check_invariants(&mut pool, &tree), sorted_entries(&values));
        let reopened =
            BPlusTree::open(pool.bpm(), DataType::BigInt, tree.header_page_id()).unwrap();
        assert_eq!(reopened.root_page_id, tree.root_page_id);
    }

    #[test]
    fn many_inserts_should_split_internal_nodes_and_keep_every_key() {
        let orders: [Vec<i64>; 3] = [
            (0..2_000).collect(),
            (0..2_000).rev().collect(),
            (0..2_000).map(|i| (i * 7_919) % 2_000 - 1_000).collect(),
        ];
        for values in orders {
            let mut pool = TestPool::new(16);
            let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();

            for chunk in values.chunks(250) {
                insert_all(&mut pool, &mut tree, chunk);
                check_invariants(&mut pool, &tree);
            }

            assert!(
                root_level(&mut pool, &tree) >= 2,
                "internal nodes split too"
            );
            assert_eq!(check_invariants(&mut pool, &tree), sorted_entries(&values));
            for &value in &values {
                assert_eq!(
                    tree.get(pool.bpm(), &Value::BigInt(value)).unwrap(),
                    Some(rid_of(value))
                );
            }
            assert_eq!(pool.pins_of(tree.root_page_id), 0);
            assert_eq!(pool.pins_of(tree.header_page_id()), 0);
        }
    }

    #[test]
    fn text_keys_of_mixed_sizes_should_split_by_bytes() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::VARCHAR_MAX).unwrap();
        let max = tree.max_key_size();
        let keys: Vec<String> = (0..600)
            .map(|i: usize| {
                let len = [1, max, 2 + (i * 37) % (max - 1)][i % 3];
                let mut text = format!("{:05}", (i * 7_919) % 600);
                text.extend(std::iter::repeat_n('k', len.saturating_sub(5)));
                text.truncate(len.max(5).min(max));
                text
            })
            .collect();

        for (slot, text) in keys.iter().enumerate() {
            tree.insert(
                pool.bpm(),
                &Value::Text(text.clone()),
                Rid::new(1, slot as u16),
            )
            .unwrap();
        }

        let entries = check_invariants(&mut pool, &tree);
        assert_eq!(entries.len(), keys.len());
        assert!(root_level(&mut pool, &tree) >= 2);
        for (slot, text) in keys.iter().enumerate() {
            let rid = tree.get(pool.bpm(), &Value::Text(text.clone())).unwrap();
            assert_eq!(rid, Some(Rid::new(1, slot as u16)), "{text}");
        }
    }

    #[test]
    fn a_duplicate_in_a_deep_tree_should_change_nothing() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let values: Vec<i64> = (0..500).collect();
        insert_all(&mut pool, &mut tree, &values);

        let err = tree
            .insert(pool.bpm(), &Value::BigInt(250), Rid::new(9, 9))
            .unwrap_err();

        assert!(matches!(err, IndexError::DuplicateKey { .. }));
        assert_eq!(check_invariants(&mut pool, &tree), sorted_entries(&values));
    }

    fn tree_pages(pool: &mut TestPool, tree: &BPlusTree) -> Vec<u32> {
        let mut walk = Walk::default();
        walk.visit(pool, tree, tree.root_page_id, None, None, None);
        walk.pages.into_iter().collect()
    }

    fn collect_range(
        pool: &mut TestPool,
        tree: &BPlusTree,
        from: Bound<i64>,
        to: Bound<i64>,
    ) -> Vec<i64> {
        let (from, to) = (from.map(Value::BigInt), to.map(Value::BigInt));
        tree.range(pool.bpm(), from.as_ref(), to.as_ref())
            .unwrap()
            .map(|entry| {
                let (key, rid) = entry.unwrap();
                let Value::BigInt(value) = key else {
                    panic!("{key:?}")
                };
                assert_eq!(rid, rid_of(value));
                value
            })
            .collect()
    }

    #[test]
    fn range_should_yield_the_keys_within_the_bounds_in_order() {
        use Bound::{Excluded, Included, Unbounded};
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let values: Vec<i64> = (0..300).map(|i| (i * 7) % 300 * 3).collect();
        insert_all(&mut pool, &mut tree, &values);
        let model: std::collections::BTreeSet<i64> = values.iter().copied().collect();
        let bounds = [
            (Included(30), Included(60)),
            (Excluded(30), Excluded(60)),
            (Included(31), Excluded(61)),
            (Unbounded, Included(9)),
            (Included(850), Unbounded),
            (Excluded(-1), Excluded(0)),
            (Unbounded, Unbounded),
            (Included(-100), Included(10_000)),
        ];

        for (from, to) in bounds {
            let found = collect_range(&mut pool, &tree, from, to);

            let expected: Vec<i64> = model.range((from, to)).copied().collect();
            assert_eq!(found, expected, "{from:?}..{to:?}");
        }
        assert!(root_level(&mut pool, &tree) >= 1, "the ranges cross leaves");
    }

    #[test]
    fn empty_and_reversed_ranges_should_yield_nothing() {
        use Bound::{Excluded, Included, Unbounded};
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();

        let on_empty_tree = collect_range(&mut pool, &tree, Unbounded, Unbounded);
        insert_all(&mut pool, &mut tree, &(0..100).collect::<Vec<_>>());
        let reversed = collect_range(&mut pool, &tree, Included(60), Included(30));
        let empty = collect_range(&mut pool, &tree, Excluded(5), Excluded(6));
        let past_the_end = collect_range(&mut pool, &tree, Included(100), Unbounded);

        assert!(on_empty_tree.is_empty());
        assert!(reversed.is_empty());
        assert!(empty.is_empty());
        assert!(past_the_end.is_empty());
    }

    #[test]
    fn range_over_text_keys_should_decode_them() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::VARCHAR_MAX).unwrap();
        for (slot, text) in ["pear", "apple", "fig", "", "apple pie"].iter().enumerate() {
            tree.insert(
                pool.bpm(),
                &Value::Text(text.to_string()),
                Rid::new(1, slot as u16),
            )
            .unwrap();
        }
        let from = Value::Text("apple".to_string());
        let to = Value::Text("g".to_string());

        let found: Vec<Value> = tree
            .range(pool.bpm(), Bound::Included(&from), Bound::Excluded(&to))
            .unwrap()
            .map(|entry| entry.unwrap().0)
            .collect();

        let text = |value: &str| Value::Text(value.to_string());
        assert_eq!(found, [text("apple"), text("apple pie"), text("fig")]);
    }

    #[test]
    fn dropping_a_range_midway_should_leave_no_page_pinned() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        insert_all(&mut pool, &mut tree, &(0..500).collect::<Vec<_>>());

        let taken = tree
            .range(pool.bpm(), Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .take(40)
            .count();

        assert_eq!(taken, 40);
        for page_id in tree_pages(&mut pool, &tree) {
            assert_eq!(pool.pins_of(page_id), 0, "page {page_id}");
        }
    }

    #[test]
    fn range_with_a_null_or_mistyped_bound_should_fail() {
        let mut pool = TestPool::new(4);
        let tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();

        let null = tree
            .range(pool.bpm(), Bound::Included(&Value::Null), Bound::Unbounded)
            .err()
            .unwrap();
        let mismatch = tree
            .range(
                pool.bpm(),
                Bound::Unbounded,
                Bound::Excluded(&Value::Integer(3)),
            )
            .err()
            .unwrap();

        assert!(matches!(null, IndexError::NullKey));
        assert!(matches!(mismatch, IndexError::KeyTypeMismatch { .. }));
    }

    /// The leaves of a two-level tree with keys 0..100, left to right.
    fn leaves_of(pool: &mut TestPool, tree: &BPlusTree) -> Vec<u32> {
        let mut walk = Walk::default();
        walk.visit(pool, tree, tree.root_page_id, None, None, None);
        walk.leaves.iter().map(|&(page_id, _)| page_id).collect()
    }

    #[test]
    fn a_damaged_leaf_should_end_the_range_with_an_error() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        insert_all(&mut pool, &mut tree, &(0..100).collect::<Vec<_>>());
        let leaves = leaves_of(&mut pool, &tree);
        let damaged = leaves[1];
        let page = pool.bpm().fetch_page(damaged).unwrap();
        TreeHeader {
            root_page_id: 3,
            free_list_head: 0,
            key_type: 3,
        }
        .write(damaged, page.data_mut())
        .unwrap();
        pool.bpm().unpin_page(damaged, true);

        let results: Vec<_> = tree
            .range(pool.bpm(), Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .collect();

        let last = results.last().unwrap().as_ref().unwrap_err();
        assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
        assert!(matches!(last, IndexError::Corrupted { page_id, .. } if *page_id == damaged));
    }

    #[test]
    fn leaves_linked_out_of_key_order_should_be_detected() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        insert_all(&mut pool, &mut tree, &(0..100).collect::<Vec<_>>());
        let leaves = leaves_of(&mut pool, &tree);
        // The second leaf links back to the first: without the check, a scan would loop forever.
        let bytes = pool.page_bytes(leaves[1]);
        let Node::Leaf { entries, .. } = Node::read(leaves[1], &bytes, tree.max_key_size).unwrap()
        else {
            panic!("not a leaf")
        };
        pool.write_node(
            leaves[1],
            &Node::Leaf {
                next: leaves[0],
                entries,
            },
        );

        let results: Vec<_> = tree
            .range(pool.bpm(), Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .collect();

        let err = results.last().unwrap().as_ref().unwrap_err();
        assert!(
            matches!(err, IndexError::Corrupted { reason, .. } if *reason == "leaves are not linked in key order"),
            "{err:?}"
        );
    }

    fn remove_all(pool: &mut TestPool, tree: &mut BPlusTree, values: &[i64]) {
        for &value in values {
            let removed = tree.remove(pool.bpm(), &Value::BigInt(value)).unwrap();
            assert_eq!(removed, Some(rid_of(value)), "{value}");
        }
    }

    #[test]
    fn remove_should_return_the_rid_and_forget_the_key() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        insert_all(&mut pool, &mut tree, &(0..100).collect::<Vec<_>>());

        remove_all(&mut pool, &mut tree, &[40, 0, 99]);

        for value in [40, 0, 99] {
            assert_eq!(tree.get(pool.bpm(), &Value::BigInt(value)).unwrap(), None);
        }
        let left: Vec<i64> = (0..100)
            .filter(|value| ![40, 0, 99].contains(value))
            .collect();
        assert_eq!(check_invariants(&mut pool, &tree), sorted_entries(&left));
    }

    #[test]
    fn remove_of_a_missing_key_should_change_nothing() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::VARCHAR_MAX).unwrap();
        tree.insert(pool.bpm(), &Value::Text("b".to_string()), Rid::new(1, 1))
            .unwrap();
        let before = pool.page_bytes(tree.root_page_id);

        let missing = tree
            .remove(pool.bpm(), &Value::Text("a".to_string()))
            .unwrap();
        let too_large = tree
            .remove(pool.bpm(), &Value::Text("x".repeat(500)))
            .unwrap();

        assert_eq!(missing, None);
        assert_eq!(too_large, None);
        assert_eq!(pool.page_bytes(tree.root_page_id), before);
    }

    #[test]
    fn the_root_leaf_should_be_allowed_to_become_empty() {
        let mut pool = TestPool::new(4);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        insert_all(&mut pool, &mut tree, &[1, 2]);

        remove_all(&mut pool, &mut tree, &[2, 1]);

        assert!(check_invariants(&mut pool, &tree).is_empty());
        insert_all(&mut pool, &mut tree, &[3]);
        assert_eq!(check_invariants(&mut pool, &tree), sorted_entries(&[3]));
    }

    #[test]
    fn remove_of_a_null_or_mistyped_key_should_fail() {
        let mut pool = TestPool::new(4);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();

        let null = tree.remove(pool.bpm(), &Value::Null).unwrap_err();
        let mismatch = tree.remove(pool.bpm(), &Value::SmallInt(1)).unwrap_err();

        assert!(matches!(null, IndexError::NullKey));
        assert!(matches!(mismatch, IndexError::KeyTypeMismatch { .. }));
    }

    fn set_root(pool: &mut TestPool, tree: &mut BPlusTree, root_page_id: u32) {
        let page = pool.bpm().fetch_page(tree.header_page_id).unwrap();
        let header = TreeHeader::read(tree.header_page_id, page.data()).unwrap();
        TreeHeader {
            root_page_id,
            ..header
        }
        .write(tree.header_page_id, page.data_mut())
        .unwrap();
        pool.bpm().unpin_page(tree.header_page_id, true);
        tree.root_page_id = root_page_id;
    }

    /// Writes an internal node over `children` (page, first key) and returns its page.
    fn craft_internal(pool: &mut TestPool, level: u16, children: &[(u32, i64)]) -> u32 {
        let page_id = pool.new_page();
        let node = Node::Internal {
            level,
            first_child: children[0].0,
            entries: children[1..]
                .iter()
                .map(|&(child, first)| InternalEntry {
                    key: key(first),
                    child,
                })
                .collect(),
        };
        pool.write_node(page_id, &node);
        page_id
    }

    /// A tree whose leaves hold exactly `groups[g][l]` keys (0, 10, 20, ... in order). One
    /// group makes a root over the leaves; several make a root over one internal node each.
    /// Returns the keys and the leaf pages.
    fn craft_tree(
        pool: &mut TestPool,
        tree: &mut BPlusTree,
        groups: &[Vec<usize>],
    ) -> (Vec<i64>, Vec<u32>) {
        let counts: Vec<usize> = groups.iter().flatten().copied().collect();
        let leaves: Vec<u32> = counts.iter().map(|_| pool.new_page()).collect();
        let mut values = Vec::new();
        let mut firsts = Vec::new();
        for (index, &count) in counts.iter().enumerate() {
            let keys: Vec<i64> = (0..count).map(|i| (values.len() + i) as i64 * 10).collect();
            firsts.push(keys[0]);
            let next = leaves.get(index + 1).copied().unwrap_or(0);
            let node = Node::Leaf {
                next,
                entries: keys
                    .iter()
                    .map(|&value| LeafEntry {
                        key: key(value),
                        rid: rid_of(value),
                    })
                    .collect(),
            };
            pool.write_node(leaves[index], &node);
            values.extend(keys);
        }
        let children: Vec<(u32, i64)> = leaves.iter().copied().zip(firsts).collect();
        let root = if groups.len() == 1 {
            craft_internal(pool, 1, &children)
        } else {
            let mut start = 0;
            let internals: Vec<(u32, i64)> = groups
                .iter()
                .map(|group| {
                    let own = &children[start..start + group.len()];
                    start += group.len();
                    (craft_internal(pool, 1, own), own[0].1)
                })
                .collect();
            craft_internal(pool, 2, &internals)
        };
        set_root(pool, tree, root);
        assert_eq!(
            check_invariants(pool, tree),
            sorted_entries(&values),
            "a valid crafted tree"
        );
        (values, leaves)
    }

    /// Entries per leaf, left to right, and children per internal node of level 1.
    fn shape(pool: &mut TestPool, tree: &BPlusTree) -> (Vec<usize>, Vec<usize>) {
        let mut leaf_sizes = Vec::new();
        let mut fanouts = Vec::new();
        let mut walk = Walk::default();
        walk.visit(pool, tree, tree.root_page_id, None, None, None);
        let mut pages: Vec<u32> = walk.pages.into_iter().collect();
        pages.sort();
        for (page_id, _) in &walk.leaves {
            let bytes = pool.page_bytes(*page_id);
            if let Node::Leaf { entries, .. } =
                Node::read(*page_id, &bytes, tree.max_key_size).unwrap()
            {
                leaf_sizes.push(entries.len());
            }
        }
        for page_id in pages {
            let bytes = pool.page_bytes(page_id);
            if let Node::Internal {
                level: 1, entries, ..
            } = Node::read(page_id, &bytes, tree.max_key_size).unwrap()
            {
                fanouts.push(entries.len() + 1);
            }
        }
        (leaf_sizes, fanouts)
    }

    fn free_pages(pool: &mut TestPool, tree: &BPlusTree) -> Vec<u32> {
        let header =
            TreeHeader::read(tree.header_page_id, &pool.page_bytes(tree.header_page_id)).unwrap();
        let mut pages = Vec::new();
        let mut page_id = header.free_list_head;
        while page_id != 0 {
            pages.push(page_id);
            page_id = read_free_page(page_id, &pool.page_bytes(page_id)).unwrap();
        }
        pages
    }

    fn without(values: &[i64], removed: &[i64]) -> Vec<i64> {
        values
            .iter()
            .copied()
            .filter(|value| !removed.contains(value))
            .collect()
    }

    #[test]
    fn alternating_inserts_and_removes_at_a_split_should_not_rebuild_the_tree() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        // 27 keys split the root leaf into 13 and 14 entries.
        insert_all(&mut pool, &mut tree, &(0..27).collect::<Vec<_>>());
        let root = tree.root_page_id;

        for _ in 0..4 {
            remove_all(&mut pool, &mut tree, &[0]);
            assert_eq!(tree.root_page_id, root, "a removal merged the halves back");
            insert_all(&mut pool, &mut tree, &[0]);
            assert_eq!(tree.root_page_id, root, "an insert split them again");
        }

        assert_eq!(shape(&mut pool, &tree).0, [13, 14]);
        assert!(free_pages(&mut pool, &tree).is_empty());
    }

    #[test]
    fn an_underfull_leaf_should_borrow_from_its_right_sibling() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let (values, _) = craft_tree(&mut pool, &mut tree, &[vec![7, 26]]);

        // 6 entries left (108 bytes) is under a quarter of a node; 6 + 26 do not fit one leaf.
        remove_all(&mut pool, &mut tree, &[50]);

        assert_eq!(shape(&mut pool, &tree).0, [16, 16]);
        assert_eq!(
            check_invariants(&mut pool, &tree),
            sorted_entries(&without(&values, &[50]))
        );
    }

    #[test]
    fn an_underfull_leaf_should_borrow_from_its_left_sibling() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let (values, _) = craft_tree(&mut pool, &mut tree, &[vec![26, 7]]);

        remove_all(&mut pool, &mut tree, &[300]);

        assert_eq!(shape(&mut pool, &tree).0, [16, 16]);
        assert_eq!(
            check_invariants(&mut pool, &tree),
            sorted_entries(&without(&values, &[300]))
        );
    }

    #[test]
    fn an_underfull_internal_node_should_borrow_from_its_sibling() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        // Removing from the first leaf merges it with the second; its parent drops to 7
        // separators (112 bytes, under a quarter), and 7 + 1 + 30 do not fit one node.
        let (values, _) = craft_tree(&mut pool, &mut tree, &[vec![7; 9], vec![7; 31]]);

        remove_all(&mut pool, &mut tree, &[0]);

        assert_eq!(shape(&mut pool, &tree).1, [20, 19]);
        assert_eq!(root_level(&mut pool, &tree), 2);
        assert_eq!(
            check_invariants(&mut pool, &tree),
            sorted_entries(&without(&values, &[0]))
        );
    }

    #[test]
    fn an_underfull_leaf_should_merge_with_its_sibling() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let (values, leaves) = craft_tree(&mut pool, &mut tree, &[vec![7, 7, 7]]);

        remove_all(&mut pool, &mut tree, &[0]);

        assert_eq!(shape(&mut pool, &tree).0, [13, 7]);
        assert_eq!(free_pages(&mut pool, &tree), [leaves[1]]);
        assert_eq!(pool.page_type(leaves[1]), PageType::BPlusTreeFree as u8);
        assert_eq!(
            check_invariants(&mut pool, &tree),
            sorted_entries(&without(&values, &[0]))
        );
    }

    #[test]
    fn merging_the_last_two_children_of_the_root_should_shrink_the_tree() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        // 6 + 1 + 7 separators fit one node: the two internal nodes merge and the root is
        // left with one child, which becomes the root.
        let (values, _) = craft_tree(&mut pool, &mut tree, &[vec![7; 8], vec![7; 8]]);
        let old_root = tree.root_page_id;

        remove_all(&mut pool, &mut tree, &[0]);

        assert_ne!(tree.root_page_id, old_root);
        assert_eq!(root_level(&mut pool, &tree), 1);
        assert_eq!(shape(&mut pool, &tree).1, [15]);
        assert_eq!(
            free_pages(&mut pool, &tree).len(),
            3,
            "a leaf, an internal node and the old root"
        );
        assert!(free_pages(&mut pool, &tree).contains(&old_root));
        assert_eq!(
            check_invariants(&mut pool, &tree),
            sorted_entries(&without(&values, &[0]))
        );
        let reopened =
            BPlusTree::open(pool.bpm(), DataType::BigInt, tree.header_page_id()).unwrap();
        assert_eq!(reopened.root_page_id, tree.root_page_id);
    }

    #[test]
    fn removing_every_key_should_shrink_the_tree_back_to_a_root_leaf() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let values: Vec<i64> = (0..2_000).collect();
        insert_all(&mut pool, &mut tree, &values);
        let order: Vec<i64> = (0..2_000).map(|i| (i * 7_919) % 2_000).collect();

        for (step, chunk) in order.chunks(100).enumerate() {
            remove_all(&mut pool, &mut tree, chunk);
            let left: Vec<i64> = order[(step + 1) * 100..].to_vec();
            assert_eq!(check_invariants(&mut pool, &tree), sorted_entries(&left));
        }

        assert_eq!(root_level(&mut pool, &tree), 0);
        let reachable = tree_pages(&mut pool, &tree).len();
        let free = free_pages(&mut pool, &tree);
        let unique: HashSet<u32> = free.iter().copied().collect();
        assert_eq!(unique.len(), free.len(), "no page is freed twice");
        assert_eq!(reachable, 1);
        assert!(!unique.contains(&tree.root_page_id));
    }

    fn long_text(prefix: &str) -> String {
        format!("{prefix}{}", "x".repeat(99))
    }

    #[test]
    fn a_redistribution_whose_separator_does_not_fit_the_parent_should_be_skipped() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::VARCHAR_MAX).unwrap();
        let text_key =
            |text: &str| encode_key(DataType::VARCHAR_MAX, &Value::Text(text.to_string())).unwrap();
        let text_leaf = |next: u32, keys: &[String]| Node::Leaf {
            next,
            entries: keys
                .iter()
                .enumerate()
                .map(|(slot, text)| LeafEntry {
                    key: text_key(text),
                    rid: Rid::new(1, slot as u16),
                })
                .collect(),
        };
        let leaves: Vec<u32> = (0..6).map(|_| pool.new_page()).collect();
        // The first leaf will underflow; the second is full enough that merging does not fit,
        // and its keys are long, so a redistribution would need a 100-byte separator.
        let first = vec!["a".to_string(), "aa".to_string()];
        let second = vec![
            "b".to_string(),
            "b0".to_string(),
            "b1".to_string(),
            long_text("ba"),
            long_text("bb"),
            long_text("bc"),
            long_text("bd"),
        ];
        let separators: Vec<String> = vec![
            "b".to_string(),
            long_text("c"),
            long_text("d"),
            long_text("e"),
            long_text("f"),
        ];
        pool.write_node(leaves[0], &text_leaf(leaves[1], &first));
        pool.write_node(leaves[1], &text_leaf(leaves[2], &second));
        for index in 2..6 {
            let next = leaves.get(index + 1).copied().unwrap_or(0);
            pool.write_node(
                leaves[index],
                &text_leaf(next, &[separators[index - 1].clone()]),
            );
        }
        let root = pool.new_page();
        // Separators take 9 + 4 × 108 = 441 of 484 bytes: a 99-byte longer one does not fit.
        let root_node = Node::Internal {
            level: 1,
            first_child: leaves[0],
            entries: (1..6)
                .map(|index| InternalEntry {
                    key: text_key(&separators[index - 1]),
                    child: leaves[index],
                })
                .collect(),
        };
        pool.write_node(root, &root_node);
        set_root(&mut pool, &mut tree, root);
        let root_before = pool.page_bytes(root);
        let sibling_before = pool.page_bytes(leaves[1]);

        let removed = tree
            .remove(pool.bpm(), &Value::Text("aa".to_string()))
            .unwrap();

        assert_eq!(removed, Some(Rid::new(1, 1)));
        assert_eq!(pool.page_bytes(root), root_before);
        assert_eq!(pool.page_bytes(leaves[1]), sibling_before);
        assert_eq!(check_invariants(&mut pool, &tree).len(), 1 + 7 + 4);
    }

    #[test]
    fn a_page_freed_by_a_merge_should_be_reused_by_the_next_split() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let (values, leaves) = craft_tree(&mut pool, &mut tree, &[vec![7, 7, 7]]);
        remove_all(&mut pool, &mut tree, &[0]);
        let before = pool.new_page();
        let more: Vec<i64> = (1..10).chain(11..16).collect();

        // The merged leaf holds 13 entries: 14 more make it split.
        insert_all(&mut pool, &mut tree, &more);

        assert_eq!(
            pool.new_page(),
            before + 1,
            "the tree took no page from the buffer pool"
        );
        assert!(free_pages(&mut pool, &tree).is_empty());
        assert!(tree_pages(&mut pool, &tree).contains(&leaves[1]));
        let mut expected = without(&values, &[0]);
        expected.extend(more);
        assert_eq!(
            check_invariants(&mut pool, &tree),
            sorted_entries(&expected)
        );
    }

    #[test]
    fn filling_and_emptying_the_tree_again_should_not_take_new_pages() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let values: Vec<i64> = (0..1_000).map(|i| (i * 7_919) % 1_000).collect();
        insert_all(&mut pool, &mut tree, &values);
        remove_all(&mut pool, &mut tree, &values);
        let mut sentinel = pool.new_page();

        for _ in 0..3 {
            insert_all(&mut pool, &mut tree, &values);
            remove_all(&mut pool, &mut tree, &values);

            let next = pool.new_page();
            assert_eq!(next, sentinel + 1, "every page came from the free list");
            sentinel = next;
        }
        assert!(check_invariants(&mut pool, &tree).is_empty());
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

    type Model = std::collections::BTreeMap<Vec<u8>, (Value, Rid)>;

    fn random_bound(
        rng: &mut Rng,
        model: &Model,
        random_key: &mut dyn FnMut(&mut Rng) -> Value,
    ) -> Bound<Value> {
        let key = match (rng.below(2), model.is_empty()) {
            (0, false) => model
                .values()
                .nth(rng.below(model.len()))
                .unwrap()
                .0
                .clone(),
            _ => random_key(rng),
        };
        match rng.below(3) {
            0 => Bound::Included(key),
            1 => Bound::Excluded(key),
            _ => Bound::Unbounded,
        }
    }

    /// `BTreeMap::range` panics on these instead of returning nothing.
    fn is_empty_range(low: &Bound<Vec<u8>>, high: &Bound<Vec<u8>>) -> bool {
        match (low, high) {
            (Bound::Included(low_key), Bound::Included(high_key)) => low_key > high_key,
            (
                Bound::Included(low_key) | Bound::Excluded(low_key),
                Bound::Included(high_key) | Bound::Excluded(high_key),
            ) => low_key >= high_key,
            _ => false,
        }
    }

    /// Random inserts (some duplicates), removes, gets and ranges against a `BTreeMap` of the
    /// encoded keys, with every invariant checked every 50 steps.
    fn run_against_model(
        key_type: DataType,
        seed: u64,
        mut random_key: impl FnMut(&mut Rng) -> Value,
    ) {
        let mut rng = Rng(seed);
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), key_type).unwrap();
        let mut model = Model::new();
        let encode = |key: &Value| encode_key(key_type, key).unwrap();

        for step in 0..3_000_u32 {
            let existing = (!model.is_empty() && rng.below(10) < 7).then(|| {
                model
                    .values()
                    .nth(rng.below(model.len()))
                    .unwrap()
                    .0
                    .clone()
            });
            match rng.below(10) {
                0..=4 => {
                    let key = random_key(&mut rng);
                    let rid = Rid::new(step + 1, (step % 100) as u16);
                    let result = tree.insert(pool.bpm(), &key, rid);
                    match model.entry(encode(&key)) {
                        std::collections::btree_map::Entry::Occupied(_) => assert!(
                            matches!(result, Err(IndexError::DuplicateKey { .. })),
                            "step {step}: {result:?}"
                        ),
                        std::collections::btree_map::Entry::Vacant(slot) => {
                            result.unwrap();
                            slot.insert((key, rid));
                        }
                    }
                }
                5..=7 => {
                    let key = existing.unwrap_or_else(|| random_key(&mut rng));
                    let removed = tree.remove(pool.bpm(), &key).unwrap();
                    let expected = model.remove(&encode(&key)).map(|(_, rid)| rid);
                    assert_eq!(removed, expected, "step {step}");
                }
                8 => {
                    let key = existing.unwrap_or_else(|| random_key(&mut rng));
                    let found = tree.get(pool.bpm(), &key).unwrap();
                    let expected = model.get(&encode(&key)).map(|(_, rid)| *rid);
                    assert_eq!(found, expected, "step {step}");
                }
                _ => {
                    let from = random_bound(&mut rng, &model, &mut random_key);
                    let to = random_bound(&mut rng, &model, &mut random_key);
                    let found: Vec<(Value, Rid)> = tree
                        .range(pool.bpm(), from.as_ref(), to.as_ref())
                        .unwrap()
                        .map(|entry| entry.unwrap())
                        .collect();
                    let (low, high) = (from.as_ref().map(encode), to.as_ref().map(encode));
                    let expected: Vec<(Value, Rid)> = if is_empty_range(&low, &high) {
                        Vec::new()
                    } else {
                        model
                            .range((low, high))
                            .map(|(_, entry)| entry.clone())
                            .collect()
                    };
                    assert_eq!(found, expected, "step {step}: {from:?}..{to:?}");
                }
            }
            if step % 50 == 0 {
                let expected: Vec<(Vec<u8>, Rid)> = model
                    .iter()
                    .map(|(encoded, (_, rid))| (encoded.clone(), *rid))
                    .collect();
                assert_eq!(check_invariants(&mut pool, &tree), expected, "step {step}");
            }
        }
        for page_id in tree_pages(&mut pool, &tree) {
            assert_eq!(pool.pins_of(page_id), 0, "page {page_id}");
        }
    }

    #[test]
    fn random_operations_with_bigint_keys_should_match_model() {
        run_against_model(DataType::BigInt, 0x9E37_79B9_7F4A_7C15, |rng| {
            Value::BigInt(rng.below(1_500) as i64 - 750)
        });
    }

    #[test]
    fn random_operations_with_varchar_keys_should_match_model() {
        // Lengths up to the 111-byte maximum, so splits and merges happen by bytes.
        run_against_model(DataType::VARCHAR_MAX, 0x2545_F491_4F6C_DD1D, |rng| {
            let len = [0, 1, 3, 20, 60, 108][rng.below(6)];
            Value::Text(format!("{:03}{}", rng.below(400), "v".repeat(len)))
        });
    }

    fn assert_no_page_pinned(pool: &mut TestPool, tree: &BPlusTree) {
        for page_id in tree_pages(pool, tree) {
            assert_eq!(pool.pins_of(page_id), 0, "page {page_id}");
        }
        assert_eq!(pool.pins_of(tree.header_page_id), 0);
    }

    fn is_no_frame(err: &IndexError) -> bool {
        matches!(
            err,
            IndexError::BufferPool(crate::storage::buffer_pool::BufferPoolError::NoAvailableFrame)
        )
    }

    #[test]
    fn a_split_without_enough_frames_should_fail_before_writing() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let values: Vec<i64> = (0..26).collect();
        insert_all(&mut pool, &mut tree, &values);
        // The split writes the leaf, its new sibling, a new root and the header: 4 pages.
        pool.reopen(3);
        let mut tree =
            BPlusTree::open(pool.bpm(), DataType::BigInt, tree.header_page_id()).unwrap();

        let err = tree
            .insert(pool.bpm(), &Value::BigInt(26), rid_of(26))
            .unwrap_err();

        assert!(is_no_frame(&err), "{err:?}");
        assert_eq!(err.sqlstate(), "53000");
        assert_no_page_pinned(&mut pool, &tree);
        pool.reopen(16);
        let tree = BPlusTree::open(pool.bpm(), DataType::BigInt, tree.header_page_id()).unwrap();
        assert_eq!(check_invariants(&mut pool, &tree), sorted_entries(&values));
    }

    #[test]
    fn a_merge_without_enough_frames_should_fail_before_writing() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let (values, _) = craft_tree(&mut pool, &mut tree, &[vec![7, 7, 7]]);
        // The merge writes the merged leaf, the freed page, the parent and the header.
        pool.reopen(3);
        let mut tree =
            BPlusTree::open(pool.bpm(), DataType::BigInt, tree.header_page_id()).unwrap();

        let err = tree.remove(pool.bpm(), &Value::BigInt(0)).unwrap_err();

        assert!(is_no_frame(&err), "{err:?}");
        assert_no_page_pinned(&mut pool, &tree);
        pool.reopen(16);
        let tree = BPlusTree::open(pool.bpm(), DataType::BigInt, tree.header_page_id()).unwrap();
        assert_eq!(check_invariants(&mut pool, &tree), sorted_entries(&values));
    }

    #[test]
    fn a_fetch_failing_at_any_point_should_change_nothing() {
        // (page to fail, fetches of it that succeed first): the descent reads a page once and
        // writing it fetches it again; a merge also reads the sibling.
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let (values, leaves) = craft_tree(&mut pool, &mut tree, &[vec![7, 7, 7]]);
        let root = tree.root_page_id;
        let header = tree.header_page_id();
        let failures = [
            (root, 0),
            (leaves[0], 0),
            (leaves[0], 1),
            (leaves[1], 0),
            (header, 0),
        ];

        for (page_id, successes) in failures {
            pool.bpm().fail_fetch_after(page_id, successes);

            let err = tree.remove(pool.bpm(), &Value::BigInt(0)).unwrap_err();

            assert!(is_no_frame(&err), "{page_id} after {successes}: {err:?}");
            assert_eq!(check_invariants(&mut pool, &tree), sorted_entries(&values));
            assert_no_page_pinned(&mut pool, &tree);
        }
        for (page_id, successes) in [(root, 0), (leaves[2], 1)] {
            pool.bpm().fail_fetch_after(page_id, successes);

            let err = tree
                .insert(pool.bpm(), &Value::BigInt(205), rid_of(205))
                .unwrap_err();

            assert!(is_no_frame(&err), "{page_id} after {successes}: {err:?}");
            assert_eq!(check_invariants(&mut pool, &tree), sorted_entries(&values));
        }
    }

    #[test]
    fn a_damaged_node_should_be_reported_as_corrupted() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let (_, leaves) = craft_tree(&mut pool, &mut tree, &[vec![7, 7, 7]]);
        let page = pool.bpm().fetch_page(leaves[1]).unwrap();
        crate::index::node::write_free_page(leaves[1], page.data_mut(), 0).unwrap();
        pool.bpm().unpin_page(leaves[1], true);

        let get = tree.get(pool.bpm(), &Value::BigInt(100)).unwrap_err();
        let insert = tree
            .insert(pool.bpm(), &Value::BigInt(105), rid_of(105))
            .unwrap_err();
        let remove = tree.remove(pool.bpm(), &Value::BigInt(0)).unwrap_err();

        for err in [get, insert, remove] {
            assert_eq!(reason_of_ref(&err), (leaves[1], "not a B+Tree node"));
            assert_eq!(err.sqlstate(), "XX001");
        }
    }

    fn reason_of_ref(err: &IndexError) -> (u32, &'static str) {
        match err {
            IndexError::Corrupted { page_id, reason } => (*page_id, reason),
            other => panic!("expected Corrupted, got {other:?}"),
        }
    }

    #[test]
    fn a_root_at_the_highest_level_should_not_grow_another_level() {
        assert_eq!(level_above(7, 0).unwrap(), 1);
        assert_eq!(level_above(7, u16::MAX - 1).unwrap(), u16::MAX);

        let err = level_above(7, u16::MAX).unwrap_err();

        assert_eq!(
            reason_of(err),
            (7, "the tree has no level left above its root")
        );
    }

    #[test]
    fn open_on_pages_too_small_should_be_unsupported() {
        let mut pool = TestPool::with_page_size(4, 99);

        let err = BPlusTree::open(pool.bpm(), DataType::BigInt, 1).unwrap_err();

        assert!(
            matches!(err, IndexError::UnsupportedPageSize { size: 99 }),
            "{err:?}"
        );
    }

    #[test]
    fn a_free_list_pointing_to_a_live_page_should_be_detected() {
        let mut pool = TestPool::new(16);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let values: Vec<i64> = (0..26).collect();
        insert_all(&mut pool, &mut tree, &values);
        // A damaged header whose free list starts at the root: a split must not take it.
        let page = pool.bpm().fetch_page(tree.header_page_id).unwrap();
        let header = TreeHeader::read(tree.header_page_id, page.data()).unwrap();
        TreeHeader {
            free_list_head: tree.root_page_id,
            ..header
        }
        .write(tree.header_page_id, page.data_mut())
        .unwrap();
        pool.bpm().unpin_page(tree.header_page_id, true);
        let root_before = pool.page_bytes(tree.root_page_id);

        let err = tree
            .insert(pool.bpm(), &Value::BigInt(26), rid_of(26))
            .unwrap_err();

        assert_eq!(
            reason_of(err),
            (tree.root_page_id, "not a free B+Tree page")
        );
        assert_eq!(pool.page_bytes(tree.root_page_id), root_before);
        assert_eq!(
            collect_range(&mut pool, &tree, Bound::Unbounded, Bound::Unbounded),
            values
        );
    }

    #[test]
    fn range_bounds_longer_than_the_maximum_key_should_still_bound_the_range() {
        let mut pool = TestPool::new(4);
        let mut tree = BPlusTree::create(pool.bpm(), DataType::VARCHAR_MAX).unwrap();
        let text = |value: &str| Value::Text(value.to_string());
        for (slot, key) in ["a", "b", "c"].iter().enumerate() {
            tree.insert(pool.bpm(), &text(key), Rid::new(1, slot as u16))
                .unwrap();
        }
        let after_a = text(&format!("a{}", "z".repeat(200)));
        let after_b = text(&format!("b{}", "z".repeat(200)));

        let found: Vec<Value> = tree
            .range(
                pool.bpm(),
                Bound::Included(&after_a),
                Bound::Excluded(&after_b),
            )
            .unwrap()
            .map(|entry| entry.unwrap().0)
            .collect();

        assert!(after_a.to_string().len() > tree.max_key_size());
        assert_eq!(found, [text("b")]);
    }

    #[test]
    fn get_should_check_the_key_before_reading_pages() {
        let mut pool = TestPool::new(4);
        let tree = BPlusTree::create(pool.bpm(), DataType::BigInt).unwrap();
        let text_tree = BPlusTree::create(pool.bpm(), DataType::VARCHAR_MAX).unwrap();

        let null = tree.get(pool.bpm(), &Value::Null).unwrap_err();
        let mismatch = tree.get(pool.bpm(), &Value::Integer(1)).unwrap_err();
        let too_large = text_tree
            .get(pool.bpm(), &Value::Text("x".repeat(112)))
            .unwrap();

        assert!(matches!(null, IndexError::NullKey));
        assert!(matches!(mismatch, IndexError::KeyTypeMismatch { .. }));
        assert_eq!(too_large, None);
    }
}
