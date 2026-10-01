use std::fmt;

use crate::serialization::{
    ByteReader, ByteWriter, Decode, Encode, SerializationError, get_u64, put_u64,
};

/// Header shared by every page type (little-endian):
/// `[0..8)` LSN, `[8..12)` checksum, `[12]` page type, `[13]` format version,
/// `[14..16)` reserved (zeroed).
pub const LSN_OFFSET: usize = 0;
pub const LSN_SIZE: usize = 8;
pub const CHECKSUM_OFFSET: usize = LSN_OFFSET + LSN_SIZE;
pub const CHECKSUM_SIZE: usize = 4;
pub const PAGE_TYPE_OFFSET: usize = CHECKSUM_OFFSET + CHECKSUM_SIZE;
pub const VERSION_OFFSET: usize = PAGE_TYPE_OFFSET + 1;
const HEADER_RESERVED_OFFSET: usize = VERSION_OFFSET + 1;
const HEADER_RESERVED_SIZE: usize = 2;
pub const COMMON_HEADER_SIZE: usize = HEADER_RESERVED_OFFSET + HEADER_RESERVED_SIZE;

pub const PAGE_FORMAT_VERSION: u8 = 1;

/// Page 0 is reserved for the database header (the future catalog root). It is never
/// allocated, so a page link of 0 means "no page".
pub const RESERVED_PAGE_ID: u32 = 0;

#[derive(Debug, PartialEq, Eq)]
pub enum PageError {
    InvalidPageSize,
    InvalidPageData { expected: usize, actual: usize },
    UnsupportedVersion { found: u8, expected: u8 },
    Serialization(SerializationError),
}

impl From<SerializationError> for PageError {
    fn from(err: SerializationError) -> Self {
        PageError::Serialization(err)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    /// Never formatted: an all-zero page read past the end of the file.
    Free = 0,
    Slotted = 1,
}

impl TryFrom<u8> for PageType {
    type Error = SerializationError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(PageType::Free),
            1 => Ok(PageType::Slotted),
            _ => Err(SerializationError::InvalidValue {
                field: "page_type",
                value: value.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageHeader {
    pub lsn: u64,
    pub checksum: u32,
    pub page_type: PageType,
    pub version: u8,
}

impl PageHeader {
    /// Decodes the header and rejects formatted pages from another format version.
    pub fn read(page: &[u8]) -> Result<Self, PageError> {
        let header = Self::decode(&mut ByteReader::new(page))?;
        if header.page_type != PageType::Free && header.version != PAGE_FORMAT_VERSION {
            return Err(PageError::UnsupportedVersion {
                found: header.version,
                expected: PAGE_FORMAT_VERSION,
            });
        }
        Ok(header)
    }

    /// Stamps `page_type` and the current format version, keeping LSN and checksum.
    pub fn format(page: &mut [u8], page_type: PageType) -> Result<(), PageError> {
        let mut kept = ByteReader::new(page);
        let header = Self {
            lsn: kept.read_u64()?,
            checksum: kept.read_u32()?,
            page_type,
            version: PAGE_FORMAT_VERSION,
        };
        header.encode(&mut ByteWriter::new(page))?;
        Ok(())
    }
}

impl Encode for PageHeader {
    const ENCODED_SIZE: usize = COMMON_HEADER_SIZE;

    fn encode(&self, writer: &mut ByteWriter<'_>) -> Result<(), SerializationError> {
        let mut bytes = [0; Self::ENCODED_SIZE];
        let mut fields = ByteWriter::new(&mut bytes);
        fields.write_u64(self.lsn)?;
        fields.write_u32(self.checksum)?;
        fields.write_u8(self.page_type as u8)?;
        fields.write_u8(self.version)?;
        fields.write_zeros(HEADER_RESERVED_SIZE)?;
        writer.write_bytes(&bytes)
    }
}

impl Decode for PageHeader {
    fn decode(reader: &mut ByteReader<'_>) -> Result<Self, SerializationError> {
        let bytes: [u8; Self::ENCODED_SIZE] = reader.read_array()?;
        let mut fields = ByteReader::new(&bytes);
        Ok(Self {
            lsn: fields.read_u64()?,
            checksum: fields.read_u32()?,
            page_type: PageType::try_from(fields.read_u8()?)?,
            version: fields.read_u8()?,
        })
    }
}

impl fmt::Display for PageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PageError::InvalidPageSize => {
                write!(f, "page size must be at least {COMMON_HEADER_SIZE} bytes")
            }
            PageError::InvalidPageData { expected, actual } => {
                write!(
                    f,
                    "page data length {actual} does not match page size {expected}"
                )
            }
            PageError::UnsupportedVersion { found, expected } => {
                write!(
                    f,
                    "page format version {found} is not supported (expected {expected})"
                )
            }
            PageError::Serialization(err) => write!(f, "invalid page header: {err}"),
        }
    }
}

impl std::error::Error for PageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PageError::Serialization(err) => Some(err),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct Page {
    page_id: Option<u32>,
    data: Vec<u8>,
    pin_count: u32,
    is_dirty: bool,
}

impl Page {
    pub fn new(page_size: usize) -> Result<Self, PageError> {
        if page_size < COMMON_HEADER_SIZE {
            return Err(PageError::InvalidPageSize);
        }

        Ok(Self {
            page_id: None,
            data: vec![0; page_size],
            pin_count: 0,
            is_dirty: false,
        })
    }

    pub fn load(&mut self, page_id: u32, data: Vec<u8>) -> Result<(), PageError> {
        if data.len() != self.data.len() {
            return Err(PageError::InvalidPageData {
                expected: self.data.len(),
                actual: data.len(),
            });
        }

        self.page_id = Some(page_id);
        self.data = data;
        self.pin_count = 1;
        self.is_dirty = false;
        Ok(())
    }

    pub fn reset_metadata(&mut self) {
        self.page_id = None;
        self.pin_count = 0;
        self.is_dirty = false;
    }

    pub fn pin(&mut self) {
        self.pin_count += 1;
    }

    pub fn try_unpin(&mut self, is_dirty: bool) -> bool {
        if self.pin_count == 0 {
            return false;
        }

        self.pin_count -= 1;

        if is_dirty {
            self.is_dirty = true;
        }

        true
    }

    pub fn page_id(&self) -> Option<u32> {
        self.page_id
    }

    pub fn pin_count(&self) -> u32 {
        self.pin_count
    }

    pub fn is_dirty(&self) -> bool {
        self.is_dirty
    }

    pub fn mark_clean(&mut self) {
        self.is_dirty = false;
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.is_dirty = true;
    }

    pub fn lsn(&self) -> u64 {
        get_u64(&self.data, LSN_OFFSET)
    }

    pub fn set_lsn(&mut self, lsn: u64) {
        put_u64(&mut self.data, LSN_OFFSET, lsn);
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PAGE_SIZE: usize = 16;

    fn bytes(fill: u8) -> Vec<u8> {
        vec![fill; TEST_PAGE_SIZE]
    }

    #[test]
    fn page_new_should_initialize_correctly() {
        let page = Page::new(TEST_PAGE_SIZE).unwrap();

        assert_eq!(page.page_id(), None);
        assert_eq!(page.pin_count(), 0);
        assert!(!page.is_dirty());
        assert_eq!(page.data().len(), TEST_PAGE_SIZE);
        assert!(page.data().iter().all(|&b| b == 0));
    }

    #[test]
    fn page_new_should_reject_zero_size() {
        let err = Page::new(0).unwrap_err();

        assert!(matches!(err, PageError::InvalidPageSize));
    }

    #[test]
    fn page_new_should_reject_size_smaller_than_common_header() {
        let err = Page::new(COMMON_HEADER_SIZE - 1).unwrap_err();

        assert!(matches!(err, PageError::InvalidPageSize));
        assert!(Page::new(COMMON_HEADER_SIZE).is_ok());
    }

    #[test]
    fn reset_metadata_should_clear_fields_and_preserve_data() {
        let mut page = Page::new(TEST_PAGE_SIZE).unwrap();
        page.load(10, bytes(1)).unwrap();
        page.pin();
        page.pin();
        assert!(page.try_unpin(true));

        page.reset_metadata();

        assert_eq!(page.page_id(), None);
        assert_eq!(page.pin_count(), 0);
        assert!(!page.is_dirty());
        assert_eq!(page.data(), bytes(1).as_slice());
    }

    #[test]
    fn load_should_reject_wrong_size_without_mutating() {
        let mut page = Page::new(TEST_PAGE_SIZE).unwrap();
        page.load(1, bytes(9)).unwrap();
        page.pin();
        assert!(page.try_unpin(true));

        let err = page.load(2, vec![1, 2, 3]).unwrap_err();

        assert!(matches!(
            err,
            PageError::InvalidPageData {
                expected: TEST_PAGE_SIZE,
                actual: 3
            }
        ));
        assert_eq!(page.page_id(), Some(1));
        assert_eq!(page.pin_count(), 1);
        assert!(page.is_dirty());
        assert_eq!(page.data(), bytes(9).as_slice());
    }

    #[test]
    fn load_should_replace_bytes_and_reset_pin_and_dirty() {
        let mut page = Page::new(TEST_PAGE_SIZE).unwrap();
        page.data_mut()[0] = 7;
        page.pin();
        assert!(page.try_unpin(true));

        page.load(5, bytes(4)).unwrap();

        assert_eq!(page.page_id(), Some(5));
        assert_eq!(page.pin_count(), 1);
        assert!(!page.is_dirty());
        assert_eq!(page.data(), bytes(4).as_slice());
    }

    #[test]
    fn pin_and_unpin_should_track_count_and_sticky_dirty() {
        let mut page = Page::new(TEST_PAGE_SIZE).unwrap();

        page.pin();
        page.pin();
        assert_eq!(page.pin_count(), 2);

        assert!(page.try_unpin(true));
        assert_eq!(page.pin_count(), 1);
        assert!(page.is_dirty());

        assert!(page.try_unpin(false));
        assert_eq!(page.pin_count(), 0);
        assert!(page.is_dirty());

        assert!(!page.try_unpin(false));
        assert_eq!(page.pin_count(), 0);
        assert!(page.is_dirty());
    }

    #[test]
    fn set_lsn_should_write_little_endian_at_offset_0() {
        let mut page = Page::new(TEST_PAGE_SIZE).unwrap();

        page.set_lsn(0x0807_0605_0403_0201);

        assert_eq!(&page.data()[0..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn lsn_should_read_back_value() {
        let mut page = Page::new(TEST_PAGE_SIZE).unwrap();
        assert_eq!(page.lsn(), 0);

        for lsn in [1, 42, u64::MAX] {
            page.set_lsn(lsn);
            assert_eq!(page.lsn(), lsn);
        }
    }

    #[test]
    fn set_lsn_should_not_touch_other_bytes() {
        let mut page = Page::new(TEST_PAGE_SIZE).unwrap();
        page.data_mut().fill(0xEE);

        page.set_lsn(0);

        assert!(page.data()[LSN_SIZE..].iter().all(|&b| b == 0xEE));
    }

    fn header_bytes(page_type: u8, version: u8) -> Vec<u8> {
        let mut bytes = vec![0; 32];
        put_u64(&mut bytes, LSN_OFFSET, 0x0102_0304_0506_0708);
        bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_SIZE].copy_from_slice(&[9, 10, 11, 12]);
        bytes[PAGE_TYPE_OFFSET] = page_type;
        bytes[VERSION_OFFSET] = version;
        bytes
    }

    #[test]
    fn page_type_should_round_trip_through_u8() {
        for page_type in [PageType::Free, PageType::Slotted] {
            assert_eq!(PageType::try_from(page_type as u8).unwrap(), page_type);
        }
    }

    #[test]
    fn page_type_from_unknown_byte_should_fail() {
        for value in [2, u8::MAX] {
            let err = PageType::try_from(value).unwrap_err();

            assert_eq!(
                err,
                SerializationError::InvalidValue {
                    field: "page_type",
                    value: value.into()
                }
            );
        }
    }

    #[test]
    fn page_header_should_occupy_sixteen_bytes() {
        assert_eq!(PageHeader::ENCODED_SIZE, 16);
    }

    #[test]
    fn page_header_read_should_decode_every_field() {
        let bytes = header_bytes(PageType::Slotted as u8, PAGE_FORMAT_VERSION);

        let header = PageHeader::read(&bytes).unwrap();

        assert_eq!(
            header,
            PageHeader {
                lsn: 0x0102_0304_0506_0708,
                checksum: 0x0C0B_0A09,
                page_type: PageType::Slotted,
                version: PAGE_FORMAT_VERSION,
            }
        );
    }

    #[test]
    fn page_header_read_of_zeroed_page_should_be_free() {
        let header = PageHeader::read(&[0; 32]).unwrap();

        assert_eq!(header.page_type, PageType::Free);
        assert_eq!(header.version, 0);
    }

    #[test]
    fn page_header_read_with_unknown_type_should_fail() {
        let bytes = header_bytes(7, PAGE_FORMAT_VERSION);

        let err = PageHeader::read(&bytes).unwrap_err();

        assert_eq!(
            err,
            PageError::Serialization(SerializationError::InvalidValue {
                field: "page_type",
                value: 7
            })
        );
    }

    #[test]
    fn page_header_read_with_other_version_should_fail() {
        for version in [0, PAGE_FORMAT_VERSION + 1] {
            let bytes = header_bytes(PageType::Slotted as u8, version);

            let err = PageHeader::read(&bytes).unwrap_err();

            assert_eq!(
                err,
                PageError::UnsupportedVersion {
                    found: version,
                    expected: PAGE_FORMAT_VERSION
                }
            );
        }
    }

    #[test]
    fn page_header_read_of_short_page_should_fail_with_eof() {
        let err = PageHeader::read(&[0; 15]).unwrap_err();

        assert_eq!(
            err,
            PageError::Serialization(SerializationError::UnexpectedEof {
                needed: 16,
                remaining: 15
            })
        );
    }

    #[test]
    fn page_header_encode_should_zero_reserved_bytes() {
        let header = PageHeader {
            lsn: 0x0102_0304_0506_0708,
            checksum: 0x0C0B_0A09,
            page_type: PageType::Slotted,
            version: PAGE_FORMAT_VERSION,
        };
        let mut bytes = [0xFF; 17];
        let mut writer = ByteWriter::new(&mut bytes);

        header.encode(&mut writer).unwrap();

        assert_eq!(
            bytes,
            [8, 7, 6, 5, 4, 3, 2, 1, 9, 10, 11, 12, 1, 1, 0, 0, 0xFF]
        );
    }

    #[test]
    fn page_header_should_round_trip_through_encode_and_decode() {
        let header = PageHeader {
            lsn: u64::MAX,
            checksum: u32::MAX,
            page_type: PageType::Slotted,
            version: PAGE_FORMAT_VERSION,
        };
        let mut bytes = [0; 16];

        header.encode(&mut ByteWriter::new(&mut bytes)).unwrap();

        assert_eq!(
            PageHeader::decode(&mut ByteReader::new(&bytes)).unwrap(),
            header
        );
    }

    #[test]
    fn page_header_encode_into_short_buffer_should_fail_without_writing() {
        let header = PageHeader {
            lsn: 1,
            checksum: 2,
            page_type: PageType::Slotted,
            version: PAGE_FORMAT_VERSION,
        };
        let mut bytes = [0xFF; 15];

        let err = header.encode(&mut ByteWriter::new(&mut bytes)).unwrap_err();

        assert_eq!(
            err,
            SerializationError::BufferOverflow {
                needed: 16,
                remaining: 15
            }
        );
        assert_eq!(bytes, [0xFF; 15]);
    }

    #[test]
    fn page_header_format_should_set_type_and_version_keeping_lsn_and_checksum() {
        let mut bytes = header_bytes(0xEE, 0xEE);
        bytes[HEADER_RESERVED_OFFSET..].fill(0xAB);

        PageHeader::format(&mut bytes, PageType::Slotted).unwrap();

        assert_eq!(
            PageHeader::read(&bytes).unwrap(),
            PageHeader {
                lsn: 0x0102_0304_0506_0708,
                checksum: 0x0C0B_0A09,
                page_type: PageType::Slotted,
                version: PAGE_FORMAT_VERSION,
            }
        );
        assert_eq!(&bytes[HEADER_RESERVED_OFFSET..COMMON_HEADER_SIZE], &[0, 0]);
        assert!(bytes[COMMON_HEADER_SIZE..].iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn page_header_format_of_short_page_should_fail_without_writing() {
        let mut bytes = [0xEE; 15];

        let err = PageHeader::format(&mut bytes, PageType::Slotted).unwrap_err();

        assert_eq!(
            err,
            PageError::Serialization(SerializationError::BufferOverflow {
                needed: 16,
                remaining: 15
            })
        );
        assert_eq!(bytes, [0xEE; 15]);
    }

    #[test]
    fn page_header_format_of_page_without_lsn_and_checksum_should_fail() {
        let mut bytes = [0xEE; 11];

        let err = PageHeader::format(&mut bytes, PageType::Slotted).unwrap_err();

        assert_eq!(
            err,
            PageError::Serialization(SerializationError::UnexpectedEof {
                needed: CHECKSUM_SIZE,
                remaining: 3
            })
        );
        assert_eq!(bytes, [0xEE; 11]);
    }

    #[test]
    fn page_errors_should_describe_the_problem() {
        let version = PageError::UnsupportedVersion {
            found: 2,
            expected: 1,
        };
        let header = PageError::Serialization(SerializationError::InvalidValue {
            field: "page_type",
            value: 7,
        });

        assert_eq!(
            version.to_string(),
            "page format version 2 is not supported (expected 1)"
        );
        assert_eq!(
            header.to_string(),
            "invalid page header: invalid value 7 for page_type"
        );
        assert!(std::error::Error::source(&header).is_some());
    }
}
