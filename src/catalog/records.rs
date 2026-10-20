//! Catalog entries, one tuple each (all integers little-endian):
//!
//! ```text
//! TableRecord   table_id u32 | first_page_id u32 | name_len u8 | name
//! ColumnRecord  table_id u32 | position u16 | data_type u8 [max_chars u16] | flags u8
//!               | name_len u8 | name
//! IndexRecord   index_id u32 | table_id u32 | column u16 | header_page_id u32
//!               | name_len u8 | name
//! ```
//!
//! `data_type`: 1 SMALLINT, 2 INTEGER, 3 BIGINT, 4 VARCHAR followed by its `n` (1 to 65535),
//! 5 BOOLEAN, 6 REAL, 7 DOUBLE PRECISION, 8 DATE, 9 TIMESTAMP; 0 stays invalid so a zeroed
//! entry never decodes. `flags`: bit 0 = nullable, the other bits must be 0. Names have 1 to
//! 63 bytes of UTF-8. Decoding rejects anything else, including missing or trailing bytes.
//! A table or column entry has at most 74 bytes and an index entry 78: every entry fits a page.
//!
//! An index's `column` is the position of its key column in the table, and `header_page_id`
//! is its tree's header page, never the reserved page 0.

use std::num::NonZeroU16;

use super::{CatalogError, IndexId, TableId};
use crate::serialization::ByteReader;
use crate::storage::page::RESERVED_PAGE_ID;
use crate::table::{Column, Schema};
use crate::types::DataType;

const MAX_NAME_LEN: usize = Schema::MAX_COLUMN_NAME_LEN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRecord {
    pub table_id: TableId,
    pub first_page_id: u32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRecord {
    pub table_id: TableId,
    pub position: u16,
    pub column: Column,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRecord {
    pub index_id: IndexId,
    pub table_id: TableId,
    pub column: u16,
    pub header_page_id: u32,
    pub name: String,
}

const NULLABLE_FLAG: u8 = 0b1;

const TRUNCATED: CatalogError = CatalogError::Corrupted {
    reason: "catalog entry is truncated",
};
const BAD_NAME_LEN: CatalogError = CatalogError::Corrupted {
    reason: "name must have 1 to 63 bytes",
};

impl TableRecord {
    /// Callers pass validated names; one outside 1..=63 bytes is still refused here, so
    /// nothing written could fail to decode.
    pub fn encode(&self) -> Result<Vec<u8>, CatalogError> {
        let name_len = name_len(&self.name)?;
        let mut bytes = Vec::with_capacity(9 + self.name.len());
        bytes.extend(self.table_id.0.to_le_bytes());
        bytes.extend(self.first_page_id.to_le_bytes());
        bytes.push(name_len);
        bytes.extend(self.name.as_bytes());
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CatalogError> {
        let mut reader = ByteReader::new(bytes);
        let record = Self {
            table_id: TableId(reader.read_u32().map_err(|_| TRUNCATED)?),
            first_page_id: reader.read_u32().map_err(|_| TRUNCATED)?,
            name: read_name(&mut reader)?,
        };
        expect_end(&reader)?;
        Ok(record)
    }
}

impl ColumnRecord {
    pub fn encode(&self) -> Result<Vec<u8>, CatalogError> {
        let name_len = name_len(&self.column.name)?;
        let flags = if self.column.nullable {
            NULLABLE_FLAG
        } else {
            0
        };
        let mut bytes = Vec::with_capacity(11 + self.column.name.len());
        bytes.extend(self.table_id.0.to_le_bytes());
        bytes.extend(self.position.to_le_bytes());
        write_data_type(&mut bytes, self.column.data_type);
        bytes.push(flags);
        bytes.push(name_len);
        bytes.extend(self.column.name.as_bytes());
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CatalogError> {
        let mut reader = ByteReader::new(bytes);
        let table_id = TableId(reader.read_u32().map_err(|_| TRUNCATED)?);
        let position = reader.read_u16().map_err(|_| TRUNCATED)?;
        let data_type = read_data_type(&mut reader)?;
        let nullable = nullable(reader.read_u8().map_err(|_| TRUNCATED)?)?;
        let name = read_name(&mut reader)?;
        expect_end(&reader)?;
        Ok(Self {
            table_id,
            position,
            column: Column {
                name,
                data_type,
                nullable,
            },
        })
    }
}

impl IndexRecord {
    pub fn encode(&self) -> Result<Vec<u8>, CatalogError> {
        let name_len = name_len(&self.name)?;
        let mut bytes = Vec::with_capacity(15 + self.name.len());
        bytes.extend(self.index_id.0.to_le_bytes());
        bytes.extend(self.table_id.0.to_le_bytes());
        bytes.extend(self.column.to_le_bytes());
        bytes.extend(self.header_page_id.to_le_bytes());
        bytes.push(name_len);
        bytes.extend(self.name.as_bytes());
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CatalogError> {
        let mut reader = ByteReader::new(bytes);
        let index_id = IndexId(reader.read_u32().map_err(|_| TRUNCATED)?);
        let table_id = TableId(reader.read_u32().map_err(|_| TRUNCATED)?);
        let column = reader.read_u16().map_err(|_| TRUNCATED)?;
        let header_page_id = reader.read_u32().map_err(|_| TRUNCATED)?;
        let name = read_name(&mut reader)?;
        expect_end(&reader)?;
        if header_page_id == RESERVED_PAGE_ID {
            return Err(CatalogError::Corrupted {
                reason: "index header is the reserved page",
            });
        }
        Ok(Self {
            index_id,
            table_id,
            column,
            header_page_id,
            name,
        })
    }
}

fn write_data_type(bytes: &mut Vec<u8>, data_type: DataType) {
    match data_type {
        DataType::SmallInt => bytes.push(1),
        DataType::Integer => bytes.push(2),
        DataType::BigInt => bytes.push(3),
        DataType::Varchar(max_chars) => {
            bytes.push(4);
            bytes.extend(max_chars.get().to_le_bytes());
        }
        DataType::Boolean => bytes.push(5),
        DataType::Real => bytes.push(6),
        DataType::Double => bytes.push(7),
        DataType::Date => bytes.push(8),
        DataType::Timestamp => bytes.push(9),
    }
}

fn read_data_type(reader: &mut ByteReader<'_>) -> Result<DataType, CatalogError> {
    match reader.read_u8().map_err(|_| TRUNCATED)? {
        1 => Ok(DataType::SmallInt),
        2 => Ok(DataType::Integer),
        3 => Ok(DataType::BigInt),
        4 => {
            let max_chars = reader.read_u16().map_err(|_| TRUNCATED)?;
            NonZeroU16::new(max_chars)
                .map(DataType::Varchar)
                .ok_or(CatalogError::Corrupted {
                    reason: "varchar length must be 1 to 65535",
                })
        }
        5 => Ok(DataType::Boolean),
        6 => Ok(DataType::Real),
        7 => Ok(DataType::Double),
        8 => Ok(DataType::Date),
        9 => Ok(DataType::Timestamp),
        _ => Err(CatalogError::Corrupted {
            reason: "unknown column type",
        }),
    }
}

fn nullable(flags: u8) -> Result<bool, CatalogError> {
    if flags & !NULLABLE_FLAG != 0 {
        return Err(CatalogError::Corrupted {
            reason: "unknown column flags",
        });
    }
    Ok(flags == NULLABLE_FLAG)
}

fn name_len(name: &str) -> Result<u8, CatalogError> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(BAD_NAME_LEN);
    }
    u8::try_from(name.len()).map_err(|_| BAD_NAME_LEN)
}

fn read_name(reader: &mut ByteReader<'_>) -> Result<String, CatalogError> {
    let len = usize::from(reader.read_u8().map_err(|_| TRUNCATED)?);
    if len == 0 || len > MAX_NAME_LEN {
        return Err(BAD_NAME_LEN);
    }
    let bytes = reader.read_bytes(len).map_err(|_| TRUNCATED)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| CatalogError::Corrupted {
        reason: "name is not valid UTF-8",
    })
}

fn expect_end(reader: &ByteReader<'_>) -> Result<(), CatalogError> {
    if reader.remaining() != 0 {
        return Err(CatalogError::Corrupted {
            reason: "catalog entry has trailing bytes",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use DataType::{BigInt, Integer, SmallInt};

    fn table(name: &str) -> TableRecord {
        TableRecord {
            table_id: TableId(0x0403_0201),
            first_page_id: 9,
            name: name.to_string(),
        }
    }

    fn column(name: &str, data_type: DataType, nullable: bool) -> ColumnRecord {
        ColumnRecord {
            table_id: TableId(5),
            position: 0x0201,
            column: Column {
                name: name.to_string(),
                data_type,
                nullable,
            },
        }
    }

    fn index(name: &str) -> IndexRecord {
        IndexRecord {
            index_id: IndexId(0x0403_0201),
            table_id: TableId(5),
            column: 0x0201,
            header_page_id: 9,
            name: name.to_string(),
        }
    }

    #[test]
    fn index_record_should_produce_the_documented_bytes() {
        let bytes = index("by_id").encode().unwrap();

        assert_eq!(
            bytes,
            [
                1, 2, 3, 4, 5, 0, 0, 0, 1, 2, 9, 0, 0, 0, 5, b'b', b'y', b'_', b'i', b'd'
            ]
        );
    }

    #[test]
    fn index_records_should_round_trip_and_take_at_most_78_bytes() {
        let longest = "é".repeat(31) + "x";
        for name in ["i", "índice", longest.as_str()] {
            let record = index(name);

            let bytes = record.encode().unwrap();

            assert_eq!(IndexRecord::decode(&bytes).unwrap(), record);
        }
        assert_eq!(index(&longest).encode().unwrap().len(), 78);
    }

    #[test]
    fn index_record_with_a_name_outside_the_limits_should_not_encode() {
        for name in [String::new(), "x".repeat(64)] {
            let err = index(&name).encode().unwrap_err();

            assert_eq!(corrupted(err), "name must have 1 to 63 bytes");
        }
    }

    #[test]
    fn decode_of_a_damaged_index_record_should_fail() {
        let bytes = index("by_id").encode().unwrap();
        let mut trailing = bytes.clone();
        trailing.push(0);
        let mut reserved = bytes.clone();
        reserved[10..14].copy_from_slice(&[0; 4]);
        let mut empty_name = bytes[..14].to_vec();
        empty_name.push(0);
        let mut not_utf8 = bytes.clone();
        not_utf8[15] = 0xFF;

        for len in 0..bytes.len() {
            let err = IndexRecord::decode(&bytes[..len]).unwrap_err();
            assert_eq!(corrupted(err), "catalog entry is truncated", "{len}");
        }
        let cases = [
            (trailing, "catalog entry has trailing bytes"),
            (reserved, "index header is the reserved page"),
            (empty_name, "name must have 1 to 63 bytes"),
            (not_utf8, "name is not valid UTF-8"),
        ];
        for (damaged, reason) in cases {
            assert_eq!(
                corrupted(IndexRecord::decode(&damaged).unwrap_err()),
                reason
            );
        }
    }

    fn corrupted(err: CatalogError) -> &'static str {
        match err {
            CatalogError::Corrupted { reason } => reason,
            other => panic!("expected Corrupted, got {other:?}"),
        }
    }

    #[test]
    fn table_record_should_produce_the_documented_bytes() {
        let bytes = table("users").encode().unwrap();

        assert_eq!(
            bytes,
            [1, 2, 3, 4, 9, 0, 0, 0, 5, b'u', b's', b'e', b'r', b's']
        );
    }

    #[test]
    fn column_record_should_produce_the_documented_bytes() {
        let bytes = column("id", Integer, false).encode().unwrap();
        let nullable = column("id", BigInt, true).encode().unwrap();

        assert_eq!(bytes, [5, 0, 0, 0, 1, 2, 2, 0, 2, b'i', b'd']);
        assert_eq!(nullable[6..8], [3, 1]);
    }

    fn varchar(max_chars: u16) -> DataType {
        DataType::Varchar(std::num::NonZeroU16::new(max_chars).unwrap())
    }

    const ALL_TYPES: [DataType; 10] = [
        DataType::Date,
        DataType::Timestamp,
        SmallInt,
        Integer,
        BigInt,
        DataType::Real,
        DataType::Double,
        DataType::Varchar(std::num::NonZeroU16::MIN),
        DataType::VARCHAR_MAX,
        DataType::Boolean,
    ];

    #[test]
    fn column_record_of_timestamp_should_use_code_9() {
        let bytes = column("at", DataType::Timestamp, false).encode().unwrap();

        assert_eq!(bytes, [5, 0, 0, 0, 1, 2, 9, 0, 2, b'a', b't']);
    }

    #[test]
    fn column_record_of_date_should_use_code_8() {
        let bytes = column("on", DataType::Date, true).encode().unwrap();

        assert_eq!(bytes, [5, 0, 0, 0, 1, 2, 8, 1, 2, b'o', b'n']);
    }

    #[test]
    fn column_records_of_floats_should_use_codes_6_and_7() {
        let real = column("r", DataType::Real, true).encode().unwrap();
        let double = column("d", DataType::Double, true).encode().unwrap();

        assert_eq!(real, [5, 0, 0, 0, 1, 2, 6, 1, 1, b'r']);
        assert_eq!(double, [5, 0, 0, 0, 1, 2, 7, 1, 1, b'd']);
    }

    #[test]
    fn column_record_of_boolean_should_use_code_5() {
        let bytes = column("on", DataType::Boolean, false).encode().unwrap();

        assert_eq!(bytes, [5, 0, 0, 0, 1, 2, 5, 0, 2, b'o', b'n']);
    }

    #[test]
    fn column_record_of_varchar_should_store_its_length() {
        let bytes = column("bio", varchar(0x0102), true).encode().unwrap();

        assert_eq!(bytes, [5, 0, 0, 0, 1, 2, 4, 2, 1, 1, 3, b'b', b'i', b'o']);
    }

    #[test]
    fn the_largest_column_record_should_take_74_bytes() {
        let name = "a".repeat(MAX_NAME_LEN);

        let bytes = column(&name, DataType::VARCHAR_MAX, true).encode().unwrap();

        assert_eq!(bytes.len(), 74);
    }

    #[test]
    fn decode_of_a_varchar_with_length_zero_should_fail() {
        let mut bytes = column("bio", varchar(7), true).encode().unwrap();
        bytes[7..9].copy_from_slice(&[0, 0]);

        assert_eq!(
            corrupted(ColumnRecord::decode(&bytes).unwrap_err()),
            "varchar length must be 1 to 65535"
        );
    }

    fn names() -> Vec<String> {
        vec![
            "a".to_string(),
            "a".repeat(MAX_NAME_LEN),
            format!("{}a", "é".repeat(31)),
        ]
    }

    #[test]
    fn table_records_should_round_trip() {
        for name in names() {
            let record = table(&name);

            assert_eq!(
                TableRecord::decode(&record.encode().unwrap()).unwrap(),
                record
            );
        }
    }

    #[test]
    fn column_records_should_round_trip() {
        for name in names() {
            for data_type in ALL_TYPES {
                for nullable in [true, false] {
                    let record = column(&name, data_type, nullable);

                    assert_eq!(
                        ColumnRecord::decode(&record.encode().unwrap()).unwrap(),
                        record
                    );
                }
            }
        }
    }

    #[test]
    fn encode_of_a_name_outside_the_limits_should_fail() {
        for name in [String::new(), "a".repeat(MAX_NAME_LEN + 1)] {
            let table_err = table(&name).encode().unwrap_err();
            let column_err = column(&name, Integer, true).encode().unwrap_err();

            assert_eq!(corrupted(table_err), "name must have 1 to 63 bytes");
            assert_eq!(corrupted(column_err), "name must have 1 to 63 bytes");
        }
    }

    #[test]
    fn decode_of_a_truncated_entry_should_fail() {
        let table_bytes = table("users").encode().unwrap();

        for len in 0..table_bytes.len() {
            let err = TableRecord::decode(&table_bytes[..len]).unwrap_err();
            assert_eq!(corrupted(err), "catalog entry is truncated", "len {len}");
        }
        for data_type in [Integer, varchar(300)] {
            let column_bytes = column("id", data_type, true).encode().unwrap();
            for len in 0..column_bytes.len() {
                let err = ColumnRecord::decode(&column_bytes[..len]).unwrap_err();
                assert_eq!(corrupted(err), "catalog entry is truncated", "len {len}");
            }
        }
    }

    #[test]
    fn decode_with_trailing_bytes_should_fail() {
        let mut table_bytes = table("users").encode().unwrap();
        let mut column_bytes = column("id", Integer, true).encode().unwrap();
        table_bytes.push(0);
        column_bytes.push(0);

        assert_eq!(
            corrupted(TableRecord::decode(&table_bytes).unwrap_err()),
            "catalog entry has trailing bytes"
        );
        assert_eq!(
            corrupted(ColumnRecord::decode(&column_bytes).unwrap_err()),
            "catalog entry has trailing bytes"
        );
    }

    #[test]
    fn decode_of_an_unknown_type_or_flag_should_fail() {
        let valid = column("id", Integer, true).encode().unwrap();
        let cases = [
            (6, 0, "unknown column type"),
            (6, 10, "unknown column type"),
            (7, 0b10, "unknown column flags"),
            (7, 0xFF, "unknown column flags"),
        ];

        for (offset, byte, reason) in cases {
            let mut bytes = valid.clone();
            bytes[offset] = byte;

            assert_eq!(corrupted(ColumnRecord::decode(&bytes).unwrap_err()), reason);
        }
    }

    #[test]
    fn decode_of_a_name_with_a_bad_length_should_fail() {
        let mut empty = table("x").encode().unwrap();
        empty[8] = 0;
        empty.truncate(9);
        let mut long = table("x").encode().unwrap();
        long[8] = 64;
        long.truncate(9);
        long.extend([b'a'; 64]);

        for bytes in [empty, long] {
            assert_eq!(
                corrupted(TableRecord::decode(&bytes).unwrap_err()),
                "name must have 1 to 63 bytes"
            );
        }
    }

    #[test]
    fn decode_of_a_name_that_is_not_utf8_should_fail() {
        let mut table_bytes = table("ab").encode().unwrap();
        let mut column_bytes = column("ab", Integer, true).encode().unwrap();
        *table_bytes.last_mut().unwrap() = 0xFF;
        *column_bytes.last_mut().unwrap() = 0xFF;

        for err in [
            TableRecord::decode(&table_bytes).unwrap_err(),
            ColumnRecord::decode(&column_bytes).unwrap_err(),
        ] {
            assert_eq!(corrupted(err), "name is not valid UTF-8");
        }
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

        fn name(&mut self) -> String {
            let alphabet = ['a', 'Z', '_', '0', 'é', '名', ' '];
            let mut name = String::new();
            for _ in 0..1 + self.below(30) {
                let next = alphabet[self.below(alphabet.len())];
                if name.len() + next.len_utf8() > MAX_NAME_LEN {
                    break;
                }
                name.push(next);
            }
            name
        }
    }

    fn random_records(rng: &mut Rng) -> (TableRecord, ColumnRecord) {
        let table = TableRecord {
            table_id: TableId(rng.next() as u32),
            first_page_id: rng.next() as u32,
            name: rng.name(),
        };
        let column = ColumnRecord {
            table_id: TableId(rng.next() as u32),
            position: rng.next() as u16,
            column: Column {
                name: rng.name(),
                data_type: match rng.below(4) {
                    0 => ALL_TYPES[rng.below(ALL_TYPES.len())],
                    _ => [
                        SmallInt,
                        Integer,
                        BigInt,
                        varchar((rng.next() as u16).max(1)),
                        DataType::Boolean,
                        DataType::Real,
                        DataType::Double,
                        DataType::Date,
                        DataType::Timestamp,
                    ][rng.below(9)],
                },
                nullable: rng.below(2) == 0,
            },
        };
        (table, column)
    }

    fn random_index_record(rng: &mut Rng) -> IndexRecord {
        IndexRecord {
            index_id: IndexId(rng.next() as u32),
            table_id: TableId(rng.next() as u32),
            column: rng.next() as u16,
            header_page_id: (rng.next() as u32).max(1),
            name: rng.name(),
        }
    }

    #[test]
    fn random_records_should_round_trip() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

        for _ in 0..2_000 {
            let (table, column) = random_records(&mut rng);

            assert_eq!(
                TableRecord::decode(&table.encode().unwrap()).unwrap(),
                table
            );
            assert_eq!(
                ColumnRecord::decode(&column.encode().unwrap()).unwrap(),
                column
            );
            let index = random_index_record(&mut rng);
            assert_eq!(
                IndexRecord::decode(&index.encode().unwrap()).unwrap(),
                index
            );
        }
    }

    #[test]
    fn random_corruption_should_never_panic() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);

        for _ in 0..2_000 {
            let (table, column) = random_records(&mut rng);
            let index = random_index_record(&mut rng);
            for mut bytes in [
                table.encode().unwrap(),
                column.encode().unwrap(),
                index.encode().unwrap(),
            ] {
                if rng.below(3) == 0 {
                    bytes.truncate(rng.below(bytes.len() + 1));
                } else {
                    let position = rng.below(bytes.len());
                    bytes[position] ^= 1 << rng.below(8);
                }

                let _ = TableRecord::decode(&bytes);
                let _ = ColumnRecord::decode(&bytes);
                let _ = IndexRecord::decode(&bytes);
            }
        }
    }
}
