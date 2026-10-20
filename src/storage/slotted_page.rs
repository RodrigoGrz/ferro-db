//! Slotted page layout (all integers little-endian):
//!
//! ```text
//! [0..16)  common header   LSN, checksum, page type = Slotted, version (see `page`)
//! [16..18) slot_count      u16
//! [18..20) free_space_end  u16  start of the tuple area
//! [20..24) next_page_id    u32  next page of the heap file, 0 = none (page 0 is reserved)
//! [24..)   slot array      (offset u16, len u16) per slot, grows forward
//! ...      free space
//! [free_space_end..page_size) tuples, grow backward
//! ```
//!
//! A slot with `offset == 0` is a tombstone. Deleted tuples leave holes that are
//! reclaimed lazily by `compact`, which moves bytes but never changes slot ids,
//! so a `Rid` stays valid for as long as its tuple lives.

use std::fmt;
use std::num::NonZeroU32;

use crate::config::MAX_PAGE_SIZE;
use crate::serialization::{get_u16, get_u32, put_u16, put_u32};
use crate::storage::page::{COMMON_HEADER_SIZE, PageError, PageHeader, PageType};

const SLOT_COUNT_OFFSET: usize = COMMON_HEADER_SIZE;
const FREE_SPACE_END_OFFSET: usize = SLOT_COUNT_OFFSET + 2;
const NEXT_PAGE_ID_OFFSET: usize = FREE_SPACE_END_OFFSET + 2;
pub const HEADER_SIZE: usize = NEXT_PAGE_ID_OFFSET + 4;
pub const SLOT_SIZE: usize = 4;

const TOMBSTONE: u16 = 0;

#[derive(Debug, PartialEq, Eq)]
pub enum SlottedPageError {
    PageTooSmall { size: usize },
    PageTooLarge { size: usize },
    Uninitialized,
    Corrupted { reason: &'static str },
    PageFull,
    TupleTooLarge { size: usize, max: usize },
    InvalidSlot { slot_id: u16 },
    SlotDeleted { slot_id: u16 },
    InvalidHeader(PageError),
    WrongPageType { found: PageType },
}

impl fmt::Display for SlottedPageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SlottedPageError::PageTooSmall { size } => write!(
                f,
                "page size {size} is smaller than the slotted page header ({HEADER_SIZE} bytes)"
            ),
            SlottedPageError::PageTooLarge { size } => {
                write!(
                    f,
                    "page size {size} exceeds maximum of {MAX_PAGE_SIZE} bytes"
                )
            }
            SlottedPageError::Uninitialized => write!(f, "slotted page is not initialized"),
            SlottedPageError::Corrupted { reason } => {
                write!(f, "slotted page is corrupted: {reason}")
            }
            SlottedPageError::PageFull => write!(f, "not enough free space in page"),
            SlottedPageError::TupleTooLarge { size, max } => {
                write!(f, "tuple of {size} bytes exceeds maximum of {max} bytes")
            }
            SlottedPageError::InvalidSlot { slot_id } => write!(f, "slot {slot_id} does not exist"),
            SlottedPageError::SlotDeleted { slot_id } => write!(f, "slot {slot_id} is deleted"),
            SlottedPageError::InvalidHeader(err) => write!(f, "{err}"),
            SlottedPageError::WrongPageType { found } => {
                write!(f, "expected a slotted page, found {found:?}")
            }
        }
    }
}

impl std::error::Error for SlottedPageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SlottedPageError::InvalidHeader(err) => Some(err),
            _ => None,
        }
    }
}

impl From<PageError> for SlottedPageError {
    fn from(err: PageError) -> Self {
        SlottedPageError::InvalidHeader(err)
    }
}

/// Zero-copy view over the bytes of a page.
///
/// `SlottedPage<&[u8]>` reads, `SlottedPage<&mut [u8]>` (or `Vec<u8>`) also writes.
#[derive(Debug)]
pub struct SlottedPage<B> {
    data: B,
}

pub fn check_page_size(size: usize) -> Result<(), SlottedPageError> {
    if size < HEADER_SIZE {
        return Err(SlottedPageError::PageTooSmall { size });
    }
    if size > MAX_PAGE_SIZE {
        return Err(SlottedPageError::PageTooLarge { size });
    }
    Ok(())
}

/// Largest tuple an empty page of `page_size` bytes holds: everything but the header
/// and one slot.
pub fn max_tuple_size_for(page_size: usize) -> usize {
    page_size.saturating_sub(HEADER_SIZE + SLOT_SIZE)
}

fn slot_position(slot_id: u16) -> usize {
    HEADER_SIZE + slot_id as usize * SLOT_SIZE
}

impl<B: AsRef<[u8]>> SlottedPage<B> {
    /// Opens an already initialized page, validating its header and slots.
    pub fn new(data: B) -> Result<Self, SlottedPageError> {
        check_page_size(data.as_ref().len())?;
        match PageHeader::read(data.as_ref())?.page_type {
            PageType::Free => return Err(SlottedPageError::Uninitialized),
            PageType::Slotted => {}
            found @ (PageType::DatabaseHeader
            | PageType::BPlusTreeHeader
            | PageType::BPlusTreeNode
            | PageType::BPlusTreeFree) => {
                return Err(SlottedPageError::WrongPageType { found });
            }
        }
        let page = Self { data };
        page.validate()?;
        Ok(page)
    }

    pub fn get(&self, slot_id: u16) -> Result<&[u8], SlottedPageError> {
        let (offset, len) = self.live_slot(slot_id)?;
        Ok(&self.bytes()[offset..offset + len])
    }

    /// Live tuples in slot order, skipping tombstones.
    pub fn iter(&self) -> impl Iterator<Item = (u16, &[u8])> + '_ {
        (0..self.slot_count())
            .filter_map(move |slot_id| self.get(slot_id).ok().map(|tuple| (slot_id, tuple)))
    }

    pub fn slot_count(&self) -> u16 {
        get_u16(self.bytes(), SLOT_COUNT_OFFSET)
    }

    /// Free bytes between the slot array and the tuple area.
    pub fn contiguous_free_space(&self) -> usize {
        self.free_space_end() - self.slot_array_end()
    }

    /// Contiguous free space plus the holes left by deleted or shrunk tuples.
    pub fn total_free_space(&self) -> usize {
        self.page_size() - self.slot_array_end() - self.live_bytes()
    }

    pub fn max_tuple_size(&self) -> usize {
        max_tuple_size_for(self.page_size())
    }

    pub fn next_page_id(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(get_u32(self.bytes(), NEXT_PAGE_ID_OFFSET))
    }

    /// Largest tuple `insert` accepts right now: free space minus a new slot, unless a
    /// deleted slot can be reused; never above `max_tuple_size`. `None` when not even an
    /// empty tuple fits, which `Some(0)` would hide.
    pub fn largest_insertable(&self) -> Option<usize> {
        let new_slot_cost = self.insert_cost(0);
        self.total_free_space()
            .checked_sub(new_slot_cost)
            .map(|room| room.min(self.max_tuple_size()))
    }

    pub fn can_insert(&self, len: usize) -> bool {
        len <= self.max_tuple_size() && self.insert_cost(len) <= self.total_free_space()
    }

    fn bytes(&self) -> &[u8] {
        self.data.as_ref()
    }

    fn page_size(&self) -> usize {
        self.bytes().len()
    }

    fn free_space_end(&self) -> usize {
        get_u16(self.bytes(), FREE_SPACE_END_OFFSET) as usize
    }

    fn slot_array_end(&self) -> usize {
        slot_position(self.slot_count())
    }

    fn slot(&self, slot_id: u16) -> (u16, u16) {
        let position = slot_position(slot_id);
        (
            get_u16(self.bytes(), position),
            get_u16(self.bytes(), position + 2),
        )
    }

    fn live_slot(&self, slot_id: u16) -> Result<(usize, usize), SlottedPageError> {
        if slot_id >= self.slot_count() {
            return Err(SlottedPageError::InvalidSlot { slot_id });
        }
        let (offset, len) = self.slot(slot_id);
        if offset == TOMBSTONE {
            return Err(SlottedPageError::SlotDeleted { slot_id });
        }
        Ok((offset as usize, len as usize))
    }

    fn live_slots(&self) -> impl Iterator<Item = (u16, usize, usize)> + '_ {
        (0..self.slot_count()).filter_map(move |slot_id| {
            let (offset, len) = self.slot(slot_id);
            (offset != TOMBSTONE).then_some((slot_id, offset as usize, len as usize))
        })
    }

    fn live_bytes(&self) -> usize {
        self.live_slots().map(|(_, _, len)| len).sum()
    }

    fn first_free_slot(&self) -> Option<u16> {
        (0..self.slot_count()).find(|&slot_id| self.slot(slot_id).0 == TOMBSTONE)
    }

    /// Bytes an insert of `len` consumes: the tuple plus a new slot unless one is reused.
    fn insert_cost(&self, len: usize) -> usize {
        match self.first_free_slot() {
            Some(_) => len,
            None => len + SLOT_SIZE,
        }
    }

    fn validate(&self) -> Result<(), SlottedPageError> {
        let free_space_end = self.free_space_end();
        let page_size = self.page_size();
        let slot_array_end = self.slot_array_end();
        if slot_array_end > page_size {
            return Err(SlottedPageError::Corrupted {
                reason: "slot array exceeds page",
            });
        }
        if free_space_end < slot_array_end {
            return Err(SlottedPageError::Corrupted {
                reason: "free space end overlaps slot array",
            });
        }
        if free_space_end > page_size {
            return Err(SlottedPageError::Corrupted {
                reason: "free space end exceeds page",
            });
        }

        for (_, offset, len) in self.live_slots() {
            if offset < free_space_end {
                return Err(SlottedPageError::Corrupted {
                    reason: "tuple starts before the tuple area",
                });
            }
            if offset + len > page_size {
                return Err(SlottedPageError::Corrupted {
                    reason: "tuple exceeds page",
                });
            }
        }

        Ok(())
    }
}

impl<B: AsRef<[u8]> + AsMut<[u8]>> SlottedPage<B> {
    /// Formats `data` as an empty slotted page, keeping LSN and checksum.
    pub fn init(mut data: B) -> Result<Self, SlottedPageError> {
        let size = data.as_ref().len();
        check_page_size(size)?;

        let bytes = data.as_mut();
        PageHeader::format(bytes, PageType::Slotted)?;
        put_u16(bytes, SLOT_COUNT_OFFSET, 0);
        put_u16(bytes, FREE_SPACE_END_OFFSET, size as u16);
        put_u32(bytes, NEXT_PAGE_ID_OFFSET, 0);

        Ok(Self { data })
    }

    pub fn insert(&mut self, tuple: &[u8]) -> Result<u16, SlottedPageError> {
        let len = tuple.len();
        self.check_tuple_size(len)?;

        let reused_slot = self.first_free_slot();
        let cost = match reused_slot {
            Some(_) => len,
            None => len + SLOT_SIZE,
        };
        if cost > self.total_free_space() {
            return Err(SlottedPageError::PageFull);
        }
        if cost > self.contiguous_free_space() {
            self.compact();
        }

        let slot_id = match reused_slot {
            Some(slot_id) => slot_id,
            None => {
                let slot_id = self.slot_count();
                self.set_slot_count(slot_id + 1);
                slot_id
            }
        };
        self.place(slot_id, tuple);
        Ok(slot_id)
    }

    pub fn delete(&mut self, slot_id: u16) -> Result<(), SlottedPageError> {
        self.live_slot(slot_id)?;
        self.set_slot(slot_id, TOMBSTONE as usize, 0);
        Ok(())
    }

    /// Replaces a tuple keeping its slot id. Fails with `PageFull` without
    /// changing the page when the new tuple does not fit.
    pub fn update(&mut self, slot_id: u16, tuple: &[u8]) -> Result<(), SlottedPageError> {
        let (offset, old_len) = self.live_slot(slot_id)?;
        let len = tuple.len();
        self.check_tuple_size(len)?;

        if len <= old_len {
            self.bytes_mut()[offset..offset + len].copy_from_slice(tuple);
            self.set_slot(slot_id, offset, len);
            return Ok(());
        }

        if len > self.total_free_space() + old_len {
            return Err(SlottedPageError::PageFull);
        }

        // The old bytes become a hole; the slot itself stays allocated.
        self.set_slot(slot_id, TOMBSTONE as usize, 0);
        if len > self.contiguous_free_space() {
            self.compact();
        }
        self.place(slot_id, tuple);
        Ok(())
    }

    pub fn set_next_page_id(&mut self, next: Option<NonZeroU32>) {
        put_u32(
            self.bytes_mut(),
            NEXT_PAGE_ID_OFFSET,
            next.map_or(0, NonZeroU32::get),
        );
    }

    /// Packs live tuples at the end of the page, keeping every slot id.
    pub fn compact(&mut self) {
        let mut live: Vec<(u16, usize, usize)> = self.live_slots().collect();
        // Highest offset first: each tuple moves toward the end without
        // overwriting a tuple that has not been moved yet.
        live.sort_by_key(|&(_, offset, _)| std::cmp::Reverse(offset));

        let mut cursor = self.page_size();
        for (slot_id, offset, len) in live {
            cursor -= len;
            self.bytes_mut().copy_within(offset..offset + len, cursor);
            self.set_slot(slot_id, cursor, len);
        }
        self.set_free_space_end(cursor);
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        self.data.as_mut()
    }

    fn check_tuple_size(&self, len: usize) -> Result<(), SlottedPageError> {
        let max = self.max_tuple_size();
        if len > max {
            return Err(SlottedPageError::TupleTooLarge { size: len, max });
        }
        Ok(())
    }

    /// Writes `tuple` at the start of the free space. The caller guarantees it fits.
    fn place(&mut self, slot_id: u16, tuple: &[u8]) {
        let offset = self.free_space_end() - tuple.len();
        debug_assert!(offset >= self.slot_array_end());

        self.bytes_mut()[offset..offset + tuple.len()].copy_from_slice(tuple);
        self.set_free_space_end(offset);
        self.set_slot(slot_id, offset, tuple.len());
    }

    fn set_slot_count(&mut self, slot_count: u16) {
        put_u16(self.bytes_mut(), SLOT_COUNT_OFFSET, slot_count);
    }

    fn set_free_space_end(&mut self, free_space_end: usize) {
        put_u16(
            self.bytes_mut(),
            FREE_SPACE_END_OFFSET,
            free_space_end as u16,
        );
    }

    fn set_slot(&mut self, slot_id: u16, offset: usize, len: usize) {
        let position = slot_position(slot_id);
        put_u16(self.bytes_mut(), position, offset as u16);
        put_u16(self.bytes_mut(), position + 2, len as u16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PAGE_SIZE;
    use crate::serialization::SerializationError;
    use crate::storage::page::{PAGE_FORMAT_VERSION, PAGE_TYPE_OFFSET, VERSION_OFFSET};
    use std::collections::HashMap;

    /// Small page where capacity math is easy to follow: 64 - 24 = 40 usable bytes.
    const SMALL: usize = 64;

    type TestPage = SlottedPage<Vec<u8>>;

    fn empty(size: usize) -> TestPage {
        SlottedPage::init(vec![0; size]).unwrap()
    }

    fn snapshot(page: &TestPage) -> Vec<u8> {
        page.data.clone()
    }

    fn tuple(len: usize, fill: u8) -> Vec<u8> {
        vec![fill; len]
    }

    /// Checks every structural invariant, including ones `new` skips for speed.
    fn assert_invariants(page: &TestPage) {
        SlottedPage::new(page.data.as_slice()).expect("page should validate");

        let mut ranges: Vec<(usize, usize)> = page
            .live_slots()
            .filter(|&(_, _, len)| len > 0)
            .map(|(_, offset, len)| (offset, offset + len))
            .collect();
        ranges.sort();
        for pair in ranges.windows(2) {
            assert!(pair[0].1 <= pair[1].0, "tuples overlap: {pair:?}");
        }

        assert!(page.contiguous_free_space() <= page.total_free_space());
        assert_eq!(
            page.total_free_space(),
            page.page_size() - page.slot_array_end() - page.live_bytes()
        );
    }

    // --- init / validation -------------------------------------------------

    #[test]
    fn init_should_set_empty_header() {
        let page = empty(PAGE_SIZE);

        assert_eq!(page.slot_count(), 0);
        assert_eq!(page.free_space_end(), PAGE_SIZE);
        assert_eq!(page.contiguous_free_space(), PAGE_SIZE - HEADER_SIZE);
        assert_eq!(page.total_free_space(), PAGE_SIZE - HEADER_SIZE);
        assert_eq!(page.next_page_id(), None);
        assert_eq!(page.iter().count(), 0);
    }

    #[test]
    fn init_should_preserve_lsn_and_checksum_and_clear_the_next_page() {
        let mut data = vec![0; SMALL];
        data[..PAGE_TYPE_OFFSET].fill(0xAB);
        data[NEXT_PAGE_ID_OFFSET..HEADER_SIZE].fill(0xFF);

        let page = SlottedPage::init(data).unwrap();

        assert!(page.data[..PAGE_TYPE_OFFSET].iter().all(|&b| b == 0xAB));
        assert_eq!(page.next_page_id(), None);
    }

    fn page_id(value: u32) -> Option<NonZeroU32> {
        NonZeroU32::new(value)
    }

    #[test]
    fn next_page_id_should_round_trip() {
        let mut page = empty(SMALL);

        for next in [page_id(1), page_id(u32::MAX), None] {
            page.set_next_page_id(next);

            assert_eq!(page.next_page_id(), next);
        }
    }

    #[test]
    fn next_page_id_should_be_stored_little_endian_after_free_space_end() {
        let mut page = empty(SMALL);

        page.set_next_page_id(page_id(0x0403_0201));

        assert_eq!(page.data[20..24], [1, 2, 3, 4]);
    }

    #[test]
    fn set_next_page_id_should_not_touch_slots_or_tuples() {
        let mut page = empty(SMALL);
        page.insert(b"abc").unwrap();
        let before = snapshot(&page);

        page.set_next_page_id(page_id(9));

        let after = snapshot(&page);
        assert_eq!(before[..NEXT_PAGE_ID_OFFSET], after[..NEXT_PAGE_ID_OFFSET]);
        assert_eq!(before[HEADER_SIZE..], after[HEADER_SIZE..]);
        assert_eq!(page.get(0).unwrap(), b"abc");
        assert_invariants(&page);
    }

    #[test]
    fn init_should_stamp_slotted_page_type_and_version() {
        let page = empty(SMALL);

        let header = PageHeader::read(&page.data).unwrap();

        assert_eq!(header.page_type, PageType::Slotted);
        assert_eq!(header.version, PAGE_FORMAT_VERSION);
    }

    #[test]
    fn slotted_header_should_follow_the_sixteen_byte_common_header() {
        let mut page = empty(SMALL);

        page.insert(b"abc").unwrap();

        assert_eq!(get_u16(&page.data, 16), 1);
        assert_eq!(get_u16(&page.data, 18), (SMALL - 3) as u16);
        assert_eq!(get_u16(&page.data, HEADER_SIZE), (SMALL - 3) as u16);
        assert_eq!(get_u16(&page.data, HEADER_SIZE + 2), 3);
    }

    #[test]
    fn new_on_a_page_of_the_b_plus_tree_should_fail() {
        for value in 3..=5 {
            let mut page = empty(SMALL);
            page.data[PAGE_TYPE_OFFSET] = value;

            let err = SlottedPage::new(page.data).unwrap_err();

            let found = PageType::try_from(value).unwrap();
            assert_eq!(err, SlottedPageError::WrongPageType { found });
        }
    }

    #[test]
    fn new_on_a_page_of_another_type_should_fail() {
        let mut page = empty(SMALL);
        page.data[PAGE_TYPE_OFFSET] = PageType::DatabaseHeader as u8;

        let err = SlottedPage::new(page.data).unwrap_err();

        assert_eq!(
            err,
            SlottedPageError::WrongPageType {
                found: PageType::DatabaseHeader
            }
        );
        assert_eq!(
            err.to_string(),
            "expected a slotted page, found DatabaseHeader"
        );
    }

    #[test]
    fn new_on_free_page_with_slotted_fields_should_report_uninitialized() {
        let mut page = empty(SMALL);
        page.data[PAGE_TYPE_OFFSET] = PageType::Free as u8;

        let err = SlottedPage::new(page.data).unwrap_err();

        assert_eq!(err, SlottedPageError::Uninitialized);
    }

    #[test]
    fn new_on_page_with_unknown_type_should_fail() {
        let mut page = empty(SMALL);
        page.data[PAGE_TYPE_OFFSET] = 7;

        let err = SlottedPage::new(page.data).unwrap_err();

        assert_eq!(
            err,
            SlottedPageError::InvalidHeader(PageError::Serialization(
                SerializationError::InvalidValue {
                    field: "page_type",
                    value: 7
                }
            ))
        );
    }

    #[test]
    fn new_on_page_from_other_format_version_should_fail() {
        let mut page = empty(SMALL);
        page.data[VERSION_OFFSET] = PAGE_FORMAT_VERSION + 1;

        let err = SlottedPage::new(page.data).unwrap_err();

        assert_eq!(
            err,
            SlottedPageError::InvalidHeader(PageError::UnsupportedVersion {
                found: PAGE_FORMAT_VERSION + 1,
                expected: PAGE_FORMAT_VERSION
            })
        );
    }

    #[test]
    fn invalid_header_error_should_expose_its_source() {
        let err = SlottedPageError::InvalidHeader(PageError::UnsupportedVersion {
            found: 2,
            expected: 1,
        });

        assert_eq!(
            err.to_string(),
            "page format version 2 is not supported (expected 1)"
        );
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn init_should_reset_a_used_page() {
        let mut page = empty(SMALL);
        page.insert(b"abc").unwrap();

        let page = SlottedPage::init(page.data).unwrap();

        assert_eq!(page.slot_count(), 0);
        assert_eq!(page.total_free_space(), SMALL - HEADER_SIZE);
    }

    #[test]
    fn new_on_zeroed_page_should_report_uninitialized() {
        let err = SlottedPage::new(vec![0; PAGE_SIZE]).unwrap_err();

        assert_eq!(err, SlottedPageError::Uninitialized);
    }

    #[test]
    fn new_should_open_initialized_page() {
        let page = empty(SMALL);

        let reopened = SlottedPage::new(page.data).unwrap();

        assert_eq!(reopened.slot_count(), 0);
    }

    #[test]
    fn new_should_reject_page_smaller_than_header() {
        let err = SlottedPage::new(vec![0; HEADER_SIZE - 4]).unwrap_err();
        assert_eq!(
            err,
            SlottedPageError::PageTooSmall {
                size: HEADER_SIZE - 4
            }
        );

        let err = SlottedPage::init(vec![0; HEADER_SIZE - 4]).unwrap_err();
        assert!(matches!(err, SlottedPageError::PageTooSmall { .. }));
    }

    #[test]
    fn page_of_exactly_header_size_should_hold_no_tuples() {
        let mut page = empty(HEADER_SIZE);

        assert_eq!(page.total_free_space(), 0);
        assert!(!page.can_insert(0));
        assert!(page.insert(b"").is_err());
    }

    #[test]
    fn new_should_reject_page_larger_than_u16_range() {
        let size = MAX_PAGE_SIZE + 1;

        assert_eq!(
            SlottedPage::new(vec![0; size]).unwrap_err(),
            SlottedPageError::PageTooLarge { size }
        );
        assert_eq!(
            SlottedPage::init(vec![0; size]).unwrap_err(),
            SlottedPageError::PageTooLarge { size }
        );
    }

    #[test]
    fn max_page_size_should_work() {
        let mut page = empty(MAX_PAGE_SIZE);
        let big = tuple(page.max_tuple_size(), 7);

        let slot_id = page.insert(&big).unwrap();

        assert_eq!(page.get(slot_id).unwrap(), big.as_slice());
        assert_invariants(&page);
    }

    #[test]
    fn new_should_reject_free_end_before_slot_array() {
        let mut page = empty(SMALL);
        page.insert(b"abc").unwrap();
        put_u16(
            &mut page.data,
            FREE_SPACE_END_OFFSET,
            (HEADER_SIZE + 2) as u16,
        );

        let err = SlottedPage::new(page.data).unwrap_err();

        assert!(matches!(err, SlottedPageError::Corrupted { .. }));
    }

    #[test]
    fn new_should_reject_free_end_after_page_end() {
        let mut page = empty(SMALL);
        put_u16(&mut page.data, FREE_SPACE_END_OFFSET, (SMALL + 1) as u16);

        let err = SlottedPage::new(page.data).unwrap_err();

        assert!(matches!(err, SlottedPageError::Corrupted { .. }));
    }

    #[test]
    fn new_should_reject_slot_array_past_page_end() {
        let mut page = empty(SMALL);
        put_u16(&mut page.data, SLOT_COUNT_OFFSET, 100);

        let err = SlottedPage::new(page.data).unwrap_err();

        assert!(matches!(err, SlottedPageError::Corrupted { .. }));
    }

    #[test]
    fn new_should_reject_slot_pointing_outside_page() {
        let mut page = empty(SMALL);
        page.insert(b"abc").unwrap();
        page.set_slot(0, SMALL - 2, 3);

        let err = SlottedPage::new(page.data).unwrap_err();

        assert!(matches!(err, SlottedPageError::Corrupted { .. }));
    }

    #[test]
    fn new_should_reject_slot_pointing_into_header() {
        let mut page = empty(SMALL);
        page.insert(b"abc").unwrap();
        page.set_slot(0, 4, 3);

        let err = SlottedPage::new(page.data).unwrap_err();

        assert!(matches!(err, SlottedPageError::Corrupted { .. }));
    }

    #[test]
    fn new_should_accept_any_next_page_id() {
        let mut page = empty(SMALL);
        page.data[NEXT_PAGE_ID_OFFSET..HEADER_SIZE].fill(0xFF);

        assert!(SlottedPage::new(page.data).is_ok());
    }

    #[test]
    fn new_should_accept_read_only_slice() {
        let mut page = empty(SMALL);
        page.insert(b"hello").unwrap();
        let bytes = snapshot(&page);

        let read_only = SlottedPage::new(bytes.as_slice()).unwrap();

        assert_eq!(read_only.get(0).unwrap(), b"hello");
        assert_eq!(read_only.iter().count(), 1);
    }

    #[test]
    fn mutable_slice_should_write_through_to_buffer() {
        let mut bytes = vec![0; SMALL];

        {
            let mut page = SlottedPage::init(bytes.as_mut_slice()).unwrap();
            page.insert(b"xyz").unwrap();
        }

        assert_eq!(&bytes[SMALL - 3..], b"xyz");
        assert_eq!(
            SlottedPage::new(&bytes[..]).unwrap().get(0).unwrap(),
            b"xyz"
        );
    }

    // --- insert / get ------------------------------------------------------

    #[test]
    fn insert_then_get_should_return_same_bytes() {
        let mut page = empty(PAGE_SIZE);

        let slot_id = page.insert(b"hello world").unwrap();

        assert_eq!(page.get(slot_id).unwrap(), b"hello world");
    }

    #[test]
    fn insert_should_return_sequential_slot_ids() {
        let mut page = empty(PAGE_SIZE);

        let ids: Vec<u16> = (0..5).map(|i| page.insert(&[i]).unwrap()).collect();

        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
        assert_eq!(page.slot_count(), 5);
    }

    #[test]
    fn insert_should_place_tuples_from_end_of_page() {
        let mut page = empty(PAGE_SIZE);

        page.insert(&tuple(10, 1)).unwrap();
        page.insert(&tuple(20, 2)).unwrap();

        assert_eq!(page.slot(0), ((PAGE_SIZE - 10) as u16, 10));
        assert_eq!(page.slot(1), ((PAGE_SIZE - 30) as u16, 20));
        assert_eq!(page.free_space_end(), PAGE_SIZE - 30);
    }

    #[test]
    fn insert_should_update_free_space() {
        let mut page = empty(PAGE_SIZE);
        let before = page.contiguous_free_space();

        page.insert(&tuple(100, 1)).unwrap();

        assert_eq!(page.contiguous_free_space(), before - 100 - SLOT_SIZE);
        assert_eq!(page.total_free_space(), before - 100 - SLOT_SIZE);
    }

    #[test]
    fn insert_empty_tuple_should_work() {
        let mut page = empty(SMALL);

        let empty_slot = page.insert(b"").unwrap();
        let next_slot = page.insert(b"x").unwrap();

        assert_eq!(page.get(empty_slot).unwrap(), b"");
        assert_eq!(
            next_slot, 1,
            "empty tuple must not be treated as a tombstone"
        );
        assert_eq!(page.iter().count(), 2);
        assert_invariants(&page);
    }

    #[test]
    fn insert_max_size_tuple_should_fit() {
        let mut page = empty(PAGE_SIZE);
        assert_eq!(page.max_tuple_size(), 8164);
        let big = tuple(8164, 9);

        let slot_id = page.insert(&big).unwrap();

        assert_eq!(page.get(slot_id).unwrap(), big.as_slice());
        assert_eq!(page.total_free_space(), 0);
        assert_invariants(&page);
    }

    #[test]
    fn insert_larger_than_max_should_fail_with_tuple_too_large() {
        let mut page = empty(PAGE_SIZE);
        let before = snapshot(&page);

        let err = page.insert(&tuple(8165, 1)).unwrap_err();

        assert_eq!(
            err,
            SlottedPageError::TupleTooLarge {
                size: 8165,
                max: 8164
            }
        );
        assert_eq!(snapshot(&page), before);
    }

    #[test]
    fn fill_page_should_hold_exact_number_of_tuples() {
        let mut page = empty(SMALL);

        for i in 0..4 {
            assert_eq!(page.insert(&tuple(6, i)).unwrap(), i as u16);
        }

        assert_eq!(page.insert(&tuple(6, 9)), Err(SlottedPageError::PageFull));
        assert_eq!(page.total_free_space(), 0);
    }

    #[test]
    fn insert_into_full_page_should_fail_without_mutation() {
        let mut page = empty(SMALL);
        for i in 0..4 {
            page.insert(&tuple(6, i)).unwrap();
        }
        let before = snapshot(&page);

        assert_eq!(page.insert(b"x"), Err(SlottedPageError::PageFull));

        assert_eq!(snapshot(&page), before);
    }

    #[test]
    fn insert_should_count_new_slot_cost() {
        let mut page = empty(SMALL);
        page.insert(&tuple(30, 1)).unwrap();
        assert_eq!(page.total_free_space(), 6);

        // 3 bytes of data fit in 6 free bytes, but the new slot needs 4 more.
        assert!(!page.can_insert(3));
        assert_eq!(page.insert(&tuple(3, 2)), Err(SlottedPageError::PageFull));
        assert!(page.can_insert(2));
        assert!(page.insert(&tuple(2, 2)).is_ok());
    }

    #[test]
    fn get_invalid_slot_should_fail() {
        let mut page = empty(SMALL);
        page.insert(b"a").unwrap();

        assert_eq!(
            page.get(1),
            Err(SlottedPageError::InvalidSlot { slot_id: 1 })
        );
        assert_eq!(
            page.get(u16::MAX),
            Err(SlottedPageError::InvalidSlot { slot_id: u16::MAX })
        );
    }

    #[test]
    fn many_small_tuples_should_all_be_readable() {
        let mut page = empty(PAGE_SIZE);
        let mut expected = Vec::new();

        for i in 0.. {
            let data = tuple(i % 50, i as u8);
            match page.insert(&data) {
                Ok(slot_id) => expected.push((slot_id, data)),
                Err(SlottedPageError::PageFull) => break,
                Err(err) => panic!("unexpected error {err}"),
            }
        }

        assert!(expected.len() > 250);
        for (slot_id, data) in &expected {
            assert_eq!(page.get(*slot_id).unwrap(), data.as_slice());
        }
        assert_invariants(&page);
    }

    // --- delete / reuse / iter ---------------------------------------------

    #[test]
    fn delete_should_make_get_fail_with_slot_deleted() {
        let mut page = empty(SMALL);
        page.insert(b"a").unwrap();

        page.delete(0).unwrap();

        assert_eq!(
            page.get(0),
            Err(SlottedPageError::SlotDeleted { slot_id: 0 })
        );
    }

    #[test]
    fn delete_should_not_change_other_slots() {
        let mut page = empty(PAGE_SIZE);
        page.insert(b"first").unwrap();
        page.insert(b"second").unwrap();
        page.insert(b"third").unwrap();

        page.delete(1).unwrap();

        assert_eq!(page.get(0).unwrap(), b"first");
        assert_eq!(page.get(2).unwrap(), b"third");
    }

    #[test]
    fn delete_should_not_shrink_slot_count() {
        let mut page = empty(SMALL);
        page.insert(b"a").unwrap();
        page.insert(b"b").unwrap();

        page.delete(1).unwrap();

        assert_eq!(page.slot_count(), 2);
    }

    #[test]
    fn delete_should_not_move_tuple_bytes() {
        let mut page = empty(PAGE_SIZE);
        page.insert(b"first").unwrap();
        page.insert(b"second").unwrap();
        let tuple_area = page.free_space_end();
        let before = snapshot(&page);
        let contiguous = page.contiguous_free_space();

        page.delete(0).unwrap();

        assert_eq!(&page.data[tuple_area..], &before[tuple_area..]);
        assert_eq!(page.free_space_end(), tuple_area);
        assert_eq!(page.contiguous_free_space(), contiguous);
    }

    #[test]
    fn delete_should_increase_total_free_space() {
        let mut page = empty(PAGE_SIZE);
        page.insert(&tuple(40, 1)).unwrap();
        let before = page.total_free_space();

        page.delete(0).unwrap();

        assert_eq!(page.total_free_space(), before + 40);
    }

    #[test]
    fn delete_twice_should_fail() {
        let mut page = empty(SMALL);
        page.insert(b"a").unwrap();
        page.delete(0).unwrap();

        assert_eq!(
            page.delete(0),
            Err(SlottedPageError::SlotDeleted { slot_id: 0 })
        );
    }

    #[test]
    fn delete_invalid_slot_should_fail() {
        let mut page = empty(SMALL);

        assert_eq!(
            page.delete(0),
            Err(SlottedPageError::InvalidSlot { slot_id: 0 })
        );
    }

    #[test]
    fn insert_after_delete_should_reuse_lowest_free_slot() {
        let mut page = empty(PAGE_SIZE);
        for i in 0..5 {
            page.insert(&[i]).unwrap();
        }
        page.delete(3).unwrap();
        page.delete(1).unwrap();

        assert_eq!(page.insert(b"new").unwrap(), 1);
        assert_eq!(page.insert(b"newer").unwrap(), 3);
        assert_eq!(page.insert(b"newest").unwrap(), 5);
        assert_eq!(page.get(1).unwrap(), b"new");
        assert_eq!(page.get(3).unwrap(), b"newer");
    }

    #[test]
    fn reused_slot_should_not_cost_extra_slot_bytes() {
        let mut page = empty(SMALL);
        page.insert(&tuple(10, 1)).unwrap();
        page.insert(&tuple(10, 2)).unwrap();
        page.delete(0).unwrap();
        assert_eq!(page.total_free_space(), 22);

        // A new slot would need 22 + 4 bytes; reusing slot 0 needs only 22.
        assert!(page.can_insert(22));
        assert_eq!(page.insert(&tuple(22, 3)).unwrap(), 0);
        assert_eq!(page.get(1).unwrap(), tuple(10, 2).as_slice());
        assert_eq!(page.total_free_space(), 0);
        assert_invariants(&page);
    }

    #[test]
    fn iter_should_skip_deleted_slots() {
        let mut page = empty(PAGE_SIZE);
        page.insert(b"a").unwrap();
        page.insert(b"b").unwrap();
        page.insert(b"c").unwrap();
        page.delete(1).unwrap();

        let tuples: Vec<(u16, &[u8])> = page.iter().collect();

        assert_eq!(tuples, vec![(0, &b"a"[..]), (2, &b"c"[..])]);
    }

    #[test]
    fn iter_on_empty_page_should_be_empty() {
        let page = empty(PAGE_SIZE);

        assert_eq!(page.iter().count(), 0);
    }

    #[test]
    fn delete_empty_tuple_should_become_tombstone() {
        let mut page = empty(SMALL);
        page.insert(b"").unwrap();

        page.delete(0).unwrap();

        assert_eq!(
            page.get(0),
            Err(SlottedPageError::SlotDeleted { slot_id: 0 })
        );
        assert_eq!(page.slot(0).0, TOMBSTONE);
    }

    // --- compact -----------------------------------------------------------

    #[test]
    fn compact_should_preserve_slot_ids_and_contents() {
        let mut page = empty(PAGE_SIZE);
        for i in 0..10u8 {
            page.insert(&tuple(10 + i as usize, i)).unwrap();
        }
        for slot_id in [1, 4, 5, 8] {
            page.delete(slot_id).unwrap();
        }

        page.compact();

        for i in [0u8, 2, 3, 6, 7, 9] {
            assert_eq!(
                page.get(i as u16).unwrap(),
                tuple(10 + i as usize, i).as_slice()
            );
        }
        assert_invariants(&page);
    }

    #[test]
    fn compact_should_make_free_space_contiguous() {
        let mut page = empty(PAGE_SIZE);
        for i in 0..6 {
            page.insert(&tuple(30, i)).unwrap();
        }
        page.delete(0).unwrap();
        page.delete(3).unwrap();
        assert!(page.contiguous_free_space() < page.total_free_space());

        page.compact();

        assert_eq!(page.contiguous_free_space(), page.total_free_space());
    }

    #[test]
    fn compact_should_keep_tombstones() {
        let mut page = empty(SMALL);
        page.insert(b"a").unwrap();
        page.insert(b"b").unwrap();
        page.delete(0).unwrap();

        page.compact();

        assert_eq!(page.slot_count(), 2);
        assert_eq!(
            page.get(0),
            Err(SlottedPageError::SlotDeleted { slot_id: 0 })
        );
        assert_eq!(page.get(1).unwrap(), b"b");
    }

    #[test]
    fn compact_on_page_without_holes_should_be_noop() {
        let mut page = empty(PAGE_SIZE);
        page.insert(b"one").unwrap();
        page.insert(b"").unwrap();
        page.insert(b"three").unwrap();
        let before = snapshot(&page);

        page.compact();

        assert_eq!(snapshot(&page), before);
    }

    #[test]
    fn compact_empty_page_should_be_noop() {
        let mut page = empty(SMALL);
        let before = snapshot(&page);

        page.compact();

        assert_eq!(snapshot(&page), before);
    }

    #[test]
    fn compact_after_deleting_everything_should_free_whole_tuple_area() {
        let mut page = empty(SMALL);
        page.insert(&tuple(10, 1)).unwrap();
        page.insert(&tuple(10, 2)).unwrap();
        page.delete(0).unwrap();
        page.delete(1).unwrap();

        page.compact();

        assert_eq!(page.free_space_end(), SMALL);
        assert_eq!(
            page.contiguous_free_space(),
            SMALL - HEADER_SIZE - 2 * SLOT_SIZE
        );
    }

    #[test]
    fn insert_should_compact_when_only_total_space_fits() {
        let mut page = empty(SMALL);
        for i in 0..4 {
            page.insert(&tuple(6, i)).unwrap();
        }
        page.delete(1).unwrap();
        assert_eq!(page.contiguous_free_space(), 0);

        assert_eq!(page.insert(&tuple(6, 9)).unwrap(), 1);

        assert_eq!(page.get(0).unwrap(), tuple(6, 0).as_slice());
        assert_eq!(page.get(1).unwrap(), tuple(6, 9).as_slice());
        assert_eq!(page.get(2).unwrap(), tuple(6, 2).as_slice());
        assert_eq!(page.get(3).unwrap(), tuple(6, 3).as_slice());
        assert_invariants(&page);
    }

    #[test]
    fn insert_should_not_compact_when_contiguous_space_fits() {
        let mut page = empty(PAGE_SIZE);
        page.insert(b"aaaa").unwrap();
        page.insert(b"bbbb").unwrap();
        page.insert(b"cccc").unwrap();
        page.delete(1).unwrap();
        let offset_a = page.slot(0).0;
        let offset_c = page.slot(2).0;
        let free_end = page.free_space_end();

        assert_eq!(page.insert(b"dd").unwrap(), 1);

        assert_eq!(page.slot(0).0, offset_a);
        assert_eq!(page.slot(2).0, offset_c);
        assert_eq!(page.slot(1), ((free_end - 2) as u16, 2));
    }

    // --- update ------------------------------------------------------------

    #[test]
    fn update_same_size_should_overwrite_in_place() {
        let mut page = empty(PAGE_SIZE);
        page.insert(b"abcd").unwrap();
        let offset = page.slot(0).0;

        page.update(0, b"wxyz").unwrap();

        assert_eq!(page.get(0).unwrap(), b"wxyz");
        assert_eq!(page.slot(0).0, offset);
    }

    #[test]
    fn update_smaller_should_keep_offset_and_shrink_len() {
        let mut page = empty(PAGE_SIZE);
        page.insert(b"abcdef").unwrap();
        let offset = page.slot(0).0;
        let total_before = page.total_free_space();
        let contiguous_before = page.contiguous_free_space();

        page.update(0, b"xy").unwrap();

        assert_eq!(page.get(0).unwrap(), b"xy");
        assert_eq!(page.slot(0), (offset, 2));
        assert_eq!(page.total_free_space(), total_before + 4);
        assert_eq!(page.contiguous_free_space(), contiguous_before);
        assert_invariants(&page);
    }

    #[test]
    fn update_larger_should_keep_slot_id() {
        let mut page = empty(PAGE_SIZE);
        page.insert(b"a").unwrap();
        page.insert(b"b").unwrap();

        page.update(0, b"a much longer tuple").unwrap();

        assert_eq!(page.get(0).unwrap(), b"a much longer tuple");
        assert_eq!(page.get(1).unwrap(), b"b");
        assert_eq!(page.slot_count(), 2);
        assert_invariants(&page);
    }

    #[test]
    fn update_larger_should_compact_if_needed() {
        let mut page = empty(SMALL);
        page.insert(&tuple(6, 1)).unwrap();
        page.insert(&tuple(6, 2)).unwrap();
        page.insert(&tuple(6, 3)).unwrap();
        page.delete(0).unwrap();
        assert_eq!(page.contiguous_free_space(), 10);
        assert_eq!(page.total_free_space(), 16);

        page.update(2, &tuple(14, 4)).unwrap();

        assert_eq!(page.get(1).unwrap(), tuple(6, 2).as_slice());
        assert_eq!(page.get(2).unwrap(), tuple(14, 4).as_slice());
        assert_invariants(&page);
    }

    #[test]
    fn update_larger_counting_old_space_should_fit() {
        let mut page = empty(SMALL);
        page.insert(&tuple(10, 1)).unwrap();
        page.insert(&tuple(10, 2)).unwrap();
        assert_eq!(page.total_free_space(), 12);

        // 20 bytes only fit if the 10 bytes of the old version are reused.
        page.update(1, &tuple(20, 3)).unwrap();

        assert_eq!(page.get(0).unwrap(), tuple(10, 1).as_slice());
        assert_eq!(page.get(1).unwrap(), tuple(20, 3).as_slice());
        assert_eq!(page.total_free_space(), 2);
        assert_invariants(&page);
    }

    #[test]
    fn update_that_does_not_fit_should_fail_without_mutation() {
        let mut page = empty(SMALL);
        page.insert(&tuple(10, 1)).unwrap();
        page.insert(&tuple(10, 2)).unwrap();
        let before = snapshot(&page);

        assert_eq!(
            page.update(1, &tuple(23, 3)),
            Err(SlottedPageError::PageFull)
        );

        assert_eq!(snapshot(&page), before);
    }

    #[test]
    fn failed_update_should_not_compact() {
        let mut page = empty(SMALL);
        page.insert(&tuple(6, 1)).unwrap();
        page.insert(&tuple(6, 2)).unwrap();
        page.insert(&tuple(6, 3)).unwrap();
        page.delete(0).unwrap();
        let before = snapshot(&page);

        // 16 free + 6 old = 22 available, 23 requested.
        assert_eq!(
            page.update(2, &tuple(23, 4)),
            Err(SlottedPageError::PageFull)
        );

        assert_eq!(snapshot(&page), before);
    }

    #[test]
    fn update_too_large_should_fail_with_tuple_too_large() {
        let mut page = empty(SMALL);
        page.insert(b"a").unwrap();
        let before = snapshot(&page);

        let err = page.update(0, &tuple(SMALL, 1)).unwrap_err();

        assert!(matches!(err, SlottedPageError::TupleTooLarge { .. }));
        assert_eq!(snapshot(&page), before);
    }

    #[test]
    fn update_deleted_slot_should_fail() {
        let mut page = empty(SMALL);
        page.insert(b"a").unwrap();
        page.delete(0).unwrap();

        assert_eq!(
            page.update(0, b"b"),
            Err(SlottedPageError::SlotDeleted { slot_id: 0 })
        );
    }

    #[test]
    fn update_invalid_slot_should_fail() {
        let mut page = empty(SMALL);

        assert_eq!(
            page.update(0, b"b"),
            Err(SlottedPageError::InvalidSlot { slot_id: 0 })
        );
    }

    #[test]
    fn update_to_empty_tuple_should_work() {
        let mut page = empty(SMALL);
        page.insert(b"abc").unwrap();

        page.update(0, b"").unwrap();

        assert_eq!(page.get(0).unwrap(), b"");
        assert_eq!(page.iter().count(), 1);
        assert_invariants(&page);
    }

    // --- round trip / properties -------------------------------------------

    #[test]
    fn page_bytes_should_round_trip_through_new() {
        let mut page = empty(PAGE_SIZE);
        page.insert(b"one").unwrap();
        page.insert(b"two").unwrap();
        page.insert(b"three").unwrap();
        page.delete(1).unwrap();

        let reopened = SlottedPage::new(snapshot(&page)).unwrap();

        assert_eq!(reopened.slot_count(), 3);
        assert_eq!(reopened.get(0).unwrap(), b"one");
        assert_eq!(
            reopened.get(1),
            Err(SlottedPageError::SlotDeleted { slot_id: 1 })
        );
        assert_eq!(reopened.get(2).unwrap(), b"three");
        assert_eq!(reopened.total_free_space(), page.total_free_space());
    }

    #[test]
    fn can_insert_should_match_insert_result() {
        let mut with_hole = empty(SMALL);
        with_hole.insert(&tuple(6, 1)).unwrap();
        with_hole.insert(&tuple(6, 2)).unwrap();
        with_hole.delete(0).unwrap();

        let mut without_hole = empty(SMALL);
        without_hole.insert(&tuple(12, 1)).unwrap();

        for base in [empty(SMALL), with_hole, without_hole] {
            for len in 0..=SMALL {
                let mut page = SlottedPage::new(snapshot(&base)).unwrap();

                assert_eq!(
                    page.can_insert(len),
                    page.insert(&tuple(len, 7)).is_ok(),
                    "len {len}"
                );
            }
        }
    }

    #[test]
    fn max_tuple_size_for_should_match_an_empty_page() {
        for size in [HEADER_SIZE, HEADER_SIZE + SLOT_SIZE, SMALL, PAGE_SIZE] {
            assert_eq!(
                max_tuple_size_for(size),
                empty(size).max_tuple_size(),
                "page of {size} bytes"
            );
        }
        assert_eq!(max_tuple_size_for(PAGE_SIZE), 8164);
        assert_eq!(max_tuple_size_for(0), 0);
    }

    #[test]
    fn largest_insertable_of_empty_page_should_be_max_tuple_size() {
        let page = empty(SMALL);

        assert_eq!(page.largest_insertable(), Some(page.max_tuple_size()));
        assert_eq!(page.largest_insertable(), Some(36));
    }

    #[test]
    fn largest_insertable_should_discount_tuple_and_new_slot() {
        let mut page = empty(SMALL);

        page.insert(&tuple(6, 1)).unwrap();

        assert_eq!(
            page.largest_insertable(),
            Some(SMALL - HEADER_SIZE - 2 * SLOT_SIZE - 6)
        );
    }

    #[test]
    fn largest_insertable_should_not_charge_a_slot_when_one_can_be_reused() {
        let mut page = empty(SMALL);
        page.insert(&tuple(6, 1)).unwrap();
        page.insert(&tuple(6, 2)).unwrap();

        page.delete(0).unwrap();

        assert_eq!(
            page.largest_insertable(),
            Some(SMALL - HEADER_SIZE - 2 * SLOT_SIZE - 6)
        );
    }

    #[test]
    fn largest_insertable_should_count_holes_left_by_deletes() {
        let mut page = empty(SMALL);
        page.insert(&tuple(10, 1)).unwrap();
        page.insert(&tuple(10, 2)).unwrap();
        let before = page.largest_insertable().unwrap();

        page.delete(1).unwrap();

        assert_eq!(page.largest_insertable(), Some(before + 10 + SLOT_SIZE));
    }

    #[test]
    fn largest_insertable_of_page_with_room_only_for_a_slot_should_be_zero() {
        let mut page = empty(SMALL);

        // Leaves exactly the 4 bytes of a new slot: only an empty tuple still fits.
        page.insert(&tuple(page.max_tuple_size() - SLOT_SIZE, 1))
            .unwrap();

        assert_eq!(page.largest_insertable(), Some(0));
        assert!(page.can_insert(0));
    }

    #[test]
    fn largest_insertable_of_full_page_should_be_none() {
        let mut page = empty(SMALL);

        page.insert(&tuple(page.max_tuple_size(), 1)).unwrap();

        assert_eq!(page.largest_insertable(), None);
        assert!(!page.can_insert(0));
    }

    #[test]
    fn largest_insertable_of_header_sized_page_should_be_none() {
        let page = empty(HEADER_SIZE);

        assert_eq!(page.largest_insertable(), None);
    }

    #[test]
    fn largest_insertable_should_agree_with_can_insert_on_random_pages() {
        let mut rng = Rng(0xA076_1D64_78BD_642F);
        let mut page = empty(SMALL * 4);

        for step in 0..5_000 {
            let live: Vec<u16> = page.iter().map(|(slot_id, _)| slot_id).collect();
            if !live.is_empty() && rng.below(3) == 0 {
                page.delete(live[rng.below(live.len())]).unwrap();
            } else {
                let len = rng.below(40);
                let _ = page.insert(&rng.bytes(len));
            }

            match page.largest_insertable() {
                Some(largest) => {
                    assert!(page.can_insert(largest), "step {step}");
                    assert!(!page.can_insert(largest + 1), "step {step}");
                }
                None => assert!(!page.can_insert(0), "step {step}"),
            }
        }
    }

    /// Deterministic xorshift so failures are reproducible.
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

        fn bytes(&mut self, len: usize) -> Vec<u8> {
            (0..len).map(|_| self.next() as u8).collect()
        }
    }

    #[test]
    fn random_operations_should_match_model() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut page = empty(PAGE_SIZE);
        let mut model: HashMap<u16, Vec<u8>> = HashMap::new();

        for step in 0..10_000 {
            let len = if rng.below(10) == 0 {
                rng.below(2000)
            } else {
                rng.below(200)
            };
            let data = rng.bytes(len);
            let live: Vec<u16> = model.keys().copied().collect();

            match rng.below(3) {
                0 => {
                    let expected_ok = page.can_insert(len);
                    match page.insert(&data) {
                        Ok(slot_id) => {
                            assert!(expected_ok, "step {step}");
                            assert!(model.insert(slot_id, data).is_none(), "step {step}");
                        }
                        Err(SlottedPageError::PageFull) => assert!(!expected_ok, "step {step}"),
                        Err(err) => panic!("step {step}: {err}"),
                    }
                }
                1 if !live.is_empty() => {
                    let slot_id = live[rng.below(live.len())];
                    page.delete(slot_id).unwrap();
                    model.remove(&slot_id);
                }
                2 if !live.is_empty() => {
                    let slot_id = live[rng.below(live.len())];
                    let before = snapshot(&page);
                    match page.update(slot_id, &data) {
                        Ok(()) => {
                            model.insert(slot_id, data);
                        }
                        Err(SlottedPageError::PageFull) => {
                            assert_eq!(snapshot(&page), before, "step {step}");
                        }
                        Err(err) => panic!("step {step}: {err}"),
                    }
                }
                _ => {}
            }

            assert_invariants(&page);
            if step % 100 == 0 {
                let actual: HashMap<u16, Vec<u8>> =
                    page.iter().map(|(id, t)| (id, t.to_vec())).collect();
                assert_eq!(actual, model, "step {step}");
            }
        }

        let actual: HashMap<u16, Vec<u8>> = page.iter().map(|(id, t)| (id, t.to_vec())).collect();
        assert_eq!(actual, model);
    }
}
