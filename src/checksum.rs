use crate::page::{CHECKSUM_OFFSET, CHECKSUM_SIZE};
use crate::serialization::{get_u32, put_u32};

const POLYNOMIAL: u32 = 0xEDB8_8320;
const CHECKSUM_END: usize = CHECKSUM_OFFSET + CHECKSUM_SIZE;

static TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLYNOMIAL
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

fn update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc = TABLE[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

/// CRC32 (IEEE 802.3), the same variant used by zlib and PNG.
pub fn crc32(data: &[u8]) -> u32 {
    !update(!0, data)
}

/// CRC32 of a page computed as if the checksum field were zeroed.
///
/// Panics if the page is smaller than the common page header.
pub fn page_checksum(page: &[u8]) -> u32 {
    let crc = update(!0, &page[..CHECKSUM_OFFSET]);
    let crc = update(crc, &[0; CHECKSUM_SIZE]);
    !update(crc, &page[CHECKSUM_END..])
}

pub fn stored_checksum(page: &[u8]) -> u32 {
    get_u32(page, CHECKSUM_OFFSET)
}

/// Writes the page checksum into the checksum field.
pub fn stamp_page(page: &mut [u8]) {
    let checksum = page_checksum(page);
    put_u32(page, CHECKSUM_OFFSET, checksum);
}

/// An all-zero page was never written (new page), so it is accepted without a checksum.
pub fn verify_page(page: &[u8]) -> bool {
    page.iter().all(|&b| b == 0) || stored_checksum(page) == page_checksum(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_of_empty_is_zero() {
        assert_eq!(crc32(&[]), 0);
    }

    #[test]
    fn crc32_standard_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn crc32_should_detect_single_bit_flip() {
        let data = vec![0xA5; 64];
        let mut flipped = data.clone();
        flipped[40] ^= 0b0000_0100;

        assert_ne!(crc32(&data), crc32(&flipped));
    }

    #[test]
    fn page_checksum_should_ignore_checksum_field() {
        let mut page = vec![7; 32];
        let before = page_checksum(&page);

        page[CHECKSUM_OFFSET..CHECKSUM_END].copy_from_slice(&[1, 2, 3, 4]);

        assert_eq!(page_checksum(&page), before);
    }

    #[test]
    fn page_checksum_should_equal_crc_of_page_with_zeroed_field() {
        let mut page: Vec<u8> = (0..32).collect();
        let checksum = page_checksum(&page);

        page[CHECKSUM_OFFSET..CHECKSUM_END].fill(0);

        assert_eq!(checksum, crc32(&page));
    }

    #[test]
    fn stamped_page_should_verify() {
        let mut page: Vec<u8> = (0..32).collect();

        stamp_page(&mut page);

        assert_eq!(stored_checksum(&page), page_checksum(&page));
        assert!(verify_page(&page));
    }

    #[test]
    fn modified_page_should_fail_verification() {
        let mut page: Vec<u8> = (0..32).collect();
        stamp_page(&mut page);

        page[20] ^= 1;

        assert!(!verify_page(&page));
    }

    #[test]
    fn zeroed_page_should_verify() {
        assert!(verify_page(&[0; 32]));
    }

    #[test]
    fn unstamped_non_zero_page_should_fail_verification() {
        let mut page = vec![0; 32];
        page[20] = 1;

        assert!(!verify_page(&page));
    }
}
