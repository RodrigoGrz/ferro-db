//! Runtime values. A stored field is `Option<&[u8]>`, the contract of `TupleLayout::encode`
//! and `TupleRef::field`: `None` is NULL (a bit in the tuple's NULL bitmap). `Some` holds
//! an integer in two's complement, little-endian, `DataType::fixed_width` bytes, or a
//! VARCHAR as its UTF-8 bytes; empty text is `Some(&[])`, never NULL.

use std::cmp::Ordering;
use std::fmt;

use super::{DataType, TypeError, integer, text};

/// `PartialEq` is structural and meant for tests: `Null == Null` and
/// `SmallInt(1) != BigInt(1)`. SQL equality is `compare`, which yields UNKNOWN for NULL.
///
/// NULL carries no type: the type of an expression is decided statically from the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Null,
    SmallInt(i16),
    Integer(i32),
    BigInt(i64),
    /// A VARCHAR value. The limit `n` belongs to the column, not to the value.
    Text(String),
}

impl Value {
    pub fn is_null(&self) -> bool {
        *self == Value::Null
    }

    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Value::Null => None,
            Value::SmallInt(_) => Some(DataType::SmallInt),
            Value::Integer(_) => Some(DataType::Integer),
            Value::BigInt(_) => Some(DataType::BigInt),
            Value::Text(_) => Some(DataType::VARCHAR_MAX),
        }
    }

    pub fn encode(&self) -> Option<Vec<u8>> {
        match self {
            Value::Null => None,
            Value::SmallInt(value) => Some(value.to_le_bytes().to_vec()),
            Value::Integer(value) => Some(value.to_le_bytes().to_vec()),
            Value::BigInt(value) => Some(value.to_le_bytes().to_vec()),
            Value::Text(text) => Some(text.as_bytes().to_vec()),
        }
    }

    /// Like `encode`, but text gives up its bytes instead of copying them.
    pub fn into_encoded(self) -> Option<Vec<u8>> {
        match self {
            Value::Text(text) => Some(text.into_bytes()),
            other => other.encode(),
        }
    }

    pub fn decode(ty: DataType, field: Option<&[u8]>) -> Result<Value, TypeError> {
        let Some(bytes) = field else {
            return Ok(Value::Null);
        };
        Ok(match ty {
            DataType::SmallInt => Value::SmallInt(i16::from_le_bytes(fixed_width(ty, bytes)?)),
            DataType::Integer => Value::Integer(i32::from_le_bytes(fixed_width(ty, bytes)?)),
            DataType::BigInt => Value::BigInt(i64::from_le_bytes(fixed_width(ty, bytes)?)),
            DataType::Varchar(max_chars) => text::decode(max_chars, bytes)?,
        })
    }

    /// Text input. Never yields NULL: `NULL` is a keyword, so the text `'NULL'` is
    /// invalid input for an integer and plain text for a VARCHAR. A VARCHAR follows the
    /// store assignment rule.
    pub fn parse(ty: DataType, input: &str) -> Result<Value, TypeError> {
        match ty {
            DataType::Varchar(max_chars) => text::assign(max_chars, input),
            integer => integer::parse(integer, input),
        }
    }

    /// Store assignment (`INSERT`/`UPDATE`): only within a type family. Text longer than
    /// the target VARCHAR(n) is an error unless the excess is only spaces, which are cut.
    pub fn assign(&self, target: DataType) -> Result<Value, TypeError> {
        let Some(ty) = self.data_type() else {
            return Ok(Value::Null);
        };
        if ty.is_text() != target.is_text() {
            return Err(TypeError::DatatypeMismatch { value: ty, target });
        }
        match (self, target) {
            (Value::Text(text), DataType::Varchar(max_chars)) => text::assign(max_chars, text),
            _ => self.cast(target),
        }
    }

    /// Explicit `CAST`: integers and text convert both ways, and text longer than the
    /// target VARCHAR(n) is cut, whatever the excess holds.
    pub fn cast(&self, target: DataType) -> Result<Value, TypeError> {
        let Some(operand) = self.operand() else {
            return Ok(Value::Null);
        };
        match (operand, target) {
            (Operand::Integer(_, value), DataType::Varchar(max_chars)) => {
                Ok(text::truncate(max_chars, &value.to_string()))
            }
            (Operand::Integer(_, value), integer) => {
                integer::narrow(integer, value).ok_or(TypeError::OutOfRange { ty: integer })
            }
            (Operand::Text(text), DataType::Varchar(max_chars)) => {
                Ok(text::truncate(max_chars, text))
            }
            (Operand::Text(text), integer) => integer::parse(integer, text),
        }
    }

    /// `||`. The result has no column, so it must fit the widest VARCHAR.
    pub fn concat(&self, rhs: &Value) -> Result<Value, TypeError> {
        match (self.operand(), rhs.operand()) {
            (Some(Operand::Text(lhs)), Some(Operand::Text(rhs))) => text::concat(lhs, rhs),
            (Some(lhs), Some(rhs)) => Err(undefined_operator("||", &lhs, &rhs)),
            _ => Ok(Value::Null),
        }
    }

    pub fn add(&self, rhs: &Value) -> Result<Value, TypeError> {
        self.strict_binary(rhs, "+", integer::add)
    }

    pub fn sub(&self, rhs: &Value) -> Result<Value, TypeError> {
        self.strict_binary(rhs, "-", integer::sub)
    }

    pub fn mul(&self, rhs: &Value) -> Result<Value, TypeError> {
        self.strict_binary(rhs, "*", integer::mul)
    }

    pub fn div(&self, rhs: &Value) -> Result<Value, TypeError> {
        self.strict_binary(rhs, "/", integer::div)
    }

    pub fn rem(&self, rhs: &Value) -> Result<Value, TypeError> {
        self.strict_binary(rhs, "%", integer::rem)
    }

    pub fn neg(&self) -> Result<Value, TypeError> {
        match self.operand() {
            None => Ok(Value::Null),
            Some(Operand::Integer(ty, value)) => integer::neg(ty, value),
            Some(text) => Err(TypeError::UndefinedOperator {
                op: "-",
                left: None,
                right: text.data_type(),
            }),
        }
    }

    /// Strict operators: a NULL operand yields NULL before the operation
    /// runs, so `NULL / 0` is NULL rather than an error.
    fn strict_binary(
        &self,
        rhs: &Value,
        op: &'static str,
        operation: fn(DataType, i64, i64) -> Result<Value, TypeError>,
    ) -> Result<Value, TypeError> {
        match (self.operand(), rhs.operand()) {
            (Some(Operand::Integer(lhs_ty, lhs)), Some(Operand::Integer(rhs_ty, rhs))) => {
                operation(lhs_ty.promote(rhs_ty), lhs, rhs)
            }
            (Some(lhs), Some(rhs)) => Err(undefined_operator(op, &lhs, &rhs)),
            _ => Ok(Value::Null),
        }
    }

    /// SQL comparison (`=`, `<`, ...): `Ok(None)` is UNKNOWN, the result whenever a side is
    /// NULL. Text compares by its UTF-8 bytes, without padding (`'a' < 'a '`). Comparing
    /// different families is an error naming `=`, which every comparison derives from.
    pub fn compare(&self, rhs: &Value) -> Result<Option<Ordering>, TypeError> {
        match (self.operand(), rhs.operand()) {
            (Some(lhs), Some(rhs)) => lhs.compare(&rhs, "=").map(Some),
            _ => Ok(None),
        }
    }

    /// `IS DISTINCT FROM`: NULL equals NULL and differs from every value; never UNKNOWN.
    pub fn is_distinct_from(&self, rhs: &Value) -> Result<bool, TypeError> {
        match (self.operand(), rhs.operand()) {
            (Some(lhs), Some(rhs)) => Ok(lhs.compare(&rhs, "IS DISTINCT FROM")?.is_ne()),
            (lhs, rhs) => Ok(lhs.is_some() != rhs.is_some()),
        }
    }

    /// Total order for `ORDER BY` and indexes. `compare` is not one because of UNKNOWN and
    /// of the families it refuses to mix; here integers sort before text and NULL after
    /// every value (`NULLS LAST` for `ASC`).
    pub fn sort_cmp(&self, rhs: &Value) -> Ordering {
        match (self.operand(), rhs.operand()) {
            (Some(lhs), Some(rhs)) => lhs.sort_cmp(&rhs),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    }

    /// `None` is NULL.
    fn operand(&self) -> Option<Operand<'_>> {
        match self {
            Value::Null => None,
            Value::SmallInt(value) => Some(Operand::Integer(DataType::SmallInt, (*value).into())),
            Value::Integer(value) => Some(Operand::Integer(DataType::Integer, (*value).into())),
            Value::BigInt(value) => Some(Operand::Integer(DataType::BigInt, *value)),
            Value::Text(text) => Some(Operand::Text(text)),
        }
    }
}

/// A non-NULL value as the operators see it: every integer widened to `i64`, so one
/// implementation covers all widths, and text borrowed.
enum Operand<'a> {
    Integer(DataType, i64),
    Text(&'a str),
}

impl Operand<'_> {
    fn data_type(&self) -> DataType {
        match self {
            Operand::Integer(ty, _) => *ty,
            Operand::Text(_) => DataType::VARCHAR_MAX,
        }
    }

    fn compare(&self, rhs: &Operand, op: &'static str) -> Result<Ordering, TypeError> {
        if self.data_type().is_text() != rhs.data_type().is_text() {
            return Err(undefined_operator(op, self, rhs));
        }
        Ok(self.sort_cmp(rhs))
    }

    /// `str` orders by its UTF-8 bytes, which is also code point order. A column never
    /// mixes families; putting integers first only makes the order total.
    fn sort_cmp(&self, rhs: &Operand) -> Ordering {
        match (self, rhs) {
            (Operand::Integer(_, lhs), Operand::Integer(_, rhs)) => lhs.cmp(rhs),
            (Operand::Text(lhs), Operand::Text(rhs)) => lhs.cmp(rhs),
            (Operand::Integer(..), Operand::Text(_)) => Ordering::Less,
            (Operand::Text(_), Operand::Integer(..)) => Ordering::Greater,
        }
    }
}

fn undefined_operator(op: &'static str, lhs: &Operand, rhs: &Operand) -> TypeError {
    TypeError::UndefinedOperator {
        op,
        left: Some(lhs.data_type()),
        right: rhs.data_type(),
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::SmallInt(value) => write!(f, "{value}"),
            Value::Integer(value) => write!(f, "{value}"),
            Value::BigInt(value) => write!(f, "{value}"),
            Value::Text(text) => f.write_str(text),
        }
    }
}

fn fixed_width<const N: usize>(ty: DataType, bytes: &[u8]) -> Result<[u8; N], TypeError> {
    bytes.try_into().map_err(|_| TypeError::InvalidEncoding {
        ty,
        expected: ty.fixed_width().unwrap_or_default(),
        actual: bytes.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use DataType::{BigInt, Integer, SmallInt};

    const ALL_TYPES: [DataType; 3] = [SmallInt, Integer, BigInt];

    fn limits() -> Vec<Value> {
        vec![
            Value::SmallInt(i16::MIN),
            Value::SmallInt(-1),
            Value::SmallInt(0),
            Value::SmallInt(1),
            Value::SmallInt(i16::MAX),
            Value::Integer(i32::MIN),
            Value::Integer(-1),
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(i32::MAX),
            Value::BigInt(i64::MIN),
            Value::BigInt(-1),
            Value::BigInt(0),
            Value::BigInt(1),
            Value::BigInt(i64::MAX),
        ]
    }

    #[test]
    fn is_null_should_be_true_only_for_null() {
        assert!(Value::Null.is_null());
        assert!(limits().iter().all(|value| !value.is_null()));
    }

    #[test]
    fn data_type_should_follow_the_variant() {
        assert_eq!(Value::Null.data_type(), None);
        assert_eq!(Value::SmallInt(1).data_type(), Some(SmallInt));
        assert_eq!(Value::Integer(1).data_type(), Some(Integer));
        assert_eq!(Value::BigInt(1).data_type(), Some(BigInt));
    }

    #[test]
    fn encode_should_produce_little_endian_twos_complement() {
        assert_eq!(Value::SmallInt(-2).encode().unwrap(), [0xFE, 0xFF]);
        assert_eq!(Value::Integer(1).encode().unwrap(), [1, 0, 0, 0]);
        assert_eq!(
            Value::BigInt(i64::MIN).encode().unwrap(),
            [0, 0, 0, 0, 0, 0, 0, 0x80]
        );
        assert_eq!(
            Value::Integer(i32::MAX).encode().unwrap(),
            [0xFF, 0xFF, 0xFF, 0x7F]
        );
    }

    #[test]
    fn encode_of_null_should_be_none() {
        assert_eq!(Value::Null.encode(), None);
    }

    #[test]
    fn encode_should_match_the_type_layout() {
        for value in limits() {
            let ty = value.data_type().unwrap();

            assert_eq!(value.encode().unwrap().len(), ty.fixed_width().unwrap());
        }
    }

    #[test]
    fn decode_should_round_trip_the_limits() {
        for value in limits() {
            let ty = value.data_type().unwrap();

            let decoded = Value::decode(ty, value.encode().as_deref()).unwrap();

            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn decode_of_missing_field_should_be_null_for_every_type() {
        for ty in ALL_TYPES {
            assert_eq!(Value::decode(ty, None).unwrap(), Value::Null);
        }
    }

    #[test]
    fn decode_with_wrong_width_should_fail() {
        for ty in ALL_TYPES {
            for actual in [
                0,
                ty.fixed_width().unwrap() - 1,
                ty.fixed_width().unwrap() + 1,
            ] {
                let bytes = vec![0; actual];

                let err = Value::decode(ty, Some(&bytes)).unwrap_err();

                assert_eq!(
                    err,
                    TypeError::InvalidEncoding {
                        ty,
                        expected: ty.fixed_width().unwrap(),
                        actual
                    }
                );
            }
        }
    }

    fn invalid_text(ty: DataType, input: &str) -> TypeError {
        TypeError::InvalidTextRepresentation {
            ty,
            input: input.to_string(),
        }
    }

    fn text_out_of_range(ty: DataType, input: &str) -> TypeError {
        TypeError::TextOutOfRange {
            ty,
            input: input.to_string(),
        }
    }

    #[test]
    fn parse_should_read_plain_decimal_numbers() {
        assert_eq!(Value::parse(Integer, "0").unwrap(), Value::Integer(0));
        assert_eq!(Value::parse(Integer, "42").unwrap(), Value::Integer(42));
        assert_eq!(Value::parse(SmallInt, "-7").unwrap(), Value::SmallInt(-7));
        assert_eq!(Value::parse(BigInt, "+7").unwrap(), Value::BigInt(7));
    }

    #[test]
    fn parse_should_treat_minus_zero_as_zero() {
        assert_eq!(Value::parse(Integer, "-0").unwrap(), Value::Integer(0));
    }

    #[test]
    fn parse_should_accept_leading_zeros() {
        assert_eq!(Value::parse(SmallInt, "007").unwrap(), Value::SmallInt(7));
    }

    #[test]
    fn parse_should_ignore_surrounding_whitespace() {
        assert_eq!(Value::parse(Integer, "  42  ").unwrap(), Value::Integer(42));
        assert_eq!(
            Value::parse(Integer, "\t\n\r\x0B\x0C-1\n").unwrap(),
            Value::Integer(-1)
        );
    }

    #[test]
    fn parse_should_accept_the_limits_of_each_type() {
        for value in limits() {
            let ty = value.data_type().unwrap();

            assert_eq!(Value::parse(ty, &value.to_string()).unwrap(), value);
        }
    }

    #[test]
    fn parse_one_past_the_limits_should_be_out_of_range() {
        let cases = [
            (SmallInt, "32768"),
            (SmallInt, "-32769"),
            (Integer, "2147483648"),
            (Integer, "-2147483649"),
            (BigInt, "9223372036854775808"),
            (BigInt, "-9223372036854775809"),
            (BigInt, "99999999999999999999999999"),
        ];

        for (ty, text) in cases {
            assert_eq!(
                Value::parse(ty, text).unwrap_err(),
                text_out_of_range(ty, text)
            );
        }
    }

    #[test]
    fn parse_of_malformed_text_should_fail() {
        let cases = [
            "", " ", "-", "+", "+-1", "--1", "1a", "a1", "1 2", "1.0", "1e3", "0x10", "1_000",
            "NULL", "١٢", "- 1",
        ];

        for text in cases {
            assert_eq!(
                Value::parse(Integer, text).unwrap_err(),
                invalid_text(Integer, text),
                "{text:?}"
            );
        }
    }

    #[test]
    fn display_should_print_decimal_or_null() {
        assert_eq!(Value::SmallInt(-42).to_string(), "-42");
        assert_eq!(Value::Integer(0).to_string(), "0");
        assert_eq!(Value::BigInt(i64::MIN).to_string(), "-9223372036854775808");
        assert_eq!(Value::Null.to_string(), "NULL");
    }

    fn varchar(max_chars: u16) -> DataType {
        DataType::Varchar(std::num::NonZeroU16::new(max_chars).unwrap())
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_string())
    }

    fn too_long(max_chars: u16, chars: usize) -> TypeError {
        TypeError::StringTooLong {
            ty: varchar(max_chars),
            chars,
        }
    }

    fn corrupted(max_chars: u16, reason: &'static str) -> TypeError {
        TypeError::CorruptedText {
            ty: varchar(max_chars),
            reason,
        }
    }

    #[test]
    fn text_should_have_the_widest_varchar_type() {
        assert_eq!(text("abc").data_type(), Some(DataType::VARCHAR_MAX));
        assert!(!text("").is_null());
    }

    #[test]
    fn encode_of_text_should_be_its_utf8_bytes() {
        assert_eq!(text("abc").encode().unwrap(), b"abc");
        assert_eq!(text("ação").encode().unwrap(), "ação".as_bytes());
        assert_eq!(text("🦀").encode().unwrap(), [0xF0, 0x9F, 0xA6, 0x80]);
    }

    #[test]
    fn into_encoded_should_match_encode() {
        let mut values = limits();
        values.extend([Value::Null, text(""), text("ação 🦀")]);

        for value in values {
            assert_eq!(value.clone().into_encoded(), value.encode(), "{value:?}");
        }
    }

    #[test]
    fn encode_of_empty_text_should_be_an_empty_field_not_null() {
        assert_eq!(text("").encode(), Some(Vec::new()));
    }

    #[test]
    fn decode_should_round_trip_text() {
        let cases = [
            (varchar(1), ""),
            (varchar(10), "abc"),
            (varchar(4), "ação"),
            (varchar(2), "🦀🦀"),
            (varchar(3), "a b"),
            (DataType::VARCHAR_MAX, "NULL"),
        ];

        for (ty, value) in cases {
            let decoded = Value::decode(ty, text(value).encode().as_deref()).unwrap();

            assert_eq!(decoded, text(value), "{ty} {value:?}");
        }
    }

    #[test]
    fn decode_of_invalid_utf8_should_be_corrupted_text() {
        for bytes in [
            &[0xFF][..],
            &[0xC3],
            &[b'a', 0xF0, 0x9F, 0xA6],
            &[0xED, 0xA0, 0x80],
        ] {
            let err = Value::decode(varchar(10), Some(bytes)).unwrap_err();

            assert_eq!(err, corrupted(10, "text is not valid UTF-8"), "{bytes:?}");
        }
    }

    #[test]
    fn decode_of_text_longer_than_the_column_should_be_corrupted_text() {
        let err = Value::decode(varchar(3), Some("ação".as_bytes())).unwrap_err();

        assert_eq!(err, corrupted(3, "text is longer than its column"));
    }

    #[test]
    fn parse_of_varchar_should_accept_up_to_n_characters() {
        assert_eq!(Value::parse(varchar(3), "").unwrap(), text(""));
        assert_eq!(Value::parse(varchar(3), "abc").unwrap(), text("abc"));
        assert_eq!(Value::parse(varchar(4), "ação").unwrap(), text("ação"));
        assert_eq!(Value::parse(varchar(1), "🦀").unwrap(), text("🦀"));
    }

    #[test]
    fn parse_of_varchar_should_keep_the_text_as_is() {
        let value = "  NULL\t 42 ";

        assert_eq!(Value::parse(varchar(20), value).unwrap(), text(value));
    }

    #[test]
    fn parse_of_varchar_past_n_characters_should_be_too_long() {
        assert_eq!(
            Value::parse(varchar(3), "abcd").unwrap_err(),
            too_long(3, 4)
        );
        assert_eq!(
            Value::parse(varchar(3), "ação").unwrap_err(),
            too_long(3, 4)
        );
        assert_eq!(
            Value::parse(varchar(1), "🦀🦀").unwrap_err(),
            too_long(1, 2)
        );
    }

    #[test]
    fn parse_of_varchar_should_cut_an_excess_made_only_of_spaces() {
        assert_eq!(Value::parse(varchar(3), "abc   ").unwrap(), text("abc"));
        assert_eq!(Value::parse(varchar(3), "a    ").unwrap(), text("a  "));
        assert_eq!(Value::parse(varchar(1), "    ").unwrap(), text(" "));
    }

    #[test]
    fn parse_of_varchar_should_reject_an_excess_with_other_characters() {
        assert_eq!(
            Value::parse(varchar(3), "abc d").unwrap_err(),
            too_long(3, 5)
        );
        assert_eq!(
            Value::parse(varchar(3), "abc\t").unwrap_err(),
            too_long(3, 4)
        );
        assert_eq!(
            Value::parse(varchar(3), "abc\u{A0}").unwrap_err(),
            too_long(3, 4)
        );
    }

    #[test]
    fn cast_of_an_integer_to_varchar_should_be_its_decimal_text() {
        assert_eq!(Value::Integer(42).cast(varchar(10)).unwrap(), text("42"));
        assert_eq!(
            Value::BigInt(i64::MIN).cast(DataType::VARCHAR_MAX).unwrap(),
            text("-9223372036854775808")
        );
        assert_eq!(Value::SmallInt(-7).cast(varchar(2)).unwrap(), text("-7"));
    }

    #[test]
    fn cast_of_an_integer_to_a_short_varchar_should_truncate() {
        assert_eq!(
            Value::Integer(-12345).cast(varchar(3)).unwrap(),
            text("-12")
        );
    }

    #[test]
    fn cast_of_text_to_an_integer_should_parse_it() {
        assert_eq!(text(" 42 ").cast(Integer).unwrap(), Value::Integer(42));
        assert_eq!(text("-7").cast(SmallInt).unwrap(), Value::SmallInt(-7));
        assert_eq!(
            text("abc").cast(BigInt).unwrap_err(),
            invalid_text(BigInt, "abc")
        );
        assert_eq!(
            text("40000").cast(SmallInt).unwrap_err(),
            text_out_of_range(SmallInt, "40000")
        );
    }

    #[test]
    fn cast_of_text_to_varchar_should_truncate_to_n_characters() {
        let cases = [
            ("abcdef", 3, "abc"),
            ("ação", 2, "aç"),
            ("🦀🦀", 1, "🦀"),
            ("ab", 3, "ab"),
            ("abc   ", 4, "abc "),
            ("", 1, ""),
        ];

        for (value, max_chars, expected) in cases {
            assert_eq!(
                text(value).cast(varchar(max_chars)).unwrap(),
                text(expected),
                "{value:?} as varchar({max_chars})"
            );
        }
    }

    #[test]
    fn cast_of_null_should_be_null_for_varchar() {
        assert_eq!(Value::Null.cast(varchar(1)).unwrap(), Value::Null);
    }

    #[test]
    fn assign_between_integers_should_behave_like_cast() {
        for value in limits() {
            for target in ALL_TYPES {
                assert_eq!(
                    value.assign(target),
                    value.cast(target),
                    "{value:?} {target}"
                );
            }
        }
    }

    #[test]
    fn assign_of_text_to_varchar_should_follow_the_store_assignment_rule() {
        assert_eq!(text("ação").assign(varchar(4)).unwrap(), text("ação"));
        assert_eq!(text("abc  ").assign(varchar(3)).unwrap(), text("abc"));
        assert_eq!(text("abcd").assign(varchar(3)).unwrap_err(), too_long(3, 4));
    }

    #[test]
    fn assign_across_families_should_be_a_datatype_mismatch() {
        assert_eq!(
            Value::Integer(1).assign(varchar(10)).unwrap_err(),
            TypeError::DatatypeMismatch {
                value: Integer,
                target: varchar(10)
            }
        );
        assert_eq!(
            text("1").assign(SmallInt).unwrap_err(),
            TypeError::DatatypeMismatch {
                value: DataType::VARCHAR_MAX,
                target: SmallInt
            }
        );
    }

    #[test]
    fn assign_of_null_should_be_null_for_every_type() {
        for ty in [SmallInt, Integer, BigInt, varchar(1)] {
            assert_eq!(Value::Null.assign(ty).unwrap(), Value::Null);
        }
    }

    fn undefined(op: &'static str, left: Option<DataType>, right: DataType) -> TypeError {
        TypeError::UndefinedOperator { op, left, right }
    }

    #[test]
    fn concat_should_join_texts() {
        assert_eq!(text("ab").concat(&text("cd")).unwrap(), text("abcd"));
        assert_eq!(text("").concat(&text("x")).unwrap(), text("x"));
        assert_eq!(text("ação").concat(&text("🦀")).unwrap(), text("ação🦀"));
        assert_eq!(text("a ").concat(&text("")).unwrap(), text("a "));
    }

    #[test]
    fn concat_with_null_should_be_null() {
        assert_eq!(text("a").concat(&Value::Null).unwrap(), Value::Null);
        assert_eq!(Value::Null.concat(&text("a")).unwrap(), Value::Null);
        assert_eq!(Value::Null.concat(&Value::Null).unwrap(), Value::Null);
    }

    #[test]
    fn concat_of_null_with_an_integer_should_be_null_before_the_type_check() {
        assert_eq!(Value::Null.concat(&Value::Integer(1)).unwrap(), Value::Null);
        assert_eq!(Value::BigInt(1).concat(&Value::Null).unwrap(), Value::Null);
    }

    #[test]
    fn concat_with_an_integer_should_be_an_undefined_operator() {
        let text_type = Some(DataType::VARCHAR_MAX);

        assert_eq!(
            text("a").concat(&Value::Integer(1)).unwrap_err(),
            undefined("||", text_type, Integer)
        );
        assert_eq!(
            Value::SmallInt(1).concat(&text("a")).unwrap_err(),
            undefined("||", Some(SmallInt), DataType::VARCHAR_MAX)
        );
        assert_eq!(
            Value::BigInt(1).concat(&Value::BigInt(2)).unwrap_err(),
            undefined("||", Some(BigInt), BigInt)
        );
    }

    #[test]
    fn concat_up_to_the_widest_varchar_should_work() {
        let widest = "é".repeat(usize::from(u16::MAX) - 1);

        let joined = text(&widest).concat(&text("🦀")).unwrap();

        assert_eq!(joined, text(&format!("{widest}🦀")));
    }

    #[test]
    fn concat_past_the_widest_varchar_should_be_too_long() {
        let widest = text(&"a".repeat(usize::from(u16::MAX)));

        assert_eq!(
            widest.concat(&text("b")).unwrap_err(),
            too_long(u16::MAX, usize::from(u16::MAX) + 1)
        );
        assert_eq!(widest.concat(&text("  ")).unwrap(), widest);
    }

    #[test]
    fn arithmetic_with_text_should_be_an_undefined_operator() {
        let (one, digit) = (Value::Integer(1), text("1"));
        let text_type = DataType::VARCHAR_MAX;

        for (name, op) in BINARY_OPS {
            assert_eq!(
                op(&digit, &one).unwrap_err(),
                undefined(name, Some(text_type), Integer)
            );
            assert_eq!(
                op(&one, &digit).unwrap_err(),
                undefined(name, Some(Integer), text_type)
            );
            assert_eq!(
                op(&digit, &digit).unwrap_err(),
                undefined(name, Some(text_type), text_type)
            );
        }
        assert_eq!(digit.neg().unwrap_err(), undefined("-", None, text_type));
    }

    #[test]
    fn arithmetic_with_null_and_text_should_be_null() {
        for (_, op) in BINARY_OPS {
            assert_eq!(op(&Value::Null, &text("1")).unwrap(), Value::Null);
            assert_eq!(op(&text("1"), &Value::Null).unwrap(), Value::Null);
        }
    }

    #[test]
    fn compare_of_text_should_follow_the_utf8_bytes_without_padding() {
        let cases = [
            ("a", "a", Ordering::Equal),
            ("", "a", Ordering::Less),
            ("Z", "a", Ordering::Less),
            ("a", "a ", Ordering::Less),
            ("é", "z", Ordering::Greater),
            ("ábaco", "banana", Ordering::Greater),
            ("🦀", "\u{FFFF}", Ordering::Greater),
            ("10", "9", Ordering::Less),
        ];

        for (lhs, rhs, expected) in cases {
            assert_eq!(
                text(lhs).compare(&text(rhs)),
                Ok(Some(expected)),
                "{lhs:?} vs {rhs:?}"
            );
            assert_eq!(text(rhs).compare(&text(lhs)), Ok(Some(expected.reverse())));
        }
    }

    #[test]
    fn compare_of_text_with_null_should_be_unknown() {
        assert_eq!(text("a").compare(&Value::Null), Ok(None));
        assert_eq!(Value::Null.compare(&text("")), Ok(None));
    }

    #[test]
    fn compare_across_families_should_be_an_undefined_operator() {
        assert_eq!(
            text("1").compare(&Value::Integer(1)),
            Err(undefined("=", Some(DataType::VARCHAR_MAX), Integer))
        );
        assert_eq!(
            Value::BigInt(1).compare(&text("1")),
            Err(undefined("=", Some(BigInt), DataType::VARCHAR_MAX))
        );
    }

    #[test]
    fn is_distinct_from_should_compare_text() {
        let cases = [
            (text("a"), text("a"), false),
            (text("a"), text("a "), true),
            (text(""), Value::Null, true),
            (Value::Null, text("a"), true),
        ];

        for (lhs, rhs, is_distinct) in cases {
            assert_eq!(
                lhs.is_distinct_from(&rhs),
                Ok(is_distinct),
                "{lhs:?}, {rhs:?}"
            );
        }
    }

    #[test]
    fn is_distinct_from_across_families_should_be_an_undefined_operator() {
        assert_eq!(
            text("1").is_distinct_from(&Value::Integer(1)),
            Err(undefined(
                "IS DISTINCT FROM",
                Some(DataType::VARCHAR_MAX),
                Integer
            ))
        );
    }

    #[test]
    fn sort_cmp_should_put_integers_before_text_and_null_last() {
        let mut values = vec![
            Value::Null,
            text("b"),
            Value::BigInt(i64::MAX),
            text(""),
            Value::SmallInt(-1),
            text("B"),
        ];

        values.sort_by(Value::sort_cmp);

        assert_eq!(
            values,
            [
                Value::SmallInt(-1),
                Value::BigInt(i64::MAX),
                text(""),
                text("B"),
                text("b"),
                Value::Null,
            ]
        );
    }

    #[test]
    fn display_of_text_should_print_it_unquoted() {
        assert_eq!(text("ação 🦀").to_string(), "ação 🦀");
        assert_eq!(text("").to_string(), "");
    }

    #[test]
    fn cast_to_a_wider_type_should_always_work() {
        assert_eq!(
            Value::SmallInt(i16::MIN).cast(Integer).unwrap(),
            Value::Integer(i16::MIN.into())
        );
        assert_eq!(
            Value::SmallInt(i16::MAX).cast(BigInt).unwrap(),
            Value::BigInt(i16::MAX.into())
        );
        assert_eq!(
            Value::Integer(i32::MIN).cast(BigInt).unwrap(),
            Value::BigInt(i32::MIN.into())
        );
    }

    #[test]
    fn cast_to_the_same_type_should_be_identity() {
        for value in limits() {
            assert_eq!(value.cast(value.data_type().unwrap()).unwrap(), value);
        }
    }

    #[test]
    fn cast_to_a_narrower_type_should_keep_values_at_the_limit() {
        assert_eq!(
            Value::BigInt(i32::MAX.into()).cast(Integer).unwrap(),
            Value::Integer(i32::MAX)
        );
        assert_eq!(
            Value::BigInt(i16::MIN.into()).cast(SmallInt).unwrap(),
            Value::SmallInt(i16::MIN)
        );
        assert_eq!(
            Value::Integer(-5).cast(SmallInt).unwrap(),
            Value::SmallInt(-5)
        );
    }

    #[test]
    fn cast_to_a_narrower_type_one_past_the_limit_should_be_out_of_range() {
        let cases = [
            (Value::Integer(i32::from(i16::MAX) + 1), SmallInt),
            (Value::Integer(i32::from(i16::MIN) - 1), SmallInt),
            (Value::BigInt(i64::from(i32::MAX) + 1), Integer),
            (Value::BigInt(i64::from(i32::MIN) - 1), Integer),
            (Value::BigInt(i64::MAX), SmallInt),
        ];

        for (value, target) in cases {
            assert_eq!(
                value.cast(target).unwrap_err(),
                TypeError::OutOfRange { ty: target }
            );
        }
    }

    #[test]
    fn cast_of_null_should_be_null_for_every_type() {
        for ty in ALL_TYPES {
            assert_eq!(Value::Null.cast(ty).unwrap(), Value::Null);
        }
    }

    type BinaryOp = fn(&Value, &Value) -> Result<Value, TypeError>;

    const BINARY_OPS: [(&str, BinaryOp); 5] = [
        ("+", Value::add),
        ("-", Value::sub),
        ("*", Value::mul),
        ("/", Value::div),
        ("%", Value::rem),
    ];

    fn out_of_range(ty: DataType) -> TypeError {
        TypeError::OutOfRange { ty }
    }

    #[test]
    fn arithmetic_should_compute_common_cases() {
        let (seven, two) = (Value::Integer(7), Value::Integer(2));

        assert_eq!(seven.add(&two).unwrap(), Value::Integer(9));
        assert_eq!(seven.sub(&two).unwrap(), Value::Integer(5));
        assert_eq!(seven.mul(&two).unwrap(), Value::Integer(14));
        assert_eq!(seven.div(&two).unwrap(), Value::Integer(3));
        assert_eq!(seven.rem(&two).unwrap(), Value::Integer(1));
        assert_eq!(seven.neg().unwrap(), Value::Integer(-7));
    }

    #[test]
    fn arithmetic_result_should_have_the_wider_operand_type() {
        let small = Value::SmallInt(3);
        let int = Value::Integer(3);
        let big = Value::BigInt(3);

        assert_eq!(small.add(&small).unwrap(), Value::SmallInt(6));
        assert_eq!(small.add(&int).unwrap(), Value::Integer(6));
        assert_eq!(int.mul(&small).unwrap(), Value::Integer(9));
        assert_eq!(small.sub(&big).unwrap(), Value::BigInt(0));
        assert_eq!(big.div(&int).unwrap(), Value::BigInt(1));
    }

    #[test]
    fn smallint_arithmetic_should_overflow_at_smallint_even_if_integer_would_fit() {
        let err = Value::SmallInt(i16::MAX)
            .add(&Value::SmallInt(1))
            .unwrap_err();

        assert_eq!(err, out_of_range(SmallInt));
    }

    #[test]
    fn arithmetic_past_the_limits_should_be_out_of_range() {
        let cases: [(Value, BinaryOp, Value); 9] = [
            (Value::SmallInt(i16::MIN), Value::sub, Value::SmallInt(1)),
            (Value::SmallInt(i16::MAX), Value::mul, Value::SmallInt(2)),
            (Value::Integer(i32::MAX), Value::add, Value::Integer(1)),
            (Value::Integer(i32::MIN), Value::sub, Value::SmallInt(1)),
            (
                Value::Integer(i32::MAX),
                Value::mul,
                Value::Integer(i32::MAX),
            ),
            (Value::Integer(i32::MIN), Value::div, Value::Integer(-1)),
            (Value::BigInt(i64::MAX), Value::add, Value::SmallInt(1)),
            (Value::BigInt(i64::MIN), Value::mul, Value::BigInt(-1)),
            (Value::BigInt(i64::MIN), Value::div, Value::Integer(-1)),
        ];

        for (lhs, op, rhs) in cases {
            let ty = lhs.data_type().unwrap().promote(rhs.data_type().unwrap());

            assert_eq!(
                op(&lhs, &rhs).unwrap_err(),
                out_of_range(ty),
                "{lhs:?}, {rhs:?}"
            );
        }
    }

    #[test]
    fn neg_of_the_minimum_should_be_out_of_range() {
        let cases = [
            (Value::SmallInt(i16::MIN), SmallInt),
            (Value::Integer(i32::MIN), Integer),
            (Value::BigInt(i64::MIN), BigInt),
        ];

        for (value, ty) in cases {
            assert_eq!(value.neg().unwrap_err(), out_of_range(ty));
        }
    }

    #[test]
    fn neg_of_the_maximum_should_work() {
        assert_eq!(
            Value::BigInt(i64::MAX).neg().unwrap(),
            Value::BigInt(-i64::MAX)
        );
    }

    #[test]
    fn division_should_truncate_toward_zero() {
        assert_eq!(
            Value::Integer(7).div(&Value::Integer(-2)).unwrap(),
            Value::Integer(-3)
        );
        assert_eq!(
            Value::Integer(-7).div(&Value::Integer(2)).unwrap(),
            Value::Integer(-3)
        );
    }

    #[test]
    fn remainder_should_have_the_sign_of_the_dividend() {
        assert_eq!(
            Value::Integer(-7).rem(&Value::Integer(2)).unwrap(),
            Value::Integer(-1)
        );
        assert_eq!(
            Value::Integer(7).rem(&Value::Integer(-2)).unwrap(),
            Value::Integer(1)
        );
    }

    #[test]
    fn remainder_of_the_minimum_by_minus_one_should_be_zero() {
        for value in [
            Value::SmallInt(i16::MIN),
            Value::Integer(i32::MIN),
            Value::BigInt(i64::MIN),
        ] {
            let ty = value.data_type().unwrap();
            let minus_one = Value::BigInt(-1).cast(ty).unwrap();

            let result = value.rem(&minus_one).unwrap();

            assert_eq!(result, Value::BigInt(0).cast(ty).unwrap());
        }
    }

    #[test]
    fn division_and_remainder_by_zero_should_fail() {
        for zero in [Value::SmallInt(0), Value::Integer(0), Value::BigInt(0)] {
            assert_eq!(
                Value::Integer(1).div(&zero).unwrap_err(),
                TypeError::DivisionByZero
            );
            assert_eq!(
                Value::Integer(0).rem(&zero).unwrap_err(),
                TypeError::DivisionByZero
            );
        }
    }

    #[test]
    fn arithmetic_with_null_should_be_null() {
        let one = Value::Integer(1);

        for (name, op) in BINARY_OPS {
            assert_eq!(
                op(&Value::Null, &one).unwrap(),
                Value::Null,
                "NULL {name} 1"
            );
            assert_eq!(
                op(&one, &Value::Null).unwrap(),
                Value::Null,
                "1 {name} NULL"
            );
            assert_eq!(op(&Value::Null, &Value::Null).unwrap(), Value::Null);
        }
        assert_eq!(Value::Null.neg().unwrap(), Value::Null);
    }

    #[test]
    fn null_should_win_over_division_by_zero() {
        let zero = Value::Integer(0);

        assert_eq!(Value::Null.div(&zero).unwrap(), Value::Null);
        assert_eq!(Value::Null.rem(&zero).unwrap(), Value::Null);
        assert_eq!(zero.div(&Value::Null).unwrap(), Value::Null);
    }

    #[test]
    fn compare_should_order_values_of_the_same_type() {
        let (one, two) = (Value::Integer(1), Value::Integer(2));

        assert_eq!(one.compare(&two), Ok(Some(Ordering::Less)));
        assert_eq!(two.compare(&one), Ok(Some(Ordering::Greater)));
        assert_eq!(one.compare(&one), Ok(Some(Ordering::Equal)));
    }

    #[test]
    fn compare_should_use_the_numeric_value_across_widths() {
        assert_eq!(
            Value::SmallInt(-1).compare(&Value::BigInt(0)),
            Ok(Some(Ordering::Less))
        );
        assert_eq!(
            Value::Integer(5).compare(&Value::BigInt(5)),
            Ok(Some(Ordering::Equal))
        );
        assert_eq!(
            Value::SmallInt(i16::MAX).compare(&Value::Integer(i32::MIN)),
            Ok(Some(Ordering::Greater))
        );
        assert_eq!(
            Value::BigInt(i64::MIN).compare(&Value::SmallInt(i16::MIN)),
            Ok(Some(Ordering::Less))
        );
    }

    #[test]
    fn compare_with_null_should_be_unknown() {
        let one = Value::Integer(1);

        assert_eq!(one.compare(&Value::Null), Ok(None));
        assert_eq!(Value::Null.compare(&one), Ok(None));
        assert_eq!(Value::Null.compare(&Value::Null), Ok(None));
    }

    #[test]
    fn is_distinct_from_should_treat_null_as_a_comparable_value() {
        let cases = [
            (Value::Integer(1), Value::Integer(1), false),
            (Value::Integer(1), Value::Integer(2), true),
            (Value::SmallInt(7), Value::BigInt(7), false),
            (Value::Integer(1), Value::Null, true),
            (Value::Null, Value::Integer(1), true),
            (Value::Null, Value::Null, false),
        ];

        for (lhs, rhs, is_distinct) in cases {
            assert_eq!(
                lhs.is_distinct_from(&rhs),
                Ok(is_distinct),
                "{lhs:?}, {rhs:?}"
            );
        }
    }

    #[test]
    fn sort_cmp_should_place_null_after_every_value() {
        assert_eq!(
            Value::Null.sort_cmp(&Value::BigInt(i64::MAX)),
            Ordering::Greater
        );
        assert_eq!(
            Value::SmallInt(i16::MIN).sort_cmp(&Value::Null),
            Ordering::Less
        );
        assert_eq!(Value::Null.sort_cmp(&Value::Null), Ordering::Equal);
    }

    #[test]
    fn sort_cmp_should_agree_with_compare_for_non_null_values() {
        for lhs in limits() {
            for rhs in limits() {
                assert_eq!(Ok(Some(lhs.sort_cmp(&rhs))), lhs.compare(&rhs));
            }
        }
    }

    #[test]
    fn sorting_should_put_nulls_last() {
        let mut values = vec![
            Value::Null,
            Value::Integer(3),
            Value::SmallInt(-1),
            Value::Null,
            Value::BigInt(2),
        ];

        values.sort_by(Value::sort_cmp);

        assert_eq!(
            values,
            [
                Value::SmallInt(-1),
                Value::BigInt(2),
                Value::Integer(3),
                Value::Null,
                Value::Null
            ]
        );
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

    /// Biased toward the limits, where overflow bugs live; about 10% NULL.
    fn random_value(rng: &mut Rng) -> Value {
        if rng.below(10) == 0 {
            return Value::Null;
        }
        let raw = match rng.below(3) {
            0 => [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX][rng.below(7)],
            1 => rng.below(21) as i64 - 10,
            _ => rng.next() as i64,
        };
        // Truncating casts on purpose: `i64::MIN as i16` is 0, so the limits of the
        // narrow types are drawn separately.
        match rng.below(4) {
            0 => Value::SmallInt(raw as i16),
            1 => Value::Integer(raw as i32),
            2 => Value::BigInt(raw),
            _ => [
                Value::SmallInt(i16::MIN),
                Value::SmallInt(i16::MAX),
                Value::Integer(i32::MIN),
                Value::Integer(i32::MAX),
            ][rng.below(4)]
            .clone(),
        }
    }

    fn model(value: &Value) -> Option<i128> {
        match *value {
            Value::Null => None,
            Value::SmallInt(v) => Some(v.into()),
            Value::Integer(v) => Some(v.into()),
            Value::BigInt(v) => Some(v.into()),
            Value::Text(_) => unreachable!("integer model only"),
        }
    }

    fn model_narrow(ty: DataType, value: i128) -> Result<Value, TypeError> {
        let out_of_range = |_| TypeError::OutOfRange { ty };
        Ok(match ty {
            SmallInt => Value::SmallInt(i16::try_from(value).map_err(out_of_range)?),
            Integer => Value::Integer(i32::try_from(value).map_err(out_of_range)?),
            BigInt => Value::BigInt(i64::try_from(value).map_err(out_of_range)?),
            DataType::Varchar(_) => unreachable!("integer model only"),
        })
    }

    fn expected_binary(name: &str, lhs: &Value, rhs: &Value) -> Result<Value, TypeError> {
        let (Some(l), Some(r)) = (model(lhs), model(rhs)) else {
            return Ok(Value::Null);
        };
        if matches!(name, "/" | "%") && r == 0 {
            return Err(TypeError::DivisionByZero);
        }
        let result = match name {
            "+" => l + r,
            "-" => l - r,
            "*" => l * r,
            "/" => l / r,
            _ => l % r,
        };
        let ty = lhs.data_type().unwrap().promote(rhs.data_type().unwrap());
        model_narrow(ty, result)
    }

    /// NULL sorts last: `(is_null, value)` orders every non-NULL first.
    fn model_sort_key(value: &Value) -> (bool, Option<i128>) {
        (value.is_null(), model(value))
    }

    #[test]
    fn random_operations_should_match_the_i128_model() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

        for _ in 0..10_000 {
            let (lhs, rhs) = (random_value(&mut rng), random_value(&mut rng));
            let (name, op) = BINARY_OPS[rng.below(BINARY_OPS.len())];

            assert_eq!(
                op(&lhs, &rhs),
                expected_binary(name, &lhs, &rhs),
                "{lhs:?} {name} {rhs:?}"
            );
            let expected_neg = match (lhs.data_type(), model(&lhs)) {
                (Some(ty), Some(l)) => model_narrow(ty, -l),
                _ => Ok(Value::Null),
            };
            assert_eq!(lhs.neg(), expected_neg, "-{lhs:?}");
            assert_eq!(
                lhs.compare(&rhs),
                Ok(model(&lhs).zip(model(&rhs)).map(|(l, r)| l.cmp(&r)))
            );
            assert_eq!(
                lhs.sort_cmp(&rhs),
                model_sort_key(&lhs).cmp(&model_sort_key(&rhs))
            );
            assert_eq!(lhs.is_distinct_from(&rhs), Ok(model(&lhs) != model(&rhs)));
        }
    }

    #[test]
    fn random_values_should_round_trip_through_bytes_and_text() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);

        for _ in 0..10_000 {
            let value = random_value(&mut rng);
            let ty = value.data_type().unwrap_or(ALL_TYPES[rng.below(3)]);

            assert_eq!(
                Value::decode(ty, value.encode().as_deref()).as_ref(),
                Ok(&value)
            );
            if !value.is_null() {
                assert_eq!(Value::parse(ty, &value.to_string()).as_ref(), Ok(&value));
            }
        }
    }

    /// Multibyte characters, tabs and trailing spaces: where bytes and characters differ,
    /// and where store assignment cuts instead of failing.
    fn random_text(rng: &mut Rng) -> String {
        let alphabet = ['a', 'Z', ' ', '\t', 'é', '名', '🦀'];
        let len = rng.below(12);
        let mut text: String = (0..len)
            .map(|_| alphabet[rng.below(alphabet.len())])
            .collect();
        if rng.below(3) == 0 {
            text.push_str(&" ".repeat(rng.below(6)));
        }
        text
    }

    fn model_assign(input: &str, max_chars: u16) -> Result<Value, TypeError> {
        let chars: Vec<char> = input.chars().collect();
        let n = usize::from(max_chars);
        if chars.len() <= n || chars[n..].iter().all(|&c| c == ' ') {
            Ok(Value::Text(chars.iter().take(n).collect()))
        } else {
            Err(too_long(max_chars, chars.len()))
        }
    }

    fn model_cast(input: &str, max_chars: u16) -> Value {
        Value::Text(input.chars().take(usize::from(max_chars)).collect())
    }

    #[test]
    fn random_text_operations_should_match_the_char_model() {
        let mut rng = Rng(0xA076_1D64_78BD_642F);

        for _ in 0..10_000 {
            let (lhs, rhs) = (random_text(&mut rng), random_text(&mut rng));
            let max_chars = 1 + rng.below(12) as u16;
            let ty = varchar(max_chars);
            let (value, other) = (text(&lhs), text(&rhs));

            assert_eq!(value.assign(ty), model_assign(&lhs, max_chars), "{lhs:?}");
            assert_eq!(Value::parse(ty, &lhs), model_assign(&lhs, max_chars));
            assert_eq!(value.cast(ty), Ok(model_cast(&lhs, max_chars)));
            assert_eq!(value.concat(&other), Ok(text(&format!("{lhs}{rhs}"))));
            assert_eq!(
                value.compare(&other),
                Ok(Some(lhs.chars().cmp(rhs.chars()))),
                "{lhs:?} vs {rhs:?}"
            );
            assert_eq!(value.is_distinct_from(&other), Ok(lhs != rhs));
            if let Ok(stored) = value.assign(ty) {
                assert_eq!(
                    Value::decode(ty, stored.encode().as_deref()).as_ref(),
                    Ok(&stored)
                );
                assert_eq!(Value::parse(ty, &stored.to_string()).as_ref(), Ok(&stored));
            }
        }
    }

    #[test]
    fn sort_cmp_should_be_a_total_order() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);

        for _ in 0..10_000 {
            let [a, b, c] = [(); 3].map(|_| match rng.below(4) {
                0 => text(&random_text(&mut rng)),
                _ => random_value(&mut rng),
            });

            assert_eq!(a.sort_cmp(&a), Ordering::Equal);
            assert_eq!(a.sort_cmp(&b), b.sort_cmp(&a).reverse());
            if a.sort_cmp(&b).is_le() && b.sort_cmp(&c).is_le() {
                assert!(a.sort_cmp(&c).is_le(), "{a:?} <= {b:?} <= {c:?}");
            }
        }
    }
}
