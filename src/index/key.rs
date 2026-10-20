//! Index keys as bytes whose `memcmp` order is the SQL order of the key type, so the B+Tree
//! compares keys without knowing SQL types:
//!
//! | Type | Bytes |
//! |---|---|
//! | `SMALLINT` / `INTEGER` / `BIGINT` | big-endian with the sign bit flipped (2 / 4 / 8 bytes) |
//! | `BOOLEAN` | `0` false, `1` true |
//! | `DATE` | days since 1970-01-01 (`i32`), like `INTEGER` |
//! | `TIMESTAMP` | microseconds since 1970-01-01 (`i64`), like `BIGINT` |
//! | `REAL` / `DOUBLE PRECISION` | IEEE bits, big-endian: positive → sign bit flipped, negative → every bit flipped |
//! | `VARCHAR(n)` | the UTF-8 bytes; the length comes from the node, so there is no terminator |
//!
//! Floats are finite and never `-0.0`, so equal values have equal bytes. Text compares by
//! bytes without padding (`'a' < 'a '`), the order of the rest of ferroDB.

use crate::types::{DataType, Date, Finite32, Finite64, Timestamp, Value};

use super::IndexError;

const WRONG_SIZE: &str = "key has the wrong size for its type";

pub(crate) fn encode_key(key_type: DataType, key: &Value) -> Result<Vec<u8>, IndexError> {
    let bytes = match (key_type, key) {
        (_, Value::Null) => return Err(IndexError::NullKey),
        (DataType::SmallInt, Value::SmallInt(value)) => flip_sign_bit(value.to_be_bytes()),
        (DataType::Integer, Value::Integer(value)) => flip_sign_bit(value.to_be_bytes()),
        (DataType::BigInt, Value::BigInt(value)) => flip_sign_bit(value.to_be_bytes()),
        (DataType::Date, Value::Date(date)) => flip_sign_bit(date.days_since_epoch().to_be_bytes()),
        (DataType::Timestamp, Value::Timestamp(timestamp)) => {
            flip_sign_bit(timestamp.micros_since_epoch().to_be_bytes())
        }
        (DataType::Boolean, Value::Boolean(value)) => vec![u8::from(*value)],
        (DataType::Real, Value::Real(value)) => ordered_f32_bits(value.get().to_bits())
            .to_be_bytes()
            .to_vec(),
        (DataType::Double, Value::Double(value)) => ordered_f64_bits(value.get().to_bits())
            .to_be_bytes()
            .to_vec(),
        (DataType::Varchar(_), Value::Text(text)) => text.as_bytes().to_vec(),
        (expected, key) => {
            return Err(IndexError::KeyTypeMismatch {
                expected,
                found: key.data_type().unwrap_or(expected),
            });
        }
    };
    Ok(bytes)
}

/// The code of a key type in the tree's header page: the numbering of the catalog. For an
/// index every `VARCHAR(n)` is the same type, so the length is not part of it.
pub(crate) fn key_type_code(key_type: DataType) -> u32 {
    match key_type {
        DataType::SmallInt => 1,
        DataType::Integer => 2,
        DataType::BigInt => 3,
        DataType::Varchar(_) => 4,
        DataType::Boolean => 5,
        DataType::Real => 6,
        DataType::Double => 7,
        DataType::Date => 8,
        DataType::Timestamp => 9,
    }
}

/// `None` for a code no type has (a damaged header page).
pub(crate) fn key_type_from_code(code: u32) -> Option<DataType> {
    match code {
        1 => Some(DataType::SmallInt),
        2 => Some(DataType::Integer),
        3 => Some(DataType::BigInt),
        4 => Some(DataType::VARCHAR_MAX),
        5 => Some(DataType::Boolean),
        6 => Some(DataType::Real),
        7 => Some(DataType::Double),
        8 => Some(DataType::Date),
        9 => Some(DataType::Timestamp),
        _ => None,
    }
}

/// Never trusts the bytes: the error is the reason for `IndexError::Corrupted`.
pub(crate) fn decode_key(key_type: DataType, bytes: &[u8]) -> Result<Value, &'static str> {
    match key_type {
        DataType::SmallInt => Ok(Value::SmallInt(i16::from_be_bytes(unflip_sign_bit(bytes)?))),
        DataType::Integer => Ok(Value::Integer(i32::from_be_bytes(unflip_sign_bit(bytes)?))),
        DataType::BigInt => Ok(Value::BigInt(i64::from_be_bytes(unflip_sign_bit(bytes)?))),
        DataType::Date => Date::from_days_since_epoch(i32::from_be_bytes(unflip_sign_bit(bytes)?))
            .map(Value::Date)
            .ok_or("date key out of range"),
        DataType::Timestamp => {
            Timestamp::from_micros_since_epoch(i64::from_be_bytes(unflip_sign_bit(bytes)?))
                .map(Value::Timestamp)
                .ok_or("timestamp key out of range")
        }
        DataType::Boolean => match bytes {
            [0] => Ok(Value::Boolean(false)),
            [1] => Ok(Value::Boolean(true)),
            [_] => Err("boolean key is neither 0 nor 1"),
            _ => Err(WRONG_SIZE),
        },
        DataType::Real => {
            let ordered = u32::from_be_bytes(bytes.try_into().map_err(|_| WRONG_SIZE)?);
            let value = f32::from_bits(original_f32_bits(ordered));
            check_not_negative_zero(value.to_bits() == 1 << 31)?;
            Finite32::new(value)
                .map(Value::Real)
                .ok_or("float key is not finite")
        }
        DataType::Double => {
            let ordered = u64::from_be_bytes(bytes.try_into().map_err(|_| WRONG_SIZE)?);
            let value = f64::from_bits(original_f64_bits(ordered));
            check_not_negative_zero(value.to_bits() == 1 << 63)?;
            Finite64::new(value)
                .map(Value::Double)
                .ok_or("float key is not finite")
        }
        DataType::Varchar(_) => String::from_utf8(bytes.to_vec())
            .map(Value::Text)
            .map_err(|_| "text key is not valid UTF-8"),
    }
}

/// Two's complement puts negative numbers after positive ones in byte order; flipping the
/// sign bit moves them before, keeping the order inside each half.
fn flip_sign_bit<const N: usize>(mut bytes: [u8; N]) -> Vec<u8> {
    if let Some(first) = bytes.first_mut() {
        *first ^= 0x80;
    }
    bytes.to_vec()
}

fn unflip_sign_bit<const N: usize>(bytes: &[u8]) -> Result<[u8; N], &'static str> {
    let mut fixed: [u8; N] = bytes.try_into().map_err(|_| WRONG_SIZE)?;
    if let Some(first) = fixed.first_mut() {
        *first ^= 0x80;
    }
    Ok(fixed)
}

/// IEEE floats order by magnitude with a sign bit in front: positives get the sign bit set
/// (after every negative), negatives get every bit flipped (a larger magnitude sorts first).
fn ordered_f32_bits(bits: u32) -> u32 {
    if bits & (1 << 31) == 0 {
        bits | 1 << 31
    } else {
        !bits
    }
}

fn original_f32_bits(ordered: u32) -> u32 {
    if ordered & (1 << 31) != 0 {
        ordered & !(1 << 31)
    } else {
        !ordered
    }
}

fn ordered_f64_bits(bits: u64) -> u64 {
    if bits & (1 << 63) == 0 {
        bits | 1 << 63
    } else {
        !bits
    }
}

fn original_f64_bits(ordered: u64) -> u64 {
    if ordered & (1 << 63) != 0 {
        ordered & !(1 << 63)
    } else {
        !ordered
    }
}

fn check_not_negative_zero(is_negative_zero: bool) -> Result<(), &'static str> {
    if is_negative_zero {
        return Err("negative zero is never stored");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Date, Finite32, Finite64, Timestamp};
    use std::num::NonZeroU16;

    const VARCHAR_10: DataType = DataType::Varchar(NonZeroU16::new(10).unwrap());

    fn real(value: f32) -> Value {
        Value::Real(Finite32::new(value).unwrap())
    }

    fn double(value: f64) -> Value {
        Value::Double(Finite64::new(value).unwrap())
    }

    fn date(days: i32) -> Value {
        Value::Date(Date::from_days_since_epoch(days).unwrap())
    }

    fn timestamp(micros: i64) -> Value {
        Value::Timestamp(Timestamp::from_micros_since_epoch(micros).unwrap())
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_string())
    }

    fn encoded(key_type: DataType, key: Value) -> Vec<u8> {
        encode_key(key_type, &key).unwrap()
    }

    #[test]
    fn integers_should_be_big_endian_with_the_sign_bit_flipped() {
        assert_eq!(encoded(DataType::BigInt, Value::BigInt(i64::MIN)), [0; 8]);
        assert_eq!(
            encoded(DataType::BigInt, Value::BigInt(-1)),
            [0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
        assert_eq!(
            encoded(DataType::Integer, Value::Integer(0)),
            [0x80, 0, 0, 0]
        );
        assert_eq!(
            encoded(DataType::Integer, Value::Integer(258)),
            [0x80, 0, 1, 2]
        );
        assert_eq!(
            encoded(DataType::SmallInt, Value::SmallInt(i16::MAX)),
            [0xFF, 0xFF]
        );
    }

    #[test]
    fn dates_and_timestamps_should_be_encoded_like_their_integers() {
        assert_eq!(encoded(DataType::Date, date(258)), [0x80, 0, 1, 2]);
        assert_eq!(
            encoded(DataType::Timestamp, timestamp(-1)),
            [0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
    }

    #[test]
    fn booleans_should_be_one_byte() {
        assert_eq!(encoded(DataType::Boolean, Value::Boolean(false)), [0]);
        assert_eq!(encoded(DataType::Boolean, Value::Boolean(true)), [1]);
    }

    #[test]
    fn floats_should_flip_the_sign_bit_or_every_bit() {
        assert_eq!(encoded(DataType::Real, real(0.0)), [0x80, 0, 0, 0]);
        assert_eq!(encoded(DataType::Real, real(1.0)), [0xBF, 0x80, 0, 0]);
        assert_eq!(
            encoded(DataType::Real, real(-1.0)),
            [0x40, 0x7F, 0xFF, 0xFF]
        );
        assert_eq!(
            encoded(DataType::Double, double(-0.0)),
            encoded(DataType::Double, double(0.0))
        );
    }

    #[test]
    fn text_should_be_its_utf8_bytes() {
        assert_eq!(encoded(VARCHAR_10, text("")), b"");
        assert_eq!(encoded(VARCHAR_10, text("aé")), "aé".as_bytes());
    }

    #[test]
    fn key_type_codes_should_round_trip() {
        let types = [
            DataType::SmallInt,
            DataType::Integer,
            DataType::BigInt,
            DataType::VARCHAR_MAX,
            DataType::Boolean,
            DataType::Real,
            DataType::Double,
            DataType::Date,
            DataType::Timestamp,
        ];

        for (code, key_type) in (1..).zip(types) {
            assert_eq!(key_type_code(key_type), code);
            assert_eq!(key_type_from_code(code), Some(key_type));
        }
        assert_eq!(key_type_code(VARCHAR_10), 4);
        assert_eq!(key_type_from_code(0), None);
        assert_eq!(key_type_from_code(10), None);
    }

    #[test]
    fn null_should_not_be_a_key() {
        let err = encode_key(DataType::Integer, &Value::Null).unwrap_err();

        assert!(matches!(err, IndexError::NullKey));
    }

    #[test]
    fn a_key_of_another_type_should_fail() {
        let cases = [
            (DataType::Integer, Value::SmallInt(1)),
            (DataType::BigInt, Value::Integer(1)),
            (VARCHAR_10, Value::Integer(1)),
            (DataType::Date, timestamp(0)),
            (DataType::Double, real(1.0)),
            (DataType::Boolean, text("true")),
        ];

        for (key_type, key) in cases {
            let err = encode_key(key_type, &key).unwrap_err();

            let found = key.data_type().unwrap();
            assert!(
                matches!(err, IndexError::KeyTypeMismatch { expected, found: actual }
                    if expected == key_type && actual == found),
                "{err:?}"
            );
        }
    }

    #[test]
    fn a_text_key_should_ignore_the_varchar_length() {
        let long = "x".repeat(20);

        let bytes = encoded(VARCHAR_10, text(&long));

        assert_eq!(bytes, long.as_bytes());
    }

    fn extremes() -> Vec<(DataType, Value)> {
        vec![
            (DataType::SmallInt, Value::SmallInt(i16::MIN)),
            (DataType::SmallInt, Value::SmallInt(i16::MAX)),
            (DataType::Integer, Value::Integer(i32::MIN)),
            (DataType::Integer, Value::Integer(-1)),
            (DataType::Integer, Value::Integer(i32::MAX)),
            (DataType::BigInt, Value::BigInt(i64::MIN)),
            (DataType::BigInt, Value::BigInt(0)),
            (DataType::BigInt, Value::BigInt(i64::MAX)),
            (DataType::Boolean, Value::Boolean(false)),
            (DataType::Boolean, Value::Boolean(true)),
            (DataType::Date, Value::Date(Date::MIN)),
            (DataType::Date, Value::Date(Date::MAX)),
            (DataType::Timestamp, Value::Timestamp(Timestamp::MIN)),
            (DataType::Timestamp, Value::Timestamp(Timestamp::MAX)),
            (DataType::Real, real(f32::MIN)),
            (DataType::Real, real(-f32::MIN_POSITIVE)),
            (DataType::Real, real(-f32::from_bits(1))),
            (DataType::Real, real(0.0)),
            (DataType::Real, real(f32::from_bits(1))),
            (DataType::Real, real(f32::MAX)),
            (DataType::Double, double(f64::MIN)),
            (DataType::Double, double(-f64::from_bits(1))),
            (DataType::Double, double(0.0)),
            (DataType::Double, double(f64::from_bits(1))),
            (DataType::Double, double(f64::MAX)),
            (VARCHAR_10, text("")),
            (VARCHAR_10, text(" ")),
            (VARCHAR_10, text("a")),
            (VARCHAR_10, text("a ")),
            (VARCHAR_10, text("\u{10348}")),
        ]
    }

    #[test]
    fn decode_should_return_the_encoded_key() {
        for (key_type, key) in extremes() {
            let bytes = encoded(key_type, key.clone());

            assert_eq!(decode_key(key_type, &bytes).unwrap(), key, "{key_type:?}");
        }
    }

    #[test]
    fn byte_order_of_the_extremes_should_be_the_sql_order() {
        let keys = extremes();
        for (key_type, lhs) in &keys {
            for (_, rhs) in keys.iter().filter(|(other, _)| other == key_type) {
                let bytes_order =
                    encoded(*key_type, lhs.clone()).cmp(&encoded(*key_type, rhs.clone()));

                assert_eq!(bytes_order, lhs.sort_cmp(rhs), "{lhs:?} vs {rhs:?}");
            }
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

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }

        fn between(&mut self, low: i64, high: i64) -> i64 {
            low + self.below((high - low) as u64 + 1) as i64
        }

        /// Mostly small numbers, so equal keys and neighbours are frequent.
        fn integer(&mut self) -> i64 {
            match self.below(3) {
                0 => self.between(-3, 3),
                _ => self.next() as i64,
            }
        }

        fn key(&mut self, key_type: DataType) -> Value {
            match key_type {
                DataType::SmallInt => Value::SmallInt(self.integer() as i16),
                DataType::Integer => Value::Integer(self.integer() as i32),
                DataType::BigInt => Value::BigInt(self.integer()),
                DataType::Boolean => Value::Boolean(self.below(2) == 0),
                DataType::Date => date(self.between(
                    Date::MIN.days_since_epoch().into(),
                    Date::MAX.days_since_epoch().into(),
                ) as i32),
                DataType::Timestamp => timestamp(self.between(
                    Timestamp::MIN.micros_since_epoch(),
                    Timestamp::MAX.micros_since_epoch(),
                )),
                DataType::Real => loop {
                    let bits = match self.below(3) {
                        0 => self.below(4) as u32 | (self.below(2) as u32) << 31,
                        _ => self.next() as u32,
                    };
                    if let Some(value) = Finite32::new(f32::from_bits(bits)) {
                        break Value::Real(value);
                    }
                },
                DataType::Double => loop {
                    let bits = match self.below(3) {
                        0 => self.below(4) | self.below(2) << 63,
                        _ => self.next(),
                    };
                    if let Some(value) = Finite64::new(f64::from_bits(bits)) {
                        break Value::Double(value);
                    }
                },
                DataType::Varchar(_) => {
                    const ALPHABET: [char; 8] = ['a', 'b', 'A', ' ', 'é', '€', '\u{10348}', 'z'];
                    let len = self.below(6);
                    text(
                        &(0..len)
                            .map(|_| ALPHABET[self.below(8) as usize])
                            .collect::<String>(),
                    )
                }
            }
        }
    }

    #[test]
    fn byte_order_of_random_keys_should_be_the_sql_order() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let key_types = [
            DataType::SmallInt,
            DataType::Integer,
            DataType::BigInt,
            DataType::Boolean,
            DataType::Date,
            DataType::Timestamp,
            DataType::Real,
            DataType::Double,
            VARCHAR_10,
        ];

        for key_type in key_types {
            for _ in 0..2_000 {
                let (lhs, rhs) = (rng.key(key_type), rng.key(key_type));
                let (lhs_bytes, rhs_bytes) = (
                    encoded(key_type, lhs.clone()),
                    encoded(key_type, rhs.clone()),
                );

                assert_eq!(
                    lhs_bytes.cmp(&rhs_bytes),
                    lhs.sort_cmp(&rhs),
                    "{lhs:?} vs {rhs:?}"
                );
                assert_eq!(decode_key(key_type, &lhs_bytes).unwrap(), lhs);
            }
        }
    }

    #[test]
    fn decode_of_corrupted_bytes_should_fail() {
        let nan = f32::NAN.to_bits() ^ 0x8000_0000;
        let infinity = f64::INFINITY.to_bits() ^ 0x8000_0000_0000_0000;
        let negative_zero = !0x8000_0000_u32;
        let cases: Vec<(DataType, Vec<u8>, &str)> = vec![
            (
                DataType::SmallInt,
                vec![0x80],
                "key has the wrong size for its type",
            ),
            (
                DataType::Integer,
                vec![0x80, 0, 0, 0, 0],
                "key has the wrong size for its type",
            ),
            (
                DataType::BigInt,
                vec![],
                "key has the wrong size for its type",
            ),
            (DataType::Boolean, vec![2], "boolean key is neither 0 nor 1"),
            (
                DataType::Boolean,
                vec![0, 0],
                "key has the wrong size for its type",
            ),
            (
                DataType::Date,
                encoded(DataType::Integer, Value::Integer(i32::MAX)),
                "date key out of range",
            ),
            (
                DataType::Timestamp,
                encoded(DataType::BigInt, Value::BigInt(i64::MIN)),
                "timestamp key out of range",
            ),
            (
                DataType::Real,
                nan.to_be_bytes().to_vec(),
                "float key is not finite",
            ),
            (
                DataType::Double,
                infinity.to_be_bytes().to_vec(),
                "float key is not finite",
            ),
            (
                DataType::Real,
                negative_zero.to_be_bytes().to_vec(),
                "negative zero is never stored",
            ),
            (VARCHAR_10, vec![0xC3], "text key is not valid UTF-8"),
        ];

        for (key_type, bytes, reason) in cases {
            assert_eq!(
                decode_key(key_type, &bytes),
                Err(reason),
                "{key_type:?} {bytes:?}"
            );
        }
    }
}
