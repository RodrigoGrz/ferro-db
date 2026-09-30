use std::fmt;

use crate::serialization::{ByteReader, ByteWriter, Decode, Encode, SerializationError};

#[derive(Debug)]
pub enum RidError {
    InvalidLength { expected: usize, actual: usize },
}

impl fmt::Display for RidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RidError::InvalidLength { expected, actual } => {
                write!(f, "rid must be {expected} bytes, got {actual}")
            }
        }
    }
}

impl std::error::Error for RidError {}

/// Record identifier: the physical address of a tuple (page + slot inside the page).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Rid {
    pub page_id: u32,
    pub slot_id: u16,
}

impl Rid {
    pub const SIZE: usize = 6;

    pub fn new(page_id: u32, slot_id: u16) -> Self {
        Self { page_id, slot_id }
    }

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut bytes = [0; Self::SIZE];
        bytes[0..4].copy_from_slice(&self.page_id.to_le_bytes());
        bytes[4..6].copy_from_slice(&self.slot_id.to_le_bytes());
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RidError> {
        let array = bytes.try_into().map_err(|_| RidError::InvalidLength {
            expected: Self::SIZE,
            actual: bytes.len(),
        })?;
        Ok(Self::from_array(array))
    }

    fn from_array(bytes: [u8; Self::SIZE]) -> Self {
        let [p0, p1, p2, p3, s0, s1] = bytes;
        Self {
            page_id: u32::from_le_bytes([p0, p1, p2, p3]),
            slot_id: u16::from_le_bytes([s0, s1]),
        }
    }
}

impl Encode for Rid {
    const ENCODED_SIZE: usize = Self::SIZE;

    fn encode(&self, writer: &mut ByteWriter<'_>) -> Result<(), SerializationError> {
        writer.write_bytes(&self.to_bytes())
    }
}

impl Decode for Rid {
    fn decode(reader: &mut ByteReader<'_>) -> Result<Self, SerializationError> {
        reader.read_array().map(Self::from_array)
    }
}

impl fmt::Display for Rid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.page_id, self.slot_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rid_bytes_round_trip() {
        for rid in [Rid::new(0, 0), Rid::new(3, 7), Rid::new(u32::MAX, u16::MAX)] {
            assert_eq!(Rid::from_bytes(&rid.to_bytes()).unwrap(), rid);
        }
    }

    #[test]
    fn rid_to_bytes_layout() {
        let rid = Rid::new(0x0403_0201, 0x0605);

        assert_eq!(rid.to_bytes(), [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn rid_ordering_is_page_then_slot() {
        let mut rids = vec![Rid::new(2, 0), Rid::new(1, 9), Rid::new(1, 2)];

        rids.sort();

        assert_eq!(rids, vec![Rid::new(1, 2), Rid::new(1, 9), Rid::new(2, 0)]);
    }

    #[test]
    fn rid_display() {
        assert_eq!(Rid::new(3, 7).to_string(), "(3, 7)");
    }

    #[test]
    fn rid_encoded_size_should_match_size() {
        assert_eq!(Rid::ENCODED_SIZE, Rid::SIZE);
    }

    #[test]
    fn rid_encode_should_write_after_previous_fields() {
        let mut bytes = [0; 7];
        let mut writer = ByteWriter::new(&mut bytes);
        writer.write_u8(0xEE).unwrap();

        Rid::new(0x0403_0201, 0x0605).encode(&mut writer).unwrap();

        assert_eq!(writer.position(), 7);
        assert_eq!(bytes, [0xEE, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn rid_decode_should_read_after_previous_fields() {
        let bytes = [0xEE, 1, 2, 3, 4, 5, 6];
        let mut reader = ByteReader::new(&bytes);
        reader.read_u8().unwrap();

        let rid = Rid::decode(&mut reader).unwrap();

        assert_eq!(rid, Rid::new(0x0403_0201, 0x0605));
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn rid_encode_should_round_trip_through_decode() {
        let rids = [Rid::new(0, 0), Rid::new(3, 7), Rid::new(u32::MAX, u16::MAX)];
        let mut bytes = [0; 3 * Rid::SIZE];
        let mut writer = ByteWriter::new(&mut bytes);
        for rid in rids {
            rid.encode(&mut writer).unwrap();
        }

        let mut reader = ByteReader::new(&bytes);

        for rid in rids {
            assert_eq!(Rid::decode(&mut reader).unwrap(), rid);
        }
    }

    #[test]
    fn rid_decode_with_five_bytes_should_fail_with_eof() {
        let bytes = [1, 2, 3, 4, 5];
        let mut reader = ByteReader::new(&bytes);

        let err = Rid::decode(&mut reader).unwrap_err();

        assert_eq!(
            err,
            SerializationError::UnexpectedEof {
                needed: Rid::SIZE,
                remaining: 5
            }
        );
        assert_eq!(reader.position(), 0);
    }

    #[test]
    fn rid_encode_into_short_buffer_should_fail_with_overflow() {
        let mut bytes = [0; 5];
        let mut writer = ByteWriter::new(&mut bytes);

        let err = Rid::new(1, 2).encode(&mut writer).unwrap_err();

        assert_eq!(
            err,
            SerializationError::BufferOverflow {
                needed: Rid::SIZE,
                remaining: 5
            }
        );
        assert_eq!(bytes, [0; 5]);
    }

    #[test]
    fn rid_from_bytes_wrong_length_should_fail() {
        let err = Rid::from_bytes(&[1, 2, 3]).unwrap_err();

        assert!(matches!(
            err,
            RidError::InvalidLength {
                expected: 6,
                actual: 3
            }
        ));
    }
}
