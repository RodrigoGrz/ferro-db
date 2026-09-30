//! Little-endian encoding of on-disk structures.
//!
//! `ByteReader` and `ByteWriter` are cursors that check bounds on every call and
//! never panic: a failed read or write leaves the cursor and the bytes untouched.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerializationError {
    UnexpectedEof { needed: usize, remaining: usize },
    BufferOverflow { needed: usize, remaining: usize },
    InvalidValue { field: &'static str, value: u64 },
}

impl fmt::Display for SerializationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SerializationError::UnexpectedEof { needed, remaining } => {
                write!(
                    f,
                    "needed {needed} bytes to read, only {remaining} remaining"
                )
            }
            SerializationError::BufferOverflow { needed, remaining } => {
                write!(
                    f,
                    "needed {needed} bytes to write, only {remaining} remaining"
                )
            }
            SerializationError::InvalidValue { field, value } => {
                write!(f, "invalid value {value} for {field}")
            }
        }
    }
}

impl std::error::Error for SerializationError {}

#[derive(Debug)]
pub struct ByteReader<'a> {
    unread: &'a [u8],
    position: usize,
}

impl<'a> ByteReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            unread: bytes,
            position: 0,
        }
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn remaining(&self) -> usize {
        self.unread.len()
    }

    pub fn skip(&mut self, len: usize) -> Result<(), SerializationError> {
        self.read_bytes(len).map(|_| ())
    }

    pub fn read_u8(&mut self) -> Result<u8, SerializationError> {
        self.read_array().map(u8::from_le_bytes)
    }

    pub fn read_u16(&mut self) -> Result<u16, SerializationError> {
        self.read_array().map(u16::from_le_bytes)
    }

    pub fn read_u32(&mut self) -> Result<u32, SerializationError> {
        self.read_array().map(u32::from_le_bytes)
    }

    pub fn read_u64(&mut self) -> Result<u64, SerializationError> {
        self.read_array().map(u64::from_le_bytes)
    }

    pub fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], SerializationError> {
        let (bytes, rest) = self
            .unread
            .split_at_checked(len)
            .ok_or_else(|| self.eof(len))?;
        self.advance(rest, len);
        Ok(bytes)
    }

    pub fn read_array<const N: usize>(&mut self) -> Result<[u8; N], SerializationError> {
        let (bytes, rest) = self
            .unread
            .split_first_chunk::<N>()
            .ok_or_else(|| self.eof(N))?;
        self.advance(rest, N);
        Ok(*bytes)
    }

    fn advance(&mut self, rest: &'a [u8], len: usize) {
        self.unread = rest;
        self.position += len;
    }

    fn eof(&self, needed: usize) -> SerializationError {
        SerializationError::UnexpectedEof {
            needed,
            remaining: self.remaining(),
        }
    }
}

#[derive(Debug)]
pub struct ByteWriter<'a> {
    bytes: &'a mut [u8],
    position: usize,
}

impl<'a> ByteWriter<'a> {
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    pub fn write_u8(&mut self, value: u8) -> Result<(), SerializationError> {
        self.write_bytes(&value.to_le_bytes())
    }

    pub fn write_u16(&mut self, value: u16) -> Result<(), SerializationError> {
        self.write_bytes(&value.to_le_bytes())
    }

    pub fn write_u32(&mut self, value: u32) -> Result<(), SerializationError> {
        self.write_bytes(&value.to_le_bytes())
    }

    pub fn write_u64(&mut self, value: u64) -> Result<(), SerializationError> {
        self.write_bytes(&value.to_le_bytes())
    }

    pub fn write_bytes(&mut self, value: &[u8]) -> Result<(), SerializationError> {
        self.claim(value.len())?.copy_from_slice(value);
        Ok(())
    }

    pub fn write_zeros(&mut self, len: usize) -> Result<(), SerializationError> {
        self.claim(len)?.fill(0);
        Ok(())
    }

    /// Reserves the next `len` bytes, or fails before touching anything.
    fn claim(&mut self, len: usize) -> Result<&mut [u8], SerializationError> {
        let overflow = SerializationError::BufferOverflow {
            needed: len,
            remaining: self.remaining(),
        };
        let start = self.position;
        let end = start.checked_add(len).ok_or(overflow)?;
        let range = self.bytes.get_mut(start..end).ok_or(overflow)?;
        self.position = end;
        Ok(range)
    }
}

/// A structure with a fixed-size on-disk representation.
pub trait Encode {
    const ENCODED_SIZE: usize;

    fn encode(&self, writer: &mut ByteWriter<'_>) -> Result<(), SerializationError>;
}

pub trait Decode: Sized {
    fn decode(reader: &mut ByteReader<'_>) -> Result<Self, SerializationError>;
}

// Offset-based access for hot paths whose bounds were validated up front
// (e.g. slots of a slotted page checked by `SlottedPage::new`).

fn array_at<const N: usize>(bytes: &[u8], offset: usize) -> [u8; N] {
    let mut array = [0; N];
    array.copy_from_slice(&bytes[offset..offset + N]);
    array
}

pub(crate) fn get_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(array_at(bytes, offset))
}

pub(crate) fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(array_at(bytes, offset))
}

pub(crate) fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(array_at(bytes, offset))
}

pub(crate) fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[derive(Debug, Clone, PartialEq)]
    enum Field {
        U8(u8),
        U16(u16),
        U32(u32),
        U64(u64),
        Bytes(Vec<u8>),
    }

    impl Field {
        fn random(rng: &mut Rng) -> Self {
            match rng.below(5) {
                0 => Field::U8(rng.next() as u8),
                1 => Field::U16(rng.next() as u16),
                2 => Field::U32(rng.next() as u32),
                3 => Field::U64(rng.next()),
                _ => Field::Bytes((0..rng.below(16)).map(|_| rng.next() as u8).collect()),
            }
        }

        fn size(&self) -> usize {
            match self {
                Field::U8(_) => 1,
                Field::U16(_) => 2,
                Field::U32(_) => 4,
                Field::U64(_) => 8,
                Field::Bytes(bytes) => bytes.len(),
            }
        }

        fn write(&self, writer: &mut ByteWriter<'_>) -> Result<(), SerializationError> {
            match self {
                Field::U8(value) => writer.write_u8(*value),
                Field::U16(value) => writer.write_u16(*value),
                Field::U32(value) => writer.write_u32(*value),
                Field::U64(value) => writer.write_u64(*value),
                Field::Bytes(bytes) => writer.write_bytes(bytes),
            }
        }

        fn read_like(&self, reader: &mut ByteReader<'_>) -> Result<Self, SerializationError> {
            Ok(match self {
                Field::U8(_) => Field::U8(reader.read_u8()?),
                Field::U16(_) => Field::U16(reader.read_u16()?),
                Field::U32(_) => Field::U32(reader.read_u32()?),
                Field::U64(_) => Field::U64(reader.read_u64()?),
                Field::Bytes(bytes) => Field::Bytes(reader.read_bytes(bytes.len())?.to_vec()),
            })
        }
    }

    #[test]
    fn random_field_sequences_should_round_trip() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

        for _ in 0..1_000 {
            let fields: Vec<Field> = (0..rng.below(20))
                .map(|_| Field::random(&mut rng))
                .collect();
            let total: usize = fields.iter().map(Field::size).sum();
            let mut buffer = vec![0; total + rng.below(4)];
            let mut writer = ByteWriter::new(&mut buffer);
            for field in &fields {
                field.write(&mut writer).unwrap();
            }
            assert_eq!(writer.position(), total);

            let mut reader = ByteReader::new(&buffer);
            for field in &fields {
                assert_eq!(&field.read_like(&mut reader).unwrap(), field);
            }
            assert_eq!(reader.position(), total);
        }
    }

    #[test]
    fn writes_into_a_short_buffer_should_fail_at_the_first_field_that_does_not_fit() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);

        for _ in 0..1_000 {
            let fields: Vec<Field> = (0..1 + rng.below(20))
                .map(|_| Field::random(&mut rng))
                .collect();
            let total: usize = fields.iter().map(Field::size).sum();
            let mut buffer = vec![0xAA; rng.below(total + 1)];
            let capacity = buffer.len();
            let mut writer = ByteWriter::new(&mut buffer);

            let mut written = 0;
            for field in &fields {
                match field.write(&mut writer) {
                    Ok(()) => written += field.size(),
                    Err(err) => {
                        assert_eq!(
                            err,
                            SerializationError::BufferOverflow {
                                needed: field.size(),
                                remaining: capacity - written
                            }
                        );
                        break;
                    }
                }
                assert_eq!(writer.position(), written);
            }
            assert_eq!(writer.position(), written);
        }
    }

    #[test]
    fn offset_helpers_should_match_the_cursor_layout() {
        let mut bytes = [0; 17];
        let mut writer = ByteWriter::new(&mut bytes);
        writer.write_u8(0xEE).unwrap();
        writer.write_u16(0xBEEF).unwrap();
        writer.write_u32(0xDEAD_BEEF).unwrap();
        writer.write_u64(0x0123_4567_89AB_CDEF).unwrap();

        assert_eq!(get_u16(&bytes, 1), 0xBEEF);
        assert_eq!(get_u32(&bytes, 3), 0xDEAD_BEEF);
        assert_eq!(get_u64(&bytes, 7), 0x0123_4567_89AB_CDEF);
    }

    #[test]
    fn offset_helpers_should_write_little_endian_in_place() {
        let mut bytes = [0xFF; 16];

        put_u16(&mut bytes, 1, 0x0201);
        put_u32(&mut bytes, 3, 0x0605_0403);
        put_u64(&mut bytes, 7, 0x0E0D_0C0B_0A09_0807);

        assert_eq!(
            bytes,
            [0xFF, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 0xFF]
        );
    }

    #[test]
    fn offset_helpers_should_round_trip_limits() {
        let mut bytes = [0; 14];

        put_u16(&mut bytes, 0, u16::MAX);
        put_u32(&mut bytes, 2, u32::MAX);
        put_u64(&mut bytes, 6, u64::MAX);

        assert_eq!(get_u16(&bytes, 0), u16::MAX);
        assert_eq!(get_u32(&bytes, 2), u32::MAX);
        assert_eq!(get_u64(&bytes, 6), u64::MAX);
    }

    #[test]
    fn reader_should_decode_little_endian_integers() {
        let bytes = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let mut reader = ByteReader::new(&bytes);

        let byte = reader.read_u8().unwrap();
        let short = reader.read_u16().unwrap();
        let int = reader.read_u32().unwrap();
        let long = reader.read_u64().unwrap();

        assert_eq!(byte, 0x01);
        assert_eq!(short, 0x0302);
        assert_eq!(int, 0x0706_0504);
        assert_eq!(long, 0x0F0E_0D0C_0B0A_0908);
        assert_eq!(reader.position(), 15);
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn writer_should_encode_little_endian_integers() {
        let mut bytes = [0; 15];
        let mut writer = ByteWriter::new(&mut bytes);

        writer.write_u8(0x01).unwrap();
        writer.write_u16(0x0302).unwrap();
        writer.write_u32(0x0706_0504).unwrap();
        writer.write_u64(0x0F0E_0D0C_0B0A_0908).unwrap();

        assert_eq!(writer.position(), 15);
        assert_eq!(writer.remaining(), 0);
        assert_eq!(bytes, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
    }

    #[test]
    fn integers_should_round_trip_at_their_limits() {
        let mut bytes = [0; 45];
        let mut writer = ByteWriter::new(&mut bytes);
        for value in [0, 1, u8::MAX] {
            writer.write_u8(value).unwrap();
        }
        for value in [0, 1, u16::MAX] {
            writer.write_u16(value).unwrap();
        }
        for value in [0, 1, u32::MAX] {
            writer.write_u32(value).unwrap();
        }
        for value in [0, 1, u64::MAX] {
            writer.write_u64(value).unwrap();
        }

        let mut reader = ByteReader::new(&bytes);

        for value in [0, 1, u8::MAX] {
            assert_eq!(reader.read_u8().unwrap(), value);
        }
        for value in [0, 1, u16::MAX] {
            assert_eq!(reader.read_u16().unwrap(), value);
        }
        for value in [0, 1, u32::MAX] {
            assert_eq!(reader.read_u32().unwrap(), value);
        }
        for value in [0, 1, u64::MAX] {
            assert_eq!(reader.read_u64().unwrap(), value);
        }
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn reader_on_empty_buffer_should_fail_with_eof() {
        let mut reader = ByteReader::new(&[]);

        let err = reader.read_u8().unwrap_err();

        assert_eq!(
            err,
            SerializationError::UnexpectedEof {
                needed: 1,
                remaining: 0
            }
        );
    }

    #[test]
    fn reader_one_byte_short_should_fail_without_advancing() {
        let bytes = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut reader = ByteReader::new(&bytes);
        reader.read_u8().unwrap();

        let err = reader.read_u64().unwrap_err();

        assert_eq!(
            err,
            SerializationError::UnexpectedEof {
                needed: 8,
                remaining: 7
            }
        );
        assert_eq!(reader.position(), 1);
        assert_eq!(reader.read_u32().unwrap(), 0x0504_0302);
    }

    #[test]
    fn reader_exact_size_should_succeed() {
        let bytes = [0xFF, 0xFF];
        let mut reader = ByteReader::new(&bytes);

        assert_eq!(reader.read_u16().unwrap(), u16::MAX);
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn read_bytes_should_borrow_from_the_buffer() {
        let bytes = [1, 2, 3, 4];
        let slice = {
            let mut reader = ByteReader::new(&bytes);
            reader.read_u8().unwrap();
            reader.read_bytes(2).unwrap()
        };

        assert_eq!(slice, &[2, 3]);
    }

    #[test]
    fn read_bytes_past_the_end_should_fail_without_advancing() {
        let bytes = [1, 2, 3];
        let mut reader = ByteReader::new(&bytes);

        let err = reader.read_bytes(4).unwrap_err();

        assert_eq!(
            err,
            SerializationError::UnexpectedEof {
                needed: 4,
                remaining: 3
            }
        );
        assert_eq!(reader.position(), 0);
    }

    #[test]
    fn read_bytes_of_zero_length_should_succeed_on_empty_buffer() {
        let mut reader = ByteReader::new(&[]);

        assert_eq!(reader.read_bytes(0).unwrap(), &[] as &[u8]);
    }

    #[test]
    fn read_array_should_copy_a_fixed_size_chunk() {
        let bytes = [1, 2, 3, 4];
        let mut reader = ByteReader::new(&bytes);

        let array: [u8; 3] = reader.read_array().unwrap();

        assert_eq!(array, [1, 2, 3]);
        assert_eq!(reader.position(), 3);
    }

    #[test]
    fn read_array_past_the_end_should_fail_without_advancing() {
        let bytes = [1, 2];
        let mut reader = ByteReader::new(&bytes);

        let err = reader.read_array::<3>().unwrap_err();

        assert_eq!(
            err,
            SerializationError::UnexpectedEof {
                needed: 3,
                remaining: 2
            }
        );
        assert_eq!(reader.position(), 0);
    }

    #[test]
    fn skip_should_advance_the_cursor() {
        let bytes = [1, 2, 3, 4];
        let mut reader = ByteReader::new(&bytes);

        reader.skip(3).unwrap();

        assert_eq!(reader.position(), 3);
        assert_eq!(reader.read_u8().unwrap(), 4);
    }

    #[test]
    fn skip_past_the_end_should_fail_without_advancing() {
        let bytes = [1, 2];
        let mut reader = ByteReader::new(&bytes);

        let err = reader.skip(3).unwrap_err();

        assert_eq!(
            err,
            SerializationError::UnexpectedEof {
                needed: 3,
                remaining: 2
            }
        );
        assert_eq!(reader.position(), 0);
    }

    #[test]
    fn writer_on_empty_buffer_should_fail_with_overflow() {
        let mut bytes = [];
        let mut writer = ByteWriter::new(&mut bytes);

        let err = writer.write_u8(1).unwrap_err();

        assert_eq!(
            err,
            SerializationError::BufferOverflow {
                needed: 1,
                remaining: 0
            }
        );
    }

    #[test]
    fn writer_one_byte_short_should_fail_without_writing() {
        let mut bytes = [0xAA; 5];
        let mut writer = ByteWriter::new(&mut bytes);
        writer.write_u8(1).unwrap();

        let err = writer.write_u64(u64::MAX).unwrap_err();

        assert_eq!(
            err,
            SerializationError::BufferOverflow {
                needed: 8,
                remaining: 4
            }
        );
        assert_eq!(writer.position(), 1);
        assert_eq!(bytes, [1, 0xAA, 0xAA, 0xAA, 0xAA]);
    }

    #[test]
    fn write_bytes_should_copy_the_slice() {
        let mut bytes = [0; 4];
        let mut writer = ByteWriter::new(&mut bytes);

        writer.write_u8(9).unwrap();
        writer.write_bytes(&[1, 2, 3]).unwrap();

        assert_eq!(bytes, [9, 1, 2, 3]);
    }

    #[test]
    fn write_bytes_past_the_end_should_fail_without_writing() {
        let mut bytes = [0; 2];
        let mut writer = ByteWriter::new(&mut bytes);

        let err = writer.write_bytes(&[1, 2, 3]).unwrap_err();

        assert_eq!(
            err,
            SerializationError::BufferOverflow {
                needed: 3,
                remaining: 2
            }
        );
        assert_eq!(writer.position(), 0);
        assert_eq!(bytes, [0, 0]);
    }

    #[test]
    fn write_zeros_should_clear_the_range() {
        let mut bytes = [0xFF; 4];
        let mut writer = ByteWriter::new(&mut bytes);

        writer.write_u8(7).unwrap();
        writer.write_zeros(2).unwrap();

        assert_eq!(writer.position(), 3);
        assert_eq!(bytes, [7, 0, 0, 0xFF]);
    }

    #[test]
    fn write_zeros_past_the_end_should_fail_without_writing() {
        let mut bytes = [0xFF; 2];
        let mut writer = ByteWriter::new(&mut bytes);

        let err = writer.write_zeros(3).unwrap_err();

        assert_eq!(
            err,
            SerializationError::BufferOverflow {
                needed: 3,
                remaining: 2
            }
        );
        assert_eq!(bytes, [0xFF, 0xFF]);
    }

    #[test]
    fn huge_lengths_should_fail_instead_of_overflowing() {
        let mut bytes = [0; 4];
        let mut writer = ByteWriter::new(&mut bytes);
        writer.write_u8(1).unwrap();

        let err = writer.write_zeros(usize::MAX).unwrap_err();

        assert_eq!(
            err,
            SerializationError::BufferOverflow {
                needed: usize::MAX,
                remaining: 3
            }
        );
    }

    #[test]
    fn errors_should_describe_the_problem() {
        let eof = SerializationError::UnexpectedEof {
            needed: 4,
            remaining: 1,
        };
        let overflow = SerializationError::BufferOverflow {
            needed: 2,
            remaining: 0,
        };
        let invalid = SerializationError::InvalidValue {
            field: "page_type",
            value: 9,
        };

        assert_eq!(eof.to_string(), "needed 4 bytes to read, only 1 remaining");
        assert_eq!(
            overflow.to_string(),
            "needed 2 bytes to write, only 0 remaining"
        );
        assert_eq!(invalid.to_string(), "invalid value 9 for page_type");
    }
}
