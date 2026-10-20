//! Database header, stored in page 0 (all integers little-endian):
//!
//! ```text
//! [0..16)   common page header   page type = DatabaseHeader
//! [16..24)  magic                b"ferroDB\0"
//! [24..28)  tables_first_page_id  u32, first page of the catalog's tables heap
//! [28..32)  columns_first_page_id u32, first page of the catalog's columns heap
//! [32..36)  indexes_first_page_id u32, first page of the catalog's indexes heap, 0 until
//!                                 the first index is created
//! [36..)    reserved             zeroed
//! ```
//!
//! Databases from before indexes have zeros in `32..36` (reserved then), so they open with
//! no indexes heap.
//!
//! The checksum already catches flipped bytes; the magic tells a ferroDB database apart
//! from a file of another program whose page 0 happens to be valid.

use super::CatalogError;
use crate::serialization::{ByteReader, ByteWriter};
use crate::storage::page::{COMMON_HEADER_SIZE, PageHeader, PageType};

const MAGIC: &[u8; 8] = b"ferroDB\0";
const MAGIC_OFFSET: usize = COMMON_HEADER_SIZE;
const HEADER_END: usize = MAGIC_OFFSET + MAGIC.len() + 4 + 4 + 4;

const TOO_SMALL: CatalogError = CatalogError::Corrupted {
    reason: "page too small for the database header",
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatabaseHeader {
    pub tables_first_page_id: u32,
    pub columns_first_page_id: u32,
    /// `0` until the first index is created (and in databases from before indexes).
    pub indexes_first_page_id: u32,
}

impl DatabaseHeader {
    /// `None` when page 0 was never written: the database still has to be created.
    pub fn read(page: &[u8]) -> Result<Option<Self>, CatalogError> {
        let page_header = PageHeader::read(page).map_err(|_| CatalogError::Corrupted {
            reason: "invalid header in page 0",
        })?;
        match page_header.page_type {
            PageType::Free => return Ok(None),
            PageType::Slotted
            | PageType::BPlusTreeHeader
            | PageType::BPlusTreeNode
            | PageType::BPlusTreeFree => {
                return Err(CatalogError::NotADatabase {
                    reason: "page 0 is not a database header",
                });
            }
            PageType::DatabaseHeader => {}
        }
        if page.len() < HEADER_END {
            return Err(TOO_SMALL);
        }

        let mut reader = ByteReader::new(page.get(MAGIC_OFFSET..).unwrap_or_default());
        let magic: [u8; 8] = reader.read_array().map_err(|_| TOO_SMALL)?;
        if &magic != MAGIC {
            return Err(CatalogError::NotADatabase {
                reason: "wrong magic number",
            });
        }
        Ok(Some(Self {
            tables_first_page_id: reader.read_u32().map_err(|_| TOO_SMALL)?,
            columns_first_page_id: reader.read_u32().map_err(|_| TOO_SMALL)?,
            indexes_first_page_id: reader.read_u32().map_err(|_| TOO_SMALL)?,
        }))
    }

    /// Formats `page` as the database header, keeping its LSN and checksum. Checks the
    /// size first, so a page too small is left untouched.
    pub fn write(&self, page: &mut [u8]) -> Result<(), CatalogError> {
        if page.len() < HEADER_END {
            return Err(TOO_SMALL);
        }
        PageHeader::format(page, PageType::DatabaseHeader).map_err(|_| TOO_SMALL)?;
        let mut writer = ByteWriter::new(page.get_mut(MAGIC_OFFSET..).unwrap_or_default());
        writer
            .write_bytes(MAGIC)
            .and_then(|()| writer.write_u32(self.tables_first_page_id))
            .and_then(|()| writer.write_u32(self.columns_first_page_id))
            .and_then(|()| writer.write_u32(self.indexes_first_page_id))
            .and_then(|()| writer.write_zeros(writer.remaining()))
            .map_err(|_| TOO_SMALL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serialization::put_u64;
    use crate::storage::page::{
        LSN_OFFSET, PAGE_FORMAT_VERSION, PAGE_TYPE_OFFSET, PageType, VERSION_OFFSET,
    };
    use crate::storage::slotted_page::SlottedPage;

    const PAGE: usize = 64;

    fn header() -> DatabaseHeader {
        DatabaseHeader {
            tables_first_page_id: 0x0403_0201,
            columns_first_page_id: 7,
            indexes_first_page_id: 0x0A0B_0C0D,
        }
    }

    fn written(header: DatabaseHeader) -> Vec<u8> {
        let mut page = vec![0; PAGE];
        header.write(&mut page).unwrap();
        page
    }

    #[test]
    fn write_should_produce_the_documented_bytes() {
        let page = written(header());

        assert_eq!(page[PAGE_TYPE_OFFSET], PageType::DatabaseHeader as u8);
        assert_eq!(page[VERSION_OFFSET], PAGE_FORMAT_VERSION);
        assert_eq!(&page[16..24], b"ferroDB\0");
        assert_eq!(page[24..28], [1, 2, 3, 4]);
        assert_eq!(page[28..32], [7, 0, 0, 0]);
        assert_eq!(page[32..36], [0x0D, 0x0C, 0x0B, 0x0A]);
        assert!(page[36..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn write_then_read_should_round_trip() {
        let page = written(header());

        assert_eq!(DatabaseHeader::read(&page).unwrap(), Some(header()));
    }

    #[test]
    fn write_should_keep_the_lsn_and_clear_the_reserved_bytes() {
        let mut page = vec![0xAB; PAGE];
        put_u64(&mut page, LSN_OFFSET, 42);

        header().write(&mut page).unwrap();

        assert_eq!(page[LSN_OFFSET], 42);
        assert!(page[36..].iter().all(|&byte| byte == 0));
        assert_eq!(DatabaseHeader::read(&page).unwrap(), Some(header()));
    }

    #[test]
    fn a_header_from_before_indexes_should_read_as_having_no_indexes_heap() {
        let mut page = written(header());
        // Those bytes were reserved and zeroed; the checksum is not part of this layer.
        page[32..36].copy_from_slice(&[0; 4]);

        let read = DatabaseHeader::read(&page).unwrap().unwrap();

        assert_eq!(read.indexes_first_page_id, 0);
        assert_eq!(read.tables_first_page_id, header().tables_first_page_id);
    }

    #[test]
    fn read_of_a_page_never_written_should_be_none() {
        assert_eq!(DatabaseHeader::read(&[0; PAGE]).unwrap(), None);
    }

    #[test]
    fn read_of_a_slotted_page_should_fail() {
        let mut slotted = vec![0; PAGE];
        SlottedPage::init(slotted.as_mut_slice()).unwrap();

        let err = DatabaseHeader::read(&slotted).unwrap_err();

        assert!(matches!(
            err,
            CatalogError::NotADatabase {
                reason: "page 0 is not a database header"
            }
        ));
    }

    #[test]
    fn read_of_a_b_plus_tree_page_should_fail() {
        use crate::storage::page::{PageHeader, PageType};
        for page_type in [
            PageType::BPlusTreeHeader,
            PageType::BPlusTreeNode,
            PageType::BPlusTreeFree,
        ] {
            let mut page = vec![0; PAGE];
            PageHeader::format(&mut page, page_type).unwrap();

            let err = DatabaseHeader::read(&page).unwrap_err();

            assert!(matches!(
                err,
                CatalogError::NotADatabase {
                    reason: "page 0 is not a database header"
                }
            ));
        }
    }

    #[test]
    fn read_with_a_wrong_magic_should_fail() {
        let mut page = written(header());
        page[16] = b'F';

        let err = DatabaseHeader::read(&page).unwrap_err();

        assert!(matches!(
            err,
            CatalogError::NotADatabase {
                reason: "wrong magic number"
            }
        ));
    }

    #[test]
    fn a_page_too_small_for_the_header_should_fail() {
        let mut small = vec![0; 24];

        let write_err = header().write(&mut small).unwrap_err();
        small[PAGE_TYPE_OFFSET] = PageType::DatabaseHeader as u8;
        small[VERSION_OFFSET] = PAGE_FORMAT_VERSION;
        let read_err = DatabaseHeader::read(&small).unwrap_err();

        for err in [write_err, read_err] {
            assert!(matches!(
                err,
                CatalogError::Corrupted {
                    reason: "page too small for the database header"
                }
            ));
        }
    }
}
