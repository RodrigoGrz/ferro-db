//! Pages of a B+Tree. Little-endian, after the 16-byte common header:
//!
//! **Header page** (`PageType::BPlusTreeHeader`), the fixed identity of a tree:
//!
//! | Bytes | Field |
//! |---|---|
//! | `16..20` | `root_page_id u32` |
//! | `20..24` | `free_list_head u32` (`0` = empty) |
//! | `24..28` | `key_type u32` (`key::key_type_code`) |
//! | `28..` | zero |
//!
//! **Free page** (`PageType::BPlusTreeFree`), freed by a merge and kept for reuse:
//! `16..20` `next_free u32` (`0` = last), then zero.
//!
//! **Node** (`PageType::BPlusTreeNode`), a leaf or an internal node:
//!
//! | Bytes | Field |
//! |---|---|
//! | `16..18` | `level u16`: `0` is a leaf; a child is always one level below its parent |
//! | `18..20` | `slot_count u16` |
//! | `20..22` | `data_start u16`: the entries fill `data_start..page_size` |
//! | `22..24` | reserved (zero) |
//! | `24..28` | `link u32`: a leaf's next leaf (`0` = none), an internal node's first child |
//! | `28..` | slots: `slot_count × u16`, the offset of each entry, in key order |
//!
//! A leaf entry is `key_len u16` · key · `Rid` (6 bytes); an internal entry *i* is
//! `key_len u16` · key · `child u32`, the child *i + 1*, whose keys are all ≥ key *i*.
//!
//! Every write rebuilds the whole node, so the entries are always packed in slot order from
//! `data_start` to the end of the page, with no holes. A read accepts only that form, with
//! keys strictly increasing: anything else is `Corrupted`.

use crate::serialization::{ByteReader, ByteWriter, Decode, SerializationError};
use crate::storage::page::{COMMON_HEADER_SIZE, PageHeader, PageType, RESERVED_PAGE_ID};
use crate::storage::rid::Rid;

use super::IndexError;

/// Node header (level, slot count, data start, reserved, link) ends here; slots follow.
pub(crate) const NODE_HEADER_END: usize = 28;
/// Slot, key length and the largest payload (a `Rid`).
const MAX_ENTRY_OVERHEAD: usize = 2 + 2 + 6;

/// A quarter of a node minus the entry overhead, so any node holds at least four entries of
/// the largest key: a split always leaves two halves that fit, and a separator always fits
/// its parent.
pub(crate) fn max_key_size_for(page_size: usize) -> usize {
    (page_size.saturating_sub(NODE_HEADER_END) / 4).saturating_sub(MAX_ENTRY_OVERHEAD)
}

const TOO_SMALL: &str = "page too small for a B+Tree page";

const SLOT_SIZE: usize = 2;
const KEY_LEN_SIZE: usize = 2;
const CHILD_SIZE: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeafEntry {
    pub key: Vec<u8>,
    pub rid: Rid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InternalEntry {
    pub key: Vec<u8>,
    pub child: u32,
}

/// A node as the tree changes it: decoded from a page, edited, then written back whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    Leaf {
        next: u32,
        entries: Vec<LeafEntry>,
    },
    Internal {
        level: u16,
        first_child: u32,
        entries: Vec<InternalEntry>,
    },
}

/// Bytes a leaf entry takes in a page, slot included.
pub(crate) fn leaf_entry_size(key_len: usize) -> usize {
    SLOT_SIZE + KEY_LEN_SIZE + key_len + Rid::SIZE
}

/// Bytes an internal entry takes in a page, slot included.
pub(crate) fn internal_entry_size(key_len: usize) -> usize {
    SLOT_SIZE + KEY_LEN_SIZE + key_len + CHILD_SIZE
}

impl Node {
    pub(crate) fn level(&self) -> u16 {
        match self {
            Node::Leaf { .. } => 0,
            Node::Internal { level, .. } => *level,
        }
    }

    pub(crate) fn encoded_size(&self) -> usize {
        let entries: usize = match self {
            Node::Leaf { entries, .. } => entries
                .iter()
                .map(|entry| leaf_entry_size(entry.key.len()))
                .sum(),
            Node::Internal { entries, .. } => entries
                .iter()
                .map(|entry| internal_entry_size(entry.key.len()))
                .sum(),
        };
        NODE_HEADER_END + entries
    }

    pub(crate) fn fits(&self, page_size: usize) -> bool {
        self.encoded_size() <= page_size
    }

    /// Entries taking less than a quarter of the space a node has for them. A split leaves two
    /// halves of about half each, so a node needs several removals before it merges back,
    /// instead of every insert and removal at a split point splitting and merging again.
    pub(crate) fn is_underfull(&self, page_size: usize) -> bool {
        let used = self.encoded_size().saturating_sub(NODE_HEADER_END);
        used < page_size.saturating_sub(NODE_HEADER_END) / 4
    }

    pub(crate) fn read(page_id: u32, page: &[u8], max_key_size: usize) -> Result<Node, IndexError> {
        expect_page_type(page_id, page, PageType::BPlusTreeNode, "not a B+Tree node")?;
        let truncated = |_: SerializationError| corrupted(page_id, TOO_SMALL);
        let mut header = fields(page_id, page)?;
        let level = header.read_u16().map_err(truncated)?;
        let slot_count = usize::from(header.read_u16().map_err(truncated)?);
        let data_start = usize::from(header.read_u16().map_err(truncated)?);
        header.skip(2).map_err(truncated)?;
        let link = header.read_u32().map_err(truncated)?;
        if data_start > page.len() {
            return Err(corrupted(page_id, "data start is past the end of the page"));
        }
        if NODE_HEADER_END + slot_count * SLOT_SIZE > data_start {
            return Err(corrupted(page_id, "slot directory overlaps the entries"));
        }
        let slots = (0..slot_count)
            .map(|_| header.read_u16().map(usize::from))
            .collect::<Result<Vec<_>, _>>()
            .map_err(truncated)?;
        let entries = EntryReader {
            page_id,
            page,
            data_start,
            max_key_size,
        };

        if level == 0 {
            let entries = entries.read(&slots, Rid::decode)?;
            return Ok(Node::Leaf {
                next: link,
                entries: entries
                    .into_iter()
                    .map(|(key, rid)| LeafEntry { key, rid })
                    .collect(),
            });
        }
        let entries = entries.read(&slots, |reader| reader.read_u32())?;
        let children = std::iter::once(link).chain(entries.iter().map(|(_, child)| *child));
        if children.into_iter().any(|child| child == RESERVED_PAGE_ID) {
            return Err(corrupted(page_id, "child is the reserved page"));
        }
        Ok(Node::Internal {
            level,
            first_child: link,
            entries: entries
                .into_iter()
                .map(|(key, child)| InternalEntry { key, child })
                .collect(),
        })
    }

    /// Callers check `fits` first: a node that does not fit is a bug, reported, never written.
    pub(crate) fn write(&self, page_id: u32, page: &mut [u8]) -> Result<(), IndexError> {
        if !self.fits(page.len()) {
            return Err(corrupted(page_id, "node does not fit its page"));
        }
        let (level, link, entries): (u16, u32, Vec<EncodedEntry<'_>>) = match self {
            Node::Leaf { next, entries } => (
                0,
                *next,
                entries
                    .iter()
                    .map(|entry| (entry.key.as_slice(), entry.rid.to_bytes().to_vec()))
                    .collect(),
            ),
            Node::Internal {
                level,
                first_child,
                entries,
            } => (
                *level,
                *first_child,
                entries
                    .iter()
                    .map(|entry| (entry.key.as_slice(), entry.child.to_le_bytes().to_vec()))
                    .collect(),
            ),
        };
        let too_small = |_| corrupted(page_id, TOO_SMALL);
        let to_u16 = |value: usize| u16::try_from(value).map_err(|_| corrupted(page_id, TOO_SMALL));
        let entry_data_size =
            |(key, payload): &EncodedEntry<'_>| KEY_LEN_SIZE + key.len() + payload.len();
        let data_size: usize = entries.iter().map(entry_data_size).sum();
        let data_start = page
            .len()
            .checked_sub(data_size)
            .ok_or_else(|| corrupted(page_id, TOO_SMALL))?;
        let gap = data_start
            .checked_sub(NODE_HEADER_END + SLOT_SIZE * entries.len())
            .ok_or_else(|| corrupted(page_id, TOO_SMALL))?;

        PageHeader::format(page, PageType::BPlusTreeNode)
            .map_err(|_| corrupted(page_id, TOO_SMALL))?;
        let body = page
            .get_mut(COMMON_HEADER_SIZE..)
            .ok_or_else(|| corrupted(page_id, TOO_SMALL))?;
        let mut writer = ByteWriter::new(body);
        writer.write_u16(level).map_err(too_small)?;
        writer
            .write_u16(to_u16(entries.len())?)
            .map_err(too_small)?;
        writer.write_u16(to_u16(data_start)?).map_err(too_small)?;
        writer.write_u16(0).map_err(too_small)?;
        writer.write_u32(link).map_err(too_small)?;
        let mut offset = data_start;
        for entry in &entries {
            writer.write_u16(to_u16(offset)?).map_err(too_small)?;
            offset += entry_data_size(entry);
        }
        writer.write_zeros(gap).map_err(too_small)?;
        for (key, payload) in &entries {
            writer.write_u16(to_u16(key.len())?).map_err(too_small)?;
            writer.write_bytes(key).map_err(too_small)?;
            writer.write_bytes(payload).map_err(too_small)?;
        }
        Ok(())
    }
}

/// A key and its payload bytes (a `Rid` or a child id), as a write lays them out.
type EncodedEntry<'a> = (&'a [u8], Vec<u8>);

/// Reads the entries of a node, accepting only the packed form a write produces.
struct EntryReader<'a> {
    page_id: u32,
    page: &'a [u8],
    data_start: usize,
    max_key_size: usize,
}

impl EntryReader<'_> {
    fn read<T>(
        &self,
        slots: &[usize],
        read_payload: impl Fn(&mut ByteReader<'_>) -> Result<T, SerializationError>,
    ) -> Result<Vec<(Vec<u8>, T)>, IndexError> {
        let past_the_end = |_| corrupted(self.page_id, "entry runs past the end of the page");
        let data = self.page.get(self.data_start..).unwrap_or_default();
        let mut reader = ByteReader::new(data);
        let mut entries = Vec::with_capacity(slots.len());
        for &offset in slots {
            if offset != self.data_start + reader.position() {
                return Err(corrupted(
                    self.page_id,
                    "entries are not packed in slot order",
                ));
            }
            let key_len = usize::from(reader.read_u16().map_err(past_the_end)?);
            if key_len > self.max_key_size {
                return Err(corrupted(self.page_id, "key longer than the maximum"));
            }
            let key = reader.read_bytes(key_len).map_err(past_the_end)?.to_vec();
            let payload = read_payload(&mut reader).map_err(past_the_end)?;
            entries.push((key, payload));
        }
        if reader.remaining() != 0 {
            return Err(corrupted(
                self.page_id,
                "entries do not end at the end of the page",
            ));
        }
        if !entries.is_sorted_by(|lhs, rhs| lhs.0 < rhs.0) {
            return Err(corrupted(self.page_id, "keys are not in increasing order"));
        }
        Ok(entries)
    }
}

fn corrupted(page_id: u32, reason: &'static str) -> IndexError {
    IndexError::Corrupted { page_id, reason }
}

fn expect_page_type(
    page_id: u32,
    page: &[u8],
    expected: PageType,
    reason: &'static str,
) -> Result<(), IndexError> {
    match PageHeader::read(page) {
        Ok(header) if header.page_type == expected => Ok(()),
        Ok(_) => Err(corrupted(page_id, reason)),
        Err(_) => Err(corrupted(page_id, TOO_SMALL)),
    }
}

/// The bytes after the common header, read one `u32` at a time.
fn fields(page_id: u32, page: &[u8]) -> Result<ByteReader<'_>, IndexError> {
    page.get(COMMON_HEADER_SIZE..)
        .map(ByteReader::new)
        .ok_or_else(|| corrupted(page_id, TOO_SMALL))
}

/// Stamps the page type (keeping LSN and checksum), writes `values` after the common header
/// and zeroes the rest of the page.
fn write_fields(
    page_id: u32,
    page: &mut [u8],
    page_type: PageType,
    values: &[u32],
) -> Result<(), IndexError> {
    let needed = COMMON_HEADER_SIZE + 4 * values.len();
    if page.len() < needed {
        return Err(corrupted(page_id, TOO_SMALL));
    }
    PageHeader::format(page, page_type).map_err(|_| corrupted(page_id, TOO_SMALL))?;
    let body = page
        .get_mut(COMMON_HEADER_SIZE..)
        .ok_or_else(|| corrupted(page_id, TOO_SMALL))?;
    let mut writer = ByteWriter::new(body);
    for &value in values {
        writer
            .write_u32(value)
            .map_err(|_| corrupted(page_id, TOO_SMALL))?;
    }
    let rest = writer.remaining();
    writer
        .write_zeros(rest)
        .map_err(|_| corrupted(page_id, TOO_SMALL))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TreeHeader {
    pub root_page_id: u32,
    pub free_list_head: u32,
    /// Which key type the tree was created for (`key::key_type_code`).
    pub key_type: u32,
}

impl TreeHeader {
    pub(crate) fn read(page_id: u32, page: &[u8]) -> Result<Self, IndexError> {
        expect_page_type(
            page_id,
            page,
            PageType::BPlusTreeHeader,
            "not a B+Tree header page",
        )?;
        let mut reader = fields(page_id, page)?;
        let mut next = || reader.read_u32().map_err(|_| corrupted(page_id, TOO_SMALL));
        let header = TreeHeader {
            root_page_id: next()?,
            free_list_head: next()?,
            key_type: next()?,
        };
        if header.root_page_id == RESERVED_PAGE_ID {
            return Err(corrupted(page_id, "the root is the reserved page"));
        }
        Ok(header)
    }

    pub(crate) fn write(&self, page_id: u32, page: &mut [u8]) -> Result<(), IndexError> {
        write_fields(
            page_id,
            page,
            PageType::BPlusTreeHeader,
            &[self.root_page_id, self.free_list_head, self.key_type],
        )
    }
}

/// The next free page after `page_id`.
pub(crate) fn read_free_page(page_id: u32, page: &[u8]) -> Result<u32, IndexError> {
    expect_page_type(
        page_id,
        page,
        PageType::BPlusTreeFree,
        "not a free B+Tree page",
    )?;
    fields(page_id, page)?
        .read_u32()
        .map_err(|_| corrupted(page_id, TOO_SMALL))
}

pub(crate) fn write_free_page(
    page_id: u32,
    page: &mut [u8],
    next_free: u32,
) -> Result<(), IndexError> {
    write_fields(page_id, page, PageType::BPlusTreeFree, &[next_free])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::page::{PAGE_FORMAT_VERSION, PAGE_TYPE_OFFSET, VERSION_OFFSET};

    const PAGE: usize = 512;

    /// Garbage everywhere, so a write must set every byte it owns.
    fn dirty_page() -> Vec<u8> {
        vec![0xEE; PAGE]
    }

    fn reason_of(err: IndexError) -> (u32, &'static str) {
        match err {
            IndexError::Corrupted { page_id, reason } => (page_id, reason),
            other => panic!("expected Corrupted, got {other:?}"),
        }
    }

    fn leaf(next: u32, entries: &[(&[u8], Rid)]) -> Node {
        Node::Leaf {
            next,
            entries: entries
                .iter()
                .map(|&(key, rid)| LeafEntry {
                    key: key.to_vec(),
                    rid,
                })
                .collect(),
        }
    }

    fn internal(level: u16, first_child: u32, entries: &[(&[u8], u32)]) -> Node {
        Node::Internal {
            level,
            first_child,
            entries: entries
                .iter()
                .map(|&(key, child)| InternalEntry {
                    key: key.to_vec(),
                    child,
                })
                .collect(),
        }
    }

    fn written(node: &Node) -> Vec<u8> {
        let mut page = dirty_page();
        node.write(9, &mut page).unwrap();
        page
    }

    #[test]
    fn leaf_write_should_produce_the_documented_bytes() {
        let node = leaf(9, &[(b"ab", Rid::new(3, 1)), (b"c", Rid::new(4, 2))]);

        let page = written(&node);

        // Entries take 10 + 9 bytes, so they start at 512 - 19 = 493 (0x01ED).
        assert_eq!(page[..8], [0xEE; 8], "the LSN is kept");
        assert_eq!(page[PAGE_TYPE_OFFSET], PageType::BPlusTreeNode as u8);
        assert_eq!(page[16..28], [0, 0, 2, 0, 0xED, 0x01, 0, 0, 9, 0, 0, 0]);
        assert_eq!(page[28..32], [0xED, 0x01, 0xF7, 0x01]);
        assert!(page[32..493].iter().all(|&byte| byte == 0));
        assert_eq!(page[493..503], [2, 0, b'a', b'b', 3, 0, 0, 0, 1, 0]);
        assert_eq!(page[503..], [1, 0, b'c', 4, 0, 0, 0, 2, 0]);
        assert_eq!(node.encoded_size(), 28 + 2 * 2 + 19);
    }

    #[test]
    fn internal_write_should_produce_the_documented_bytes() {
        let node = internal(2, 5, &[(b"m", 6)]);

        let page = written(&node);

        // One entry of 7 bytes at 512 - 7 = 505 (0x01F9).
        assert_eq!(page[16..28], [2, 0, 1, 0, 0xF9, 0x01, 0, 0, 5, 0, 0, 0]);
        assert_eq!(page[28..30], [0xF9, 0x01]);
        assert_eq!(page[505..], [1, 0, b'm', 6, 0, 0, 0]);
    }

    #[test]
    fn nodes_should_round_trip() {
        let max_key = vec![0xAB; max_key_size_for(PAGE)];
        let nodes = [
            leaf(0, &[]),
            leaf(7, &[(b"", Rid::new(1, 0))]),
            leaf(
                0,
                &[
                    (b"a", Rid::new(1, 0)),
                    (&max_key, Rid::new(u32::MAX, u16::MAX)),
                ],
            ),
            internal(1, 3, &[]),
            internal(3, 3, &[(b"", 4), (b"k", 5), (&max_key, u32::MAX)]),
        ];

        for node in nodes {
            let page = written(&node);

            assert_eq!(Node::read(9, &page, max_key_size_for(PAGE)).unwrap(), node);
        }
    }

    #[test]
    fn a_node_exactly_the_size_of_the_page_should_fit_and_one_byte_more_should_not() {
        let big_len = PAGE - NODE_HEADER_END - leaf_entry_size(1) - leaf_entry_size(0);
        let node_with = |big_len: usize| {
            let big = vec![1; big_len];
            leaf(0, &[(&[0], Rid::new(1, 0)), (&big, Rid::new(1, 1))])
        };
        let exact = node_with(big_len);
        let over = node_with(big_len + 1);

        let mut page = dirty_page();
        exact.write(9, &mut page).unwrap();
        let err = over.write(9, &mut dirty_page()).unwrap_err();

        assert_eq!(exact.encoded_size(), PAGE);
        assert!(exact.fits(PAGE));
        assert!(!over.fits(PAGE));
        assert_eq!(reason_of(err), (9, "node does not fit its page"));
        assert_eq!(Node::read(9, &page, PAGE).unwrap(), exact);
    }

    #[test]
    fn four_entries_of_the_largest_key_should_fit_any_node() {
        for page_size in [512, 4096, 8192, u16::MAX as usize] {
            let key = vec![0xFF; max_key_size_for(page_size)];
            let leaf_entries = (0..4).map(|slot| LeafEntry {
                key: key.clone(),
                rid: Rid::new(1, slot),
            });
            let internal_entries = (0..4).map(|child| InternalEntry {
                key: key.clone(),
                child: child + 1,
            });
            let full_leaf = Node::Leaf {
                next: 0,
                entries: leaf_entries.collect(),
            };
            let full_internal = Node::Internal {
                level: 1,
                first_child: 9,
                entries: internal_entries.collect(),
            };

            assert!(full_leaf.fits(page_size), "{page_size}");
            assert!(full_internal.fits(page_size), "{page_size}");
        }
    }

    /// A valid leaf with entries "a" and "c" on a 512-byte page, ready to be damaged.
    fn valid_leaf() -> Vec<u8> {
        written(&leaf(0, &[(b"a", Rid::new(3, 0)), (b"c", Rid::new(3, 1))]))
    }

    fn read_reason(page: &[u8]) -> &'static str {
        reason_of(Node::read(9, page, max_key_size_for(PAGE)).unwrap_err()).1
    }

    #[test]
    fn read_of_a_damaged_node_should_fail() {
        // valid_leaf: entries of 9 bytes each at 494 and 503; slots at 28 and 30.
        let mut header = dirty_page();
        TreeHeader {
            root_page_id: 3,
            free_list_head: 0,
            key_type: 3,
        }
        .write(9, &mut header)
        .unwrap();
        let mut too_small = vec![0; 20];
        PageHeader::format(&mut too_small, PageType::BPlusTreeNode).unwrap();
        let damaged = |damage: &dyn Fn(&mut Vec<u8>)| {
            let mut page = valid_leaf();
            damage(&mut page);
            page
        };
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (header, "not a B+Tree node"),
            (too_small, "page too small for a B+Tree page"),
            (
                damaged(&|page| page[20..22].copy_from_slice(&600u16.to_le_bytes())),
                "data start is past the end of the page",
            ),
            (
                damaged(&|page| page[18..20].copy_from_slice(&300u16.to_le_bytes())),
                "slot directory overlaps the entries",
            ),
            (
                damaged(&|page| page[28] += 1),
                "entries are not packed in slot order",
            ),
            (
                damaged(&|page| page[503..505].copy_from_slice(&200u16.to_le_bytes())),
                "key longer than the maximum",
            ),
            (
                damaged(&|page| page[503..505].copy_from_slice(&50u16.to_le_bytes())),
                "entry runs past the end of the page",
            ),
            (
                damaged(&|page| {
                    page[20..22].copy_from_slice(&493u16.to_le_bytes());
                    page[28..30].copy_from_slice(&493u16.to_le_bytes());
                    page[30..32].copy_from_slice(&502u16.to_le_bytes());
                    page.copy_within(494.., 493);
                }),
                "entries do not end at the end of the page",
            ),
            (
                written(&leaf(0, &[(b"c", Rid::new(3, 0)), (b"a", Rid::new(3, 1))])),
                "keys are not in increasing order",
            ),
            (
                written(&leaf(0, &[(b"a", Rid::new(3, 0)), (b"a", Rid::new(3, 1))])),
                "keys are not in increasing order",
            ),
            (
                written(&internal(1, 0, &[(b"a", 4)])),
                "child is the reserved page",
            ),
            (
                written(&internal(1, 3, &[(b"a", 0)])),
                "child is the reserved page",
            ),
        ];

        for (page, reason) in cases {
            assert_eq!(read_reason(&page), reason);
        }
    }

    #[test]
    fn random_corruption_should_never_panic() {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as usize
        };
        for _ in 0..2_000 {
            let entries: Vec<(Vec<u8>, Rid)> = (0..next(12))
                .map(|i| (vec![i as u8; 1 + next(8)], Rid::new(1, i as u16)))
                .collect();
            let borrowed: Vec<(&[u8], Rid)> = entries
                .iter()
                .map(|(key, rid)| (key.as_slice(), *rid))
                .collect();
            let mut page = written(&leaf(3, &borrowed));
            for _ in 0..1 + next(4) {
                let at = next(PAGE);
                page[at] ^= 1 << next(8);
            }
            page.truncate(PAGE - next(3) * next(PAGE / 2));

            let _ = Node::read(9, &page, max_key_size_for(PAGE));
        }
    }

    #[test]
    fn max_key_size_should_leave_room_for_four_entries() {
        assert_eq!(max_key_size_for(512), 111);
        assert_eq!(max_key_size_for(8192), 2031);
    }

    #[test]
    fn tree_header_write_should_produce_the_documented_bytes() {
        let mut page = dirty_page();
        let header = TreeHeader {
            root_page_id: 7,
            free_list_head: 0x0102_0304,
            key_type: 9,
        };

        header.write(9, &mut page).unwrap();

        assert_eq!(page[..8], [0xEE; 8], "the LSN is kept");
        assert_eq!(page[PAGE_TYPE_OFFSET], PageType::BPlusTreeHeader as u8);
        assert_eq!(page[VERSION_OFFSET], PAGE_FORMAT_VERSION);
        assert_eq!(page[16..20], [7, 0, 0, 0]);
        assert_eq!(page[20..24], [4, 3, 2, 1]);
        assert_eq!(page[24..28], [9, 0, 0, 0]);
        assert!(page[28..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn tree_header_should_round_trip() {
        let mut page = dirty_page();
        let header = TreeHeader {
            root_page_id: u32::MAX,
            free_list_head: 0,
            key_type: 4,
        };
        header.write(9, &mut page).unwrap();

        assert_eq!(TreeHeader::read(9, &page).unwrap(), header);
    }

    #[test]
    fn tree_header_read_of_another_page_should_fail() {
        let never_written = vec![0; PAGE];
        let mut free = dirty_page();
        write_free_page(9, &mut free, 0).unwrap();

        for page in [never_written, free] {
            let err = TreeHeader::read(9, &page).unwrap_err();

            assert_eq!(reason_of(err), (9, "not a B+Tree header page"));
        }
    }

    #[test]
    fn tree_header_read_of_a_reserved_root_should_fail() {
        let mut page = dirty_page();
        TreeHeader {
            root_page_id: RESERVED_PAGE_ID,
            free_list_head: 0,
            key_type: 3,
        }
        .write(9, &mut page)
        .unwrap();

        let err = TreeHeader::read(9, &page).unwrap_err();

        assert_eq!(reason_of(err), (9, "the root is the reserved page"));
    }

    #[test]
    fn free_page_write_should_produce_the_documented_bytes() {
        let mut page = dirty_page();

        write_free_page(4, &mut page, 0x0A0B_0C0D).unwrap();

        assert_eq!(page[..8], [0xEE; 8], "the LSN is kept");
        assert_eq!(page[PAGE_TYPE_OFFSET], PageType::BPlusTreeFree as u8);
        assert_eq!(page[16..20], [0x0D, 0x0C, 0x0B, 0x0A]);
        assert!(page[20..].iter().all(|&byte| byte == 0));
        assert_eq!(read_free_page(4, &page).unwrap(), 0x0A0B_0C0D);
    }

    #[test]
    fn free_page_read_of_another_page_should_fail() {
        let mut header = dirty_page();
        TreeHeader {
            root_page_id: 3,
            free_list_head: 0,
            key_type: 3,
        }
        .write(4, &mut header)
        .unwrap();

        let err = read_free_page(4, &header).unwrap_err();

        assert_eq!(reason_of(err), (4, "not a free B+Tree page"));
    }

    #[test]
    fn pages_too_small_should_fail_without_panicking() {
        let mut tiny = vec![0; COMMON_HEADER_SIZE + 2];
        let header = TreeHeader {
            root_page_id: 3,
            free_list_head: 0,
            key_type: 3,
        };

        let write = header.write(4, &mut tiny).unwrap_err();
        PageHeader::format(&mut tiny, PageType::BPlusTreeHeader).unwrap();
        let read = TreeHeader::read(4, &tiny).unwrap_err();

        assert_eq!(reason_of(write), (4, "page too small for a B+Tree page"));
        assert_eq!(reason_of(read), (4, "page too small for a B+Tree page"));
    }
}
