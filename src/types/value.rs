//! Runtime values. A stored field is `Option<&[u8]>`, the contract of `TupleLayout::encode`
//! and `TupleRef::field`: `None` is NULL (a bit in the tuple's NULL bitmap). `Some` holds
//! an integer in two's complement, little-endian, `DataType::fixed_width` bytes; a REAL or
//! DOUBLE PRECISION as IEEE 754 little-endian, 4 or 8 bytes, never NaN, infinite or `-0.0`;
//! a VARCHAR as its UTF-8 bytes (empty text is `Some(&[])`, never NULL); a BOOLEAN as one
//! byte, `0` or `1`; a DATE as an `i32` little-endian of days since 1970-01-01, within
//! 0001-01-01..=9999-12-31; or a TIMESTAMP as an `i64` little-endian of microseconds since
//! 1970-01-01 00:00:00, in the same range.

use std::cmp::Ordering;
use std::fmt;

use super::{
    DataType, Date, Finite32, Finite64, Timestamp, TypeError, boolean, date, float, integer, text,
    timestamp,
};

/// `PartialEq` is structural and meant for tests: `Null == Null` and
/// `SmallInt(1) != BigInt(1)`. SQL equality is `sql_eq` (or `compare`), which yields
/// UNKNOWN for NULL.
///
/// NULL carries no type: the type of an expression is decided statically from the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Null,
    SmallInt(i16),
    Integer(i32),
    BigInt(i64),
    Real(Finite32),
    Double(Finite64),
    /// A VARCHAR value. The limit `n` belongs to the column, not to the value.
    Text(String),
    Boolean(bool),
    Date(Date),
    Timestamp(Timestamp),
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
            Value::Boolean(_) => Some(DataType::Boolean),
            Value::Date(_) => Some(DataType::Date),
            Value::Timestamp(_) => Some(DataType::Timestamp),
            Value::Real(_) => Some(DataType::Real),
            Value::Double(_) => Some(DataType::Double),
        }
    }

    pub fn encode(&self) -> Option<Vec<u8>> {
        match self {
            Value::Null => None,
            Value::SmallInt(value) => Some(value.to_le_bytes().to_vec()),
            Value::Integer(value) => Some(value.to_le_bytes().to_vec()),
            Value::BigInt(value) => Some(value.to_le_bytes().to_vec()),
            Value::Text(text) => Some(text.as_bytes().to_vec()),
            Value::Boolean(value) => Some(vec![boolean::encode(*value)]),
            Value::Date(value) => Some(value.days_since_epoch().to_le_bytes().to_vec()),
            Value::Timestamp(value) => Some(value.micros_since_epoch().to_le_bytes().to_vec()),
            Value::Real(value) => Some(value.get().to_le_bytes().to_vec()),
            Value::Double(value) => Some(value.get().to_le_bytes().to_vec()),
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
            DataType::Date => date::decode(fixed_width(ty, bytes)?)?,
            DataType::Timestamp => timestamp::decode(fixed_width(ty, bytes)?)?,
            DataType::Real => float::decode_real(fixed_width(ty, bytes)?)?,
            DataType::Double => float::decode_double(fixed_width(ty, bytes)?)?,
            DataType::Varchar(max_chars) => text::decode(max_chars, bytes)?,
            DataType::Boolean => {
                let [byte] = fixed_width(ty, bytes)?;
                boolean::decode(byte)?
            }
        })
    }

    /// Text input. Never yields NULL: `NULL` is a keyword, so the text `'NULL'` is
    /// invalid input for an integer and plain text for a VARCHAR. A VARCHAR follows the
    /// store assignment rule.
    pub fn parse(ty: DataType, input: &str) -> Result<Value, TypeError> {
        match ty {
            DataType::Varchar(max_chars) => text::assign(max_chars, input),
            DataType::Boolean => boolean::parse(input),
            DataType::Real => float::parse_real(input),
            DataType::Double => float::parse_double(input),
            DataType::Date => date::parse(input),
            DataType::Timestamp => timestamp::parse(input),
            integer => integer::parse(integer, input),
        }
    }

    /// Store assignment (`INSERT`/`UPDATE`): only within a type family, and never losing
    /// information silently: a float becomes an integer, and a TIMESTAMP a DATE, only through
    /// an explicit CAST. Text longer than the target VARCHAR(n) is an error unless the excess is
    /// only spaces, which are cut.
    pub fn assign(&self, target: DataType) -> Result<Value, TypeError> {
        let Some(ty) = self.data_type() else {
            return Ok(Value::Null);
        };
        let drops_a_part = ty.is_float() && target.is_integer()
            || ty == DataType::Timestamp && target == DataType::Date;
        if ty.family() != target.family() || drops_a_part {
            return Err(TypeError::DatatypeMismatch { value: ty, target });
        }
        match (self, target) {
            (Value::Text(text), DataType::Varchar(max_chars)) => text::assign(max_chars, text),
            _ => self.cast(target),
        }
    }

    /// Explicit `CAST`: every pair of types converts except floats and booleans, and datetimes
    /// with anything but text and datetimes (`CannotCast`). Text longer than the target
    /// VARCHAR(n) is cut, whatever the excess holds; only `0`/`1` and `'true'`/`'false'`/`'1'`/
    /// `'0'` become booleans; floats become integers rounding half to even; a TIMESTAMP becomes
    /// its day and a DATE its midnight.
    pub fn cast(&self, target: DataType) -> Result<Value, TypeError> {
        let Some(operand) = self.operand() else {
            return Ok(Value::Null);
        };
        match (operand, target) {
            (
                source @ (Operand::Integer(..) | Operand::Float(..) | Operand::Boolean(_)),
                DataType::Date | DataType::Timestamp,
            ) => Err(TypeError::CannotCast {
                from: source.data_type(),
                to: target,
            }),
            (Operand::Integer(_, value), DataType::Boolean) => boolean::from_integer(value),
            (Operand::Integer(_, value), DataType::Real | DataType::Double) => {
                float::from_integer(target, value)
            }
            (Operand::Integer(_, value), DataType::Varchar(max_chars)) => {
                Ok(text::truncate(max_chars, &value.to_string()))
            }
            (Operand::Integer(_, value), integer) => {
                integer::narrow(integer, value).ok_or(TypeError::OutOfRange { ty: integer })
            }
            (Operand::Float(source, _), DataType::Boolean) => Err(TypeError::CannotCast {
                from: source,
                to: DataType::Boolean,
            }),
            (Operand::Float(_, value), DataType::Real | DataType::Double) => {
                float::convert(target, value)
            }
            (Operand::Float(..), DataType::Varchar(max_chars)) => {
                Ok(text::truncate(max_chars, &self.to_string()))
            }
            (Operand::Float(_, value), integer) => float::to_integer(integer, value),
            (Operand::Text(text), DataType::Varchar(max_chars)) => {
                Ok(text::truncate(max_chars, text))
            }
            // The text input of the target type: integers, floats and booleans.
            (Operand::Text(text), other) => Value::parse(other, text),
            (Operand::Timestamp(_), DataType::Timestamp) => Ok(self.clone()),
            (Operand::Timestamp(micros), DataType::Date) => {
                Ok(Value::Date(timestamp::day_of(micros)))
            }
            (Operand::Timestamp(_), DataType::Varchar(max_chars)) => {
                Ok(text::truncate(max_chars, &self.to_string()))
            }
            (Operand::Timestamp(_), other) => Err(TypeError::CannotCast {
                from: DataType::Timestamp,
                to: other,
            }),
            (Operand::Date(days), DataType::Timestamp) => Ok(Value::Timestamp(
                Timestamp::at_midnight(Date::from_valid_days(days)),
            )),
            (Operand::Date(_), DataType::Date) => Ok(self.clone()),
            (Operand::Date(_), DataType::Varchar(max_chars)) => {
                Ok(text::truncate(max_chars, &self.to_string()))
            }
            (Operand::Date(_), other) => Err(TypeError::CannotCast {
                from: DataType::Date,
                to: other,
            }),
            (Operand::Boolean(value), DataType::Boolean) => Ok(Value::Boolean(value)),
            (Operand::Boolean(_), DataType::Real | DataType::Double) => {
                Err(TypeError::CannotCast {
                    from: DataType::Boolean,
                    to: target,
                })
            }
            (Operand::Boolean(value), DataType::Varchar(max_chars)) => {
                Ok(text::truncate(max_chars, boolean::literal(value)))
            }
            (Operand::Boolean(value), integer) => integer::narrow(integer, i64::from(value))
                .ok_or(TypeError::OutOfRange { ty: integer }),
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

    /// Numbers, or `DATE + integer` / `integer + DATE` in days.
    pub fn add(&self, rhs: &Value) -> Result<Value, TypeError> {
        match (self.operand(), rhs.operand()) {
            (Some(Operand::Date(days)), Some(Operand::Integer(_, offset)))
            | (Some(Operand::Integer(_, offset)), Some(Operand::Date(days))) => {
                date::add_days(days, offset)
            }
            _ => self.strict_binary(rhs, "+", integer::add, Some(float::add)),
        }
    }

    /// Numbers, `DATE - integer` in days, or `DATE - DATE` as the days between.
    pub fn sub(&self, rhs: &Value) -> Result<Value, TypeError> {
        match (self.operand(), rhs.operand()) {
            (Some(Operand::Date(days)), Some(Operand::Integer(_, offset))) => {
                date::subtract_days(days, offset)
            }
            (Some(Operand::Date(lhs)), Some(Operand::Date(rhs))) => {
                Ok(date::days_between(lhs, rhs))
            }
            _ => self.strict_binary(rhs, "-", integer::sub, Some(float::sub)),
        }
    }

    pub fn mul(&self, rhs: &Value) -> Result<Value, TypeError> {
        self.strict_binary(rhs, "*", integer::mul, Some(float::mul))
    }

    pub fn div(&self, rhs: &Value) -> Result<Value, TypeError> {
        self.strict_binary(rhs, "/", integer::div, Some(float::div))
    }

    /// Integers only: SQL's `MOD` is not defined for approximate numbers.
    pub fn rem(&self, rhs: &Value) -> Result<Value, TypeError> {
        self.strict_binary(rhs, "%", integer::rem, None)
    }

    pub fn neg(&self) -> Result<Value, TypeError> {
        match self.operand() {
            None => Ok(Value::Null),
            Some(Operand::Integer(ty, value)) => integer::neg(ty, value),
            Some(Operand::Float(ty, value)) => float::neg(ty, value),
            Some(other) => Err(TypeError::UndefinedOperator {
                op: "-",
                left: None,
                right: other.data_type(),
            }),
        }
    }

    /// Strict operators: a NULL operand yields NULL before the operation
    /// runs, so `NULL / 0` is NULL rather than an error.
    fn strict_binary(
        &self,
        rhs: &Value,
        op: &'static str,
        integer_operation: fn(DataType, i64, i64) -> Result<Value, TypeError>,
        float_operation: Option<FloatOperation>,
    ) -> Result<Value, TypeError> {
        match (self.operand(), rhs.operand()) {
            (Some(Operand::Integer(lhs_ty, lhs)), Some(Operand::Integer(rhs_ty, rhs))) => {
                integer_operation(lhs_ty.promote(rhs_ty), lhs, rhs)
            }
            (Some(lhs), Some(rhs)) => match (lhs.as_float(), rhs.as_float(), float_operation) {
                (Some((lhs_ty, lhs)), Some((rhs_ty, rhs)), Some(operation)) => {
                    operation(float::result_type(lhs_ty, rhs_ty), lhs, rhs)
                }
                _ => Err(undefined_operator(op, &lhs, &rhs)),
            },
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

    pub fn sql_eq(&self, rhs: &Value) -> Result<Value, TypeError> {
        self.sql_comparison(rhs, "=", Ordering::is_eq)
    }

    pub fn sql_ne(&self, rhs: &Value) -> Result<Value, TypeError> {
        self.sql_comparison(rhs, "<>", Ordering::is_ne)
    }

    pub fn sql_lt(&self, rhs: &Value) -> Result<Value, TypeError> {
        self.sql_comparison(rhs, "<", Ordering::is_lt)
    }

    pub fn sql_le(&self, rhs: &Value) -> Result<Value, TypeError> {
        self.sql_comparison(rhs, "<=", Ordering::is_le)
    }

    pub fn sql_gt(&self, rhs: &Value) -> Result<Value, TypeError> {
        self.sql_comparison(rhs, ">", Ordering::is_gt)
    }

    pub fn sql_ge(&self, rhs: &Value) -> Result<Value, TypeError> {
        self.sql_comparison(rhs, ">=", Ordering::is_ge)
    }

    /// A comparison as a SQL value: `Boolean`, or `Null` (UNKNOWN) when a side is NULL. The
    /// error across families names the operator actually used.
    fn sql_comparison(
        &self,
        rhs: &Value,
        op: &'static str,
        holds: fn(Ordering) -> bool,
    ) -> Result<Value, TypeError> {
        match (self.operand(), rhs.operand()) {
            (Some(lhs), Some(rhs)) => Ok(Value::Boolean(holds(lhs.compare(&rhs, op)?))),
            _ => Ok(Value::Null),
        }
    }

    /// `AND` is not strict: `FALSE AND NULL` is FALSE. Both operands are type-checked first,
    /// so a non-boolean fails even when the other side alone would decide the result.
    pub fn and(&self, rhs: &Value) -> Result<Value, TypeError> {
        let (lhs, rhs) = (self.truth("AND")?, rhs.truth("AND")?);
        Ok(from_truth(boolean::and(lhs, rhs)))
    }

    /// `OR` is not strict: `TRUE OR NULL` is TRUE.
    pub fn or(&self, rhs: &Value) -> Result<Value, TypeError> {
        let (lhs, rhs) = (self.truth("OR")?, rhs.truth("OR")?);
        Ok(from_truth(boolean::or(lhs, rhs)))
    }

    pub fn not(&self) -> Result<Value, TypeError> {
        Ok(from_truth(self.truth("NOT")?.map(|value| !value)))
    }

    /// `IS TRUE`: never UNKNOWN, so it is what a `WHERE` filter keeps.
    pub fn is_true(&self) -> Result<bool, TypeError> {
        Ok(self.truth("IS TRUE")? == Some(true))
    }

    pub fn is_false(&self) -> Result<bool, TypeError> {
        Ok(self.truth("IS FALSE")? == Some(false))
    }

    pub fn is_unknown(&self) -> Result<bool, TypeError> {
        Ok(self.truth("IS UNKNOWN")?.is_none())
    }

    /// The truth value of a logical operand; NULL is UNKNOWN (`None`).
    fn truth(&self, op: &'static str) -> Result<Option<bool>, TypeError> {
        match self.operand() {
            None => Ok(None),
            Some(Operand::Boolean(value)) => Ok(Some(value)),
            Some(other) => Err(TypeError::NotBoolean {
                op,
                actual: other.data_type(),
            }),
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
    /// of the families it refuses to mix; here numbers (integers and floats, interleaved by
    /// value) sort before text, text before booleans, booleans before datetimes (dates and
    /// timestamps, interleaved by instant), and NULL after every value (`NULLS LAST` for
    /// `ASC`).
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
            Value::Boolean(value) => Some(Operand::Boolean(*value)),
            Value::Date(value) => Some(Operand::Date(value.days_since_epoch())),
            Value::Timestamp(value) => Some(Operand::Timestamp(value.micros_since_epoch())),
            Value::Real(value) => Some(Operand::Float(DataType::Real, value.get().into())),
            Value::Double(value) => Some(Operand::Float(DataType::Double, value.get())),
        }
    }
}

type FloatOperation = fn(DataType, f64, f64) -> Result<Value, TypeError>;

/// A non-NULL value as the operators see it: every integer widened to `i64` and every float
/// to `f64`, so one implementation covers all widths, text borrowed, a date as its day number
/// and a timestamp as its microseconds.
enum Operand<'a> {
    Integer(DataType, i64),
    /// REAL is widened to f64 exactly.
    Float(DataType, f64),
    Text(&'a str),
    Boolean(bool),
    /// Days since 1970-01-01.
    Date(i32),
    /// Microseconds since 1970-01-01 00:00:00.
    Timestamp(i64),
}

impl Operand<'_> {
    fn data_type(&self) -> DataType {
        match self {
            Operand::Integer(ty, _) | Operand::Float(ty, _) => *ty,
            Operand::Text(_) => DataType::VARCHAR_MAX,
            Operand::Boolean(_) => DataType::Boolean,
            Operand::Date(_) => DataType::Date,
            Operand::Timestamp(_) => DataType::Timestamp,
        }
    }

    /// A numeric operand as a float, for arithmetic that involves a float. An integer above
    /// 2^53 rounds to the nearest f64: such arithmetic is approximate by definition.
    fn as_float(&self) -> Option<(DataType, f64)> {
        match *self {
            Operand::Integer(ty, value) => Some((ty, value as f64)),
            Operand::Float(ty, value) => Some((ty, value)),
            Operand::Text(_) | Operand::Boolean(_) | Operand::Date(_) | Operand::Timestamp(_) => {
                None
            }
        }
    }

    fn compare(&self, rhs: &Operand, op: &'static str) -> Result<Ordering, TypeError> {
        if self.data_type().family() != rhs.data_type().family() {
            return Err(undefined_operator(op, self, rhs));
        }
        Ok(self.sort_cmp(rhs))
    }

    /// Integers and floats compare by value (`total_cmp` is numeric without NaN and `-0.0`),
    /// `str` orders by its UTF-8 bytes, which is also code point order, and `bool` puts
    /// FALSE before TRUE. A column never mixes families; ordering them (integers, text,
    /// booleans) only makes the order total.
    fn sort_cmp(&self, rhs: &Operand) -> Ordering {
        match (self, rhs) {
            (Operand::Integer(_, lhs), Operand::Integer(_, rhs)) => lhs.cmp(rhs),
            (Operand::Float(_, lhs), Operand::Float(_, rhs)) => lhs.total_cmp(rhs),
            (Operand::Integer(_, lhs), Operand::Float(_, rhs)) => {
                float::compare_integer_with_float(*lhs, *rhs)
            }
            (Operand::Float(_, lhs), Operand::Integer(_, rhs)) => {
                float::compare_integer_with_float(*rhs, *lhs).reverse()
            }
            (Operand::Text(lhs), Operand::Text(rhs)) => lhs.cmp(rhs),
            (Operand::Boolean(lhs), Operand::Boolean(rhs)) => lhs.cmp(rhs),
            (Operand::Date(lhs), Operand::Date(rhs)) => lhs.cmp(rhs),
            (Operand::Timestamp(lhs), Operand::Timestamp(rhs)) => lhs.cmp(rhs),
            // A DATE is midnight of its day, as the standard extends it with a zero time.
            (Operand::Date(lhs), Operand::Timestamp(rhs)) => {
                timestamp::midnight_micros(*lhs).cmp(rhs)
            }
            (Operand::Timestamp(lhs), Operand::Date(rhs)) => {
                lhs.cmp(&timestamp::midnight_micros(*rhs))
            }
            _ => self.data_type().family().cmp(&rhs.data_type().family()),
        }
    }
}

fn from_truth(truth: Option<bool>) -> Value {
    truth.map_or(Value::Null, Value::Boolean)
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
            Value::Boolean(value) => f.write_str(boolean::literal(*value)),
            Value::Date(value) => write!(f, "{value}"),
            Value::Timestamp(value) => write!(f, "{value}"),
            Value::Real(value) => write!(f, "{value}"),
            Value::Double(value) => write!(f, "{value}"),
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
        TypeError::CorruptedValue {
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
    fn boolean_should_have_the_boolean_type() {
        assert_eq!(Value::Boolean(true).data_type(), Some(DataType::Boolean));
        assert!(!Value::Boolean(false).is_null());
    }

    #[test]
    fn encode_of_boolean_should_be_one_byte_0_or_1() {
        assert_eq!(Value::Boolean(false).encode().unwrap(), [0]);
        assert_eq!(Value::Boolean(true).encode().unwrap(), [1]);
    }

    #[test]
    fn decode_should_round_trip_booleans() {
        for value in [Value::Boolean(false), Value::Boolean(true)] {
            let decoded = Value::decode(DataType::Boolean, value.encode().as_deref()).unwrap();

            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn decode_of_a_boolean_byte_other_than_0_or_1_should_be_corrupted() {
        for byte in [2, 0x80, 0xFF] {
            let err = Value::decode(DataType::Boolean, Some(&[byte])).unwrap_err();

            assert_eq!(
                err,
                TypeError::CorruptedValue {
                    ty: DataType::Boolean,
                    reason: "boolean byte must be 0 or 1"
                },
                "{byte}"
            );
        }
    }

    #[test]
    fn decode_of_a_boolean_with_the_wrong_width_should_fail() {
        for bytes in [&[][..], &[0, 1]] {
            let err = Value::decode(DataType::Boolean, Some(bytes)).unwrap_err();

            assert_eq!(
                err,
                TypeError::InvalidEncoding {
                    ty: DataType::Boolean,
                    expected: 1,
                    actual: bytes.len()
                }
            );
        }
    }

    #[test]
    fn parse_of_boolean_should_accept_true_and_false_in_any_case() {
        let cases = [
            ("true", true),
            ("TRUE", true),
            ("True", true),
            ("tRuE", true),
            ("false", false),
            ("FALSE", false),
            (
                "  false	
",
                false,
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(
                Value::parse(DataType::Boolean, input),
                Ok(Value::Boolean(expected)),
                "{input:?}"
            );
        }
    }

    #[test]
    fn parse_of_boolean_should_accept_1_and_0() {
        assert_eq!(
            Value::parse(DataType::Boolean, "1"),
            Ok(Value::Boolean(true))
        );
        assert_eq!(
            Value::parse(DataType::Boolean, " 0 "),
            Ok(Value::Boolean(false))
        );
    }

    #[test]
    fn parse_of_boolean_should_reject_anything_else() {
        let cases = [
            "",
            " ",
            "t",
            "f",
            "yes",
            "no",
            "on",
            "off",
            "2",
            "01",
            "+1",
            "-0",
            "1.0",
            "unknown",
            "NULL",
            "true false",
            "truee",
            "verdadeiro",
        ];

        for input in cases {
            assert_eq!(
                Value::parse(DataType::Boolean, input),
                Err(invalid_text(DataType::Boolean, input)),
                "{input:?}"
            );
        }
    }

    #[test]
    fn display_of_boolean_should_be_the_sql_literal() {
        assert_eq!(Value::Boolean(true).to_string(), "TRUE");
        assert_eq!(Value::Boolean(false).to_string(), "FALSE");
    }

    fn real(value: f32) -> Value {
        Value::Real(Finite32::new(value).unwrap())
    }

    fn double(value: f64) -> Value {
        Value::Double(Finite64::new(value).unwrap())
    }

    fn corrupted_float(ty: DataType, reason: &'static str) -> TypeError {
        TypeError::CorruptedValue { ty, reason }
    }

    #[test]
    fn floats_should_have_their_float_type() {
        assert_eq!(real(1.5).data_type(), Some(DataType::Real));
        assert_eq!(double(1.5).data_type(), Some(DataType::Double));
    }

    #[test]
    fn encode_of_floats_should_be_ieee_754_little_endian() {
        assert_eq!(real(1.5).encode().unwrap(), [0x00, 0x00, 0xC0, 0x3F]);
        assert_eq!(double(-2.0).encode().unwrap(), [0, 0, 0, 0, 0, 0, 0, 0xC0]);
    }

    #[test]
    fn decode_should_round_trip_float_limits() {
        let reals = [
            f32::MIN,
            f32::MAX,
            f32::MIN_POSITIVE,
            f32::from_bits(1),
            0.0,
            -1.5,
        ];
        let doubles = [
            f64::MIN,
            f64::MAX,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            0.0,
            -1.5,
        ];

        for value in reals.map(real).into_iter().chain(doubles.map(double)) {
            let ty = value.data_type().unwrap();

            assert_eq!(Value::decode(ty, value.encode().as_deref()), Ok(value));
        }
    }

    #[test]
    fn decode_of_non_finite_float_bits_should_be_corrupted() {
        for bits in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY].map(f32::to_le_bytes) {
            assert_eq!(
                Value::decode(DataType::Real, Some(&bits)),
                Err(corrupted_float(DataType::Real, "float must be finite"))
            );
        }
        for bits in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY].map(f64::to_le_bytes) {
            assert_eq!(
                Value::decode(DataType::Double, Some(&bits)),
                Err(corrupted_float(DataType::Double, "float must be finite"))
            );
        }
    }

    #[test]
    fn decode_of_negative_zero_should_be_corrupted() {
        let reason = "negative zero is never stored";

        assert_eq!(
            Value::decode(DataType::Real, Some(&(-0.0f32).to_le_bytes())),
            Err(corrupted_float(DataType::Real, reason))
        );
        assert_eq!(
            Value::decode(DataType::Double, Some(&(-0.0f64).to_le_bytes())),
            Err(corrupted_float(DataType::Double, reason))
        );
    }

    #[test]
    fn decode_of_a_float_with_the_wrong_width_should_fail() {
        for (ty, expected, widths) in [(DataType::Real, 4, [3, 5]), (DataType::Double, 8, [7, 9])] {
            for actual in widths {
                assert_eq!(
                    Value::decode(ty, Some(&vec![0; actual])),
                    Err(TypeError::InvalidEncoding {
                        ty,
                        expected,
                        actual
                    })
                );
            }
        }
    }

    #[test]
    fn parse_of_double_should_read_decimal_and_scientific_forms() {
        let cases = [
            ("1", 1.0),
            ("-2.5", -2.5),
            (".5", 0.5),
            ("5.", 5.0),
            ("1e10", 1e10),
            ("1.5E-3", 1.5e-3),
            ("+7", 7.0),
            ("  3.25\t\n", 3.25),
            ("0", 0.0),
            ("123456789012345678901234567890", 1.2345678901234568e29),
            ("1e-300", 1e-300),
            ("0e-999", 0.0),
        ];

        for (input, expected) in cases {
            assert_eq!(
                Value::parse(DataType::Double, input),
                Ok(double(expected)),
                "{input:?}"
            );
        }
    }

    #[test]
    fn parse_of_minus_zero_should_be_zero() {
        assert_eq!(Value::parse(DataType::Double, "-0"), Ok(double(0.0)));
        assert_eq!(Value::parse(DataType::Real, "-0.0"), Ok(real(0.0)));
    }

    #[test]
    fn parse_of_real_should_round_directly_to_f32() {
        assert_eq!(Value::parse(DataType::Real, "0.1"), Ok(real(0.1)));
        assert_eq!(
            Value::parse(DataType::Real, "16777217"),
            Ok(real(16_777_216.0))
        );
        assert_eq!(Value::parse(DataType::Real, "-3.4e38"), Ok(real(-3.4e38)));
    }

    #[test]
    fn parse_of_floats_should_reject_anything_but_finite_decimals() {
        let cases = [
            "", " ", "NaN", "nan", "inf", "-inf", "Infinity", "infinity", "0x1p3", "1_0", "1,5",
            ".", "-.", "e5", "1e", "1e+", "--1", "+-1", "1.2.3", "1e2e3", "1 2", "١", "1f",
        ];

        for ty in [DataType::Real, DataType::Double] {
            for input in cases {
                assert_eq!(
                    Value::parse(ty, input),
                    Err(invalid_text(ty, input)),
                    "{ty} {input:?}"
                );
            }
        }
    }

    #[test]
    fn parse_of_floats_beyond_the_type_should_be_out_of_range() {
        let cases = [
            (DataType::Double, "1e309"),
            (DataType::Double, "-1e400"),
            (DataType::Double, "1e-400"),
            (DataType::Real, "1e39"),
            (DataType::Real, "3.5e38"),
            (DataType::Real, "1e-50"),
        ];

        for (ty, input) in cases {
            assert_eq!(
                Value::parse(ty, input),
                Err(text_out_of_range(ty, input)),
                "{ty} {input:?}"
            );
        }
    }

    #[test]
    fn display_of_double_should_use_the_shortest_round_trip_digits() {
        let cases = [
            (1.5, "1.5"),
            (3.0, "3"),
            (0.0, "0"),
            (-0.1, "-0.1"),
            (123.456, "123.456"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1e14, "100000000000000"),
            (1e15, "1e+15"),
            (1.2345e20, "1.2345e+20"),
            (f64::MAX, "1.7976931348623157e+308"),
            (f64::MIN_POSITIVE, "2.2250738585072014e-308"),
            (0.1 + 0.2, "0.30000000000000004"),
        ];

        for (value, expected) in cases {
            assert_eq!(double(value).to_string(), expected);
        }
    }

    #[test]
    fn display_of_real_should_switch_to_scientific_from_1e6() {
        let cases = [
            (0.1, "0.1"),
            (123456.0, "123456"),
            (1e6, "1e+06"),
            (16_777_216.0, "1.6777216e+07"),
            (f32::MAX, "3.4028235e+38"),
            (-2.5e-5, "-2.5e-05"),
        ];

        for (value, expected) in cases {
            assert_eq!(real(value).to_string(), expected);
        }
    }

    #[test]
    fn floats_should_round_trip_through_their_text() {
        let reals = [
            f32::MIN,
            f32::MAX,
            f32::MIN_POSITIVE,
            f32::from_bits(1),
            0.1,
            -7.25,
        ];
        let doubles = [
            f64::MIN,
            f64::MAX,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            0.1,
            -7.25,
        ];

        for value in reals.map(real).into_iter().chain(doubles.map(double)) {
            let ty = value.data_type().unwrap();

            assert_eq!(Value::parse(ty, &value.to_string()), Ok(value));
        }
    }

    #[test]
    fn float_arithmetic_should_compute_common_cases() {
        let (lhs, rhs) = (double(7.5), double(2.0));

        assert_eq!(lhs.add(&rhs), Ok(double(9.5)));
        assert_eq!(lhs.sub(&rhs), Ok(double(5.5)));
        assert_eq!(lhs.mul(&rhs), Ok(double(15.0)));
        assert_eq!(lhs.div(&rhs), Ok(double(3.75)));
        assert_eq!(lhs.neg(), Ok(double(-7.5)));
        assert_eq!(real(1.5).neg(), Ok(real(-1.5)));
    }

    #[test]
    fn float_arithmetic_should_give_real_only_for_two_reals() {
        let cases = [
            (real(1.0), real(2.0), DataType::Real),
            (real(1.0), double(2.0), DataType::Double),
            (double(1.0), real(2.0), DataType::Double),
            (Value::Integer(1), real(2.0), DataType::Double),
            (real(1.0), Value::SmallInt(2), DataType::Double),
            (Value::BigInt(1), double(2.0), DataType::Double),
        ];

        for (lhs, rhs, ty) in cases {
            for (name, op) in BINARY_OPS.into_iter().filter(|(name, _)| *name != "%") {
                let result = op(&lhs, &rhs).unwrap();

                assert_eq!(result.data_type(), Some(ty), "{lhs:?} {name} {rhs:?}");
            }
        }
    }

    #[test]
    fn real_arithmetic_should_round_to_f32() {
        assert_eq!(real(0.1).add(&real(0.2)), Ok(real(0.1 + 0.2)));
        assert_eq!(real(1.0).div(&real(3.0)), Ok(real(1.0 / 3.0)));
        assert_eq!(
            Value::Integer(16_777_217).mul(&real(1.0)),
            Ok(double(16_777_217.0))
        );
    }

    #[test]
    fn float_overflow_should_be_out_of_range() {
        let double_overflow = Err(TypeError::OutOfRange {
            ty: DataType::Double,
        });
        let real_overflow = Err(TypeError::OutOfRange { ty: DataType::Real });

        assert_eq!(double(f64::MAX).mul(&double(2.0)), double_overflow);
        assert_eq!(double(f64::MAX).mul(&real(f32::MAX)), double_overflow);
        assert_eq!(double(f64::MAX).add(&double(f64::MAX)), double_overflow);
        assert_eq!(double(-f64::MAX).sub(&double(f64::MAX)), double_overflow);
        assert_eq!(double(f64::MAX).div(&double(0.5)), double_overflow);
        assert_eq!(real(f32::MAX).mul(&real(2.0)), real_overflow);
        assert_eq!(real(3e38).add(&real(3e38)), real_overflow);
    }

    #[test]
    fn float_underflow_should_be_out_of_range() {
        let cases = [
            (
                double(f64::MIN_POSITIVE),
                double(f64::MIN_POSITIVE),
                DataType::Double,
            ),
            (real(1e-30), real(1e-30), DataType::Real),
        ];

        for (lhs, rhs, ty) in cases {
            assert_eq!(lhs.mul(&rhs), Err(TypeError::OutOfRange { ty }), "{lhs:?}");
        }
        assert_eq!(
            double(1e-300).div(&double(1e300)),
            Err(TypeError::OutOfRange {
                ty: DataType::Double
            })
        );
    }

    #[test]
    fn float_arithmetic_should_keep_subnormal_and_exact_zero_results() {
        assert_eq!(
            double(f64::MIN_POSITIVE).div(&double(2.0)),
            Ok(double(f64::MIN_POSITIVE / 2.0))
        );
        assert_eq!(double(1.5).sub(&double(1.5)), Ok(double(0.0)));
        assert_eq!(double(0.0).mul(&double(1e-300)), Ok(double(0.0)));
        assert_eq!(double(0.0).neg(), Ok(double(0.0)));
    }

    #[test]
    fn float_division_by_zero_should_fail() {
        let cases = [
            (double(1.0), double(0.0)),
            (double(0.0), double(0.0)),
            (double(1.0), Value::Integer(0)),
            (Value::Integer(1), real(0.0)),
        ];

        for (lhs, rhs) in cases {
            assert_eq!(lhs.div(&rhs), Err(TypeError::DivisionByZero), "{lhs:?}");
        }
    }

    #[test]
    fn remainder_with_floats_should_be_an_undefined_operator() {
        assert_eq!(
            double(5.0).rem(&double(2.0)),
            Err(undefined("%", Some(DataType::Double), DataType::Double))
        );
        assert_eq!(
            Value::Integer(5).rem(&real(2.0)),
            Err(undefined("%", Some(Integer), DataType::Real))
        );
    }

    #[test]
    fn float_arithmetic_with_null_or_another_family_should_follow_the_usual_rules() {
        assert_eq!(double(1.0).add(&Value::Null), Ok(Value::Null));
        assert_eq!(Value::Null.mul(&real(1.0)), Ok(Value::Null));
        assert_eq!(
            double(1.0).add(&text("1")),
            Err(undefined(
                "+",
                Some(DataType::Double),
                DataType::VARCHAR_MAX
            ))
        );
        assert_eq!(
            Value::Boolean(true).sub(&real(1.0)),
            Err(undefined("-", Some(DataType::Boolean), DataType::Real))
        );
    }

    const TWO_POW_53: i64 = 1 << 53;
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;

    #[test]
    fn compare_of_floats_should_be_numeric() {
        assert_eq!(double(1.5).compare(&double(2.5)), Ok(Some(Ordering::Less)));
        assert_eq!(real(1.5).compare(&double(1.5)), Ok(Some(Ordering::Equal)));
        assert_eq!(
            double(-0.5).compare(&real(-1.0)),
            Ok(Some(Ordering::Greater))
        );
    }

    #[test]
    fn compare_of_real_0_1_should_see_its_f32_rounding() {
        assert_eq!(real(0.1).compare(&double(0.1)), Ok(Some(Ordering::Greater)));
    }

    #[test]
    fn compare_of_integers_with_floats_should_be_exact() {
        let cases = [
            (
                Value::BigInt(TWO_POW_53 + 1),
                double(TWO_POW_53 as f64),
                Ordering::Greater,
            ),
            (
                Value::BigInt(TWO_POW_53),
                double(TWO_POW_53 as f64),
                Ordering::Equal,
            ),
            (Value::BigInt(i64::MAX), double(TWO_POW_63), Ordering::Less),
            (
                Value::BigInt(i64::MAX),
                double(9.223_372_036_854_775e18),
                Ordering::Greater,
            ),
            (
                Value::BigInt(i64::MIN),
                double(-TWO_POW_63),
                Ordering::Equal,
            ),
            (Value::BigInt(i64::MIN), double(-1e19), Ordering::Greater),
            (
                Value::BigInt(i64::MIN + 1),
                double(-TWO_POW_63),
                Ordering::Greater,
            ),
            (Value::Integer(1), double(1.0), Ordering::Equal),
            (Value::Integer(1), double(1.5), Ordering::Less),
            (Value::Integer(-2), double(-1.5), Ordering::Less),
            (Value::Integer(-1), double(-1.5), Ordering::Greater),
            (Value::SmallInt(0), double(1e-300), Ordering::Less),
            (Value::SmallInt(0), double(-1e-300), Ordering::Greater),
            (
                Value::Integer(16_777_217),
                real(16_777_216.0),
                Ordering::Greater,
            ),
        ];

        for (integer, float, expected) in cases {
            assert_eq!(
                integer.compare(&float),
                Ok(Some(expected)),
                "{integer:?} vs {float:?}"
            );
            assert_eq!(float.compare(&integer), Ok(Some(expected.reverse())));
        }
    }

    #[test]
    fn sql_comparisons_should_mix_integers_and_floats() {
        assert_eq!(
            Value::Integer(1).sql_lt(&double(1.5)),
            Ok(Value::Boolean(true))
        );
        assert_eq!(
            real(2.0).sql_eq(&Value::SmallInt(2)),
            Ok(Value::Boolean(true))
        );
        assert_eq!(double(1.0).sql_ne(&Value::Null), Ok(Value::Null));
        assert_eq!(Value::Integer(1).is_distinct_from(&double(1.0)), Ok(false));
        assert_eq!(
            double(1.0).sql_gt(&text("1")),
            Err(undefined(
                ">",
                Some(DataType::Double),
                DataType::VARCHAR_MAX
            ))
        );
    }

    #[test]
    fn sort_cmp_should_interleave_integers_and_floats_by_value() {
        let mut values = vec![
            double(2.5),
            Value::Integer(2),
            real(-1.0),
            Value::Null,
            Value::BigInt(3),
            double(2.0),
        ];

        values.sort_by(Value::sort_cmp);

        assert_eq!(
            values,
            [
                real(-1.0),
                Value::Integer(2),
                double(2.0),
                double(2.5),
                Value::BigInt(3),
                Value::Null,
            ]
        );
    }

    fn out_of_range_for(ty: DataType) -> TypeError {
        TypeError::OutOfRange { ty }
    }

    #[test]
    fn cast_of_integers_to_floats_should_round_to_the_nearest() {
        assert_eq!(Value::Integer(1).cast(DataType::Real), Ok(real(1.0)));
        assert_eq!(
            Value::BigInt(TWO_POW_53 + 1).cast(DataType::Double),
            Ok(double(TWO_POW_53 as f64))
        );
        assert_eq!(
            Value::Integer(16_777_217).cast(DataType::Real),
            Ok(real(16_777_216.0))
        );
        assert_eq!(
            Value::BigInt(i64::MAX).cast(DataType::Real),
            Ok(real(9.223_372e18))
        );
        assert_eq!(
            Value::SmallInt(i16::MIN).cast(DataType::Double),
            Ok(double(-32768.0))
        );
    }

    #[test]
    fn cast_of_floats_to_integers_should_round_half_to_even() {
        let cases = [
            (0.5, 0),
            (1.5, 2),
            (2.5, 2),
            (3.5, 4),
            (-0.5, 0),
            (-2.5, -2),
            (2.4, 2),
            (2.6, 3),
            (-2.6, -3),
        ];

        for (value, expected) in cases {
            assert_eq!(
                double(value).cast(Integer),
                Ok(Value::Integer(expected)),
                "{value}"
            );
        }
        assert_eq!(real(3.5).cast(SmallInt), Ok(Value::SmallInt(4)));
    }

    #[test]
    fn cast_of_floats_to_integers_at_the_limits_should_check_the_range() {
        let fits = [
            (double(32767.4), Value::SmallInt(i16::MAX)),
            (double(-32768.5), Value::SmallInt(i16::MIN)),
            (double(-TWO_POW_63), Value::BigInt(i64::MIN)),
            (
                double(9.223_372_036_854_775e18),
                Value::BigInt(9_223_372_036_854_774_784),
            ),
        ];
        let too_large = [
            (double(32767.5), SmallInt),
            (double(-32769.0), SmallInt),
            (double(2_147_483_647.5), Integer),
            (double(TWO_POW_63), BigInt),
            (double(-1e300), BigInt),
        ];

        for (value, expected) in fits {
            let ty = expected.data_type().unwrap();

            assert_eq!(value.cast(ty), Ok(expected), "{value:?}");
        }
        for (value, ty) in too_large {
            assert_eq!(
                value.cast(ty),
                Err(out_of_range_for(ty)),
                "{value:?} as {ty}"
            );
        }
    }

    #[test]
    fn cast_between_real_and_double_should_round_and_check_the_range() {
        assert_eq!(double(0.1).cast(DataType::Real), Ok(real(0.1)));
        assert_eq!(
            real(0.1).cast(DataType::Double),
            Ok(double(f64::from(0.1f32)))
        );
        assert_eq!(double(0.0).cast(DataType::Real), Ok(real(0.0)));
        assert_eq!(
            double(1e300).cast(DataType::Real),
            Err(out_of_range_for(DataType::Real))
        );
        assert_eq!(
            double(1e-300).cast(DataType::Real),
            Err(out_of_range_for(DataType::Real))
        );
    }

    #[test]
    fn cast_of_floats_to_varchar_should_be_their_text_cut_to_n() {
        assert_eq!(double(1.5).cast(DataType::VARCHAR_MAX), Ok(text("1.5")));
        assert_eq!(real(0.1).cast(DataType::VARCHAR_MAX), Ok(text("0.1")));
        assert_eq!(double(1e20).cast(varchar(3)), Ok(text("1e+")));
        assert_eq!(double(-0.25).cast(varchar(4)), Ok(text("-0.2")));
    }

    #[test]
    fn cast_of_text_to_floats_should_follow_the_input_rules() {
        assert_eq!(text(" 1.5e3 ").cast(DataType::Double), Ok(double(1500.0)));
        assert_eq!(text("0.1").cast(DataType::Real), Ok(real(0.1)));
        assert_eq!(
            text("NaN").cast(DataType::Double),
            Err(invalid_text(DataType::Double, "NaN"))
        );
    }

    #[test]
    fn cast_between_floats_and_booleans_should_be_impossible() {
        let cannot = |from, to| Err(TypeError::CannotCast { from, to });

        assert_eq!(
            real(1.0).cast(DataType::Boolean),
            cannot(DataType::Real, DataType::Boolean)
        );
        assert_eq!(
            double(0.0).cast(DataType::Boolean),
            cannot(DataType::Double, DataType::Boolean)
        );
        assert_eq!(
            Value::Boolean(true).cast(DataType::Real),
            cannot(DataType::Boolean, DataType::Real)
        );
        assert_eq!(
            Value::Boolean(false).cast(DataType::Double),
            cannot(DataType::Boolean, DataType::Double)
        );
        assert_eq!(Value::Null.cast(DataType::Real), Ok(Value::Null));
    }

    #[test]
    fn assign_of_a_float_to_an_integer_should_need_an_explicit_cast() {
        for ty in [SmallInt, Integer, BigInt] {
            assert_eq!(
                double(2.0).assign(ty),
                Err(TypeError::DatatypeMismatch {
                    value: DataType::Double,
                    target: ty
                })
            );
            assert_eq!(
                real(1.0).assign(ty),
                Err(TypeError::DatatypeMismatch {
                    value: DataType::Real,
                    target: ty
                })
            );
        }
    }

    #[test]
    fn assign_to_a_float_should_accept_any_numeric_value() {
        assert_eq!(Value::Integer(2).assign(DataType::Double), Ok(double(2.0)));
        assert_eq!(
            Value::BigInt(i64::MAX).assign(DataType::Real),
            Ok(real(9.223_372e18))
        );
        assert_eq!(double(1.5).assign(DataType::Double), Ok(double(1.5)));
        assert_eq!(real(1.5).assign(DataType::Double), Ok(double(1.5)));
        assert_eq!(
            double(1e300).assign(DataType::Real),
            Err(out_of_range_for(DataType::Real))
        );
        assert_eq!(Value::Null.assign(DataType::Real), Ok(Value::Null));
    }

    #[test]
    fn assign_of_text_or_booleans_to_floats_should_be_a_datatype_mismatch() {
        assert_eq!(
            text("1.5").assign(DataType::Double),
            Err(TypeError::DatatypeMismatch {
                value: DataType::VARCHAR_MAX,
                target: DataType::Double
            })
        );
        assert_eq!(
            Value::Boolean(true).assign(DataType::Real),
            Err(TypeError::DatatypeMismatch {
                value: DataType::Boolean,
                target: DataType::Real
            })
        );
        assert_eq!(
            double(1.0).assign(varchar(5)),
            Err(TypeError::DatatypeMismatch {
                value: DataType::Double,
                target: varchar(5)
            })
        );
    }

    fn date(year: i32, month: u32, day: u32) -> Value {
        Value::Date(Date::from_ymd(year, month, day).unwrap())
    }

    #[test]
    fn date_should_have_the_date_type() {
        assert_eq!(date(2024, 2, 29).data_type(), Some(DataType::Date));
    }

    #[test]
    fn encode_of_date_should_be_days_since_1970_little_endian() {
        assert_eq!(date(1970, 1, 2).encode().unwrap(), [1, 0, 0, 0]);
        assert_eq!(date(1969, 12, 31).encode().unwrap(), [0xFF; 4]);
        assert_eq!(date(2024, 2, 29).encode().unwrap(), 19_782i32.to_le_bytes());
    }

    #[test]
    fn decode_should_round_trip_dates() {
        let dates = [
            Value::Date(Date::MIN),
            Value::Date(Date::MAX),
            date(1970, 1, 1),
            date(2024, 2, 29),
        ];

        for value in dates {
            assert_eq!(
                Value::decode(DataType::Date, value.encode().as_deref()),
                Ok(value)
            );
        }
    }

    #[test]
    fn decode_of_days_outside_the_range_should_be_corrupted() {
        for days in [-719_163, 2_932_897, i32::MIN, i32::MAX] {
            assert_eq!(
                Value::decode(DataType::Date, Some(&days.to_le_bytes())),
                Err(TypeError::CorruptedValue {
                    ty: DataType::Date,
                    reason: "date out of range"
                }),
                "{days}"
            );
        }
    }

    #[test]
    fn decode_of_a_date_with_the_wrong_width_should_fail() {
        for actual in [3, 5] {
            assert_eq!(
                Value::decode(DataType::Date, Some(&vec![0; actual])),
                Err(TypeError::InvalidEncoding {
                    ty: DataType::Date,
                    expected: 4,
                    actual
                })
            );
        }
    }

    #[test]
    fn parse_of_date_should_read_iso_dates() {
        let cases = [
            ("2024-02-29", date(2024, 2, 29)),
            ("  1970-01-01\t\n", date(1970, 1, 1)),
            ("0001-01-01", Value::Date(Date::MIN)),
            ("9999-12-31", Value::Date(Date::MAX)),
        ];

        for (input, expected) in cases {
            assert_eq!(
                Value::parse(DataType::Date, input),
                Ok(expected),
                "{input:?}"
            );
        }
    }

    #[test]
    fn parse_of_date_should_reject_other_formats() {
        let cases = [
            "",
            " ",
            "2024-1-5",
            "2024-01-5",
            "2024/01/05",
            "20240105",
            "24-01-05",
            "2024-01-05x",
            "2024-01-05 00:00",
            "2024-01-05T00:00:00",
            "+2024-01-05",
            "-2024-01-05",
            "02024-01-05",
            "10000-01-01",
            "2024-O1-05",
            "2024--01-05",
            "２０２４-01-05",
            "today",
        ];

        for input in cases {
            let err = Value::parse(DataType::Date, input).unwrap_err();

            assert_eq!(err, invalid_text(DataType::Date, input), "{input:?}");
            assert_eq!(err.sqlstate(), "22007");
        }
    }

    #[test]
    fn parse_of_date_should_reject_days_that_do_not_exist() {
        let cases = [
            "2023-02-29",
            "1900-02-29",
            "2024-13-01",
            "2024-00-10",
            "2024-04-31",
            "2024-01-00",
            "2024-12-32",
            "0000-01-01",
        ];

        for input in cases {
            let err = Value::parse(DataType::Date, input).unwrap_err();

            assert_eq!(err, text_out_of_range(DataType::Date, input), "{input:?}");
            assert_eq!(err.sqlstate(), "22008");
        }
    }

    #[test]
    fn display_of_date_should_be_iso() {
        assert_eq!(date(1, 1, 1).to_string(), "0001-01-01");
        assert_eq!(date(2024, 2, 29).to_string(), "2024-02-29");
    }

    #[test]
    fn dates_should_round_trip_through_their_text() {
        for value in [
            Value::Date(Date::MIN),
            Value::Date(Date::MAX),
            date(1970, 1, 1),
            date(1600, 2, 29),
        ] {
            assert_eq!(Value::parse(DataType::Date, &value.to_string()), Ok(value));
        }
    }

    #[test]
    fn compare_of_dates_should_follow_the_calendar() {
        assert_eq!(
            date(2024, 1, 31).compare(&date(2024, 2, 1)),
            Ok(Some(Ordering::Less))
        );
        assert_eq!(
            Value::Date(Date::MAX).compare(&Value::Date(Date::MIN)),
            Ok(Some(Ordering::Greater))
        );
        assert_eq!(
            date(2000, 2, 29).compare(&date(2000, 2, 29)),
            Ok(Some(Ordering::Equal))
        );
    }

    #[test]
    fn compare_of_a_date_with_another_family_should_be_an_undefined_operator() {
        assert_eq!(
            date(2024, 1, 1).compare(&Value::Integer(19_723)),
            Err(undefined("=", Some(DataType::Date), Integer))
        );
        assert_eq!(
            text("2024-01-01").sql_lt(&date(2024, 1, 1)),
            Err(undefined("<", Some(DataType::VARCHAR_MAX), DataType::Date))
        );
    }

    #[test]
    fn sql_comparisons_should_work_on_dates() {
        let (earlier, later) = (date(1999, 12, 31), date(2000, 1, 1));

        assert_eq!(earlier.sql_lt(&later), Ok(Value::Boolean(true)));
        assert_eq!(later.sql_le(&earlier), Ok(Value::Boolean(false)));
        assert_eq!(earlier.sql_eq(&Value::Null), Ok(Value::Null));
        assert_eq!(earlier.is_distinct_from(&later), Ok(true));
        assert_eq!(earlier.is_distinct_from(&Value::Null), Ok(true));
    }

    #[test]
    fn sort_cmp_should_put_dates_after_the_other_families_and_before_null() {
        let mut values = vec![
            Value::Null,
            date(2024, 1, 1),
            Value::Integer(1),
            Value::Boolean(true),
            text("a"),
            date(1999, 1, 1),
        ];

        values.sort_by(Value::sort_cmp);

        assert_eq!(
            values,
            [
                Value::Integer(1),
                text("a"),
                Value::Boolean(true),
                date(1999, 1, 1),
                date(2024, 1, 1),
                Value::Null,
            ]
        );
    }

    fn date_out_of_range() -> TypeError {
        TypeError::OutOfRange { ty: DataType::Date }
    }

    #[test]
    fn adding_an_integer_to_a_date_should_move_it_by_days() {
        assert_eq!(
            date(2024, 1, 31).add(&Value::Integer(30)),
            Ok(date(2024, 3, 1))
        );
        assert_eq!(
            Value::SmallInt(1).add(&date(2024, 2, 28)),
            Ok(date(2024, 2, 29))
        );
        assert_eq!(
            date(2024, 1, 1).add(&Value::Integer(-1)),
            Ok(date(2023, 12, 31))
        );
        assert_eq!(
            date(2024, 3, 1).sub(&Value::BigInt(1)),
            Ok(date(2024, 2, 29))
        );
        assert_eq!(
            date(2024, 3, 1).add(&Value::Integer(0)),
            Ok(date(2024, 3, 1))
        );
    }

    #[test]
    fn subtracting_two_dates_should_give_the_days_between() {
        assert_eq!(
            date(2024, 3, 15).sub(&date(2024, 1, 1)),
            Ok(Value::Integer(74))
        );
        assert_eq!(
            date(2024, 1, 1).sub(&date(2024, 3, 15)),
            Ok(Value::Integer(-74))
        );
        assert_eq!(
            Value::Date(Date::MAX).sub(&Value::Date(Date::MIN)),
            Ok(Value::Integer(3_652_058))
        );
    }

    #[test]
    fn date_arithmetic_past_the_range_should_be_out_of_range() {
        let cases = [
            Value::Date(Date::MAX).add(&Value::Integer(1)),
            Value::Date(Date::MIN).sub(&Value::Integer(1)),
            Value::Date(Date::MIN).add(&Value::Integer(-1)),
            date(2024, 1, 1).add(&Value::BigInt(i64::MAX)),
            date(2024, 1, 1).add(&Value::BigInt(i64::MIN)),
            date(2024, 1, 1).sub(&Value::BigInt(i64::MIN)),
        ];

        for result in cases {
            assert_eq!(result, Err(date_out_of_range()));
        }
        assert_eq!(date_out_of_range().sqlstate(), "22008");
    }

    #[test]
    fn other_operators_with_dates_should_be_undefined() {
        let day = date(2024, 1, 1);
        let one = Value::Integer(1);

        assert_eq!(
            day.mul(&one),
            Err(undefined("*", Some(DataType::Date), Integer))
        );
        assert_eq!(
            day.div(&day),
            Err(undefined("/", Some(DataType::Date), DataType::Date))
        );
        assert_eq!(
            day.rem(&one),
            Err(undefined("%", Some(DataType::Date), Integer))
        );
        assert_eq!(
            day.add(&day),
            Err(undefined("+", Some(DataType::Date), DataType::Date))
        );
        assert_eq!(
            one.sub(&day),
            Err(undefined("-", Some(Integer), DataType::Date))
        );
        assert_eq!(
            day.add(&double(1.0)),
            Err(undefined("+", Some(DataType::Date), DataType::Double))
        );
        assert_eq!(
            day.add(&text("1")),
            Err(undefined("+", Some(DataType::Date), DataType::VARCHAR_MAX))
        );
        assert_eq!(day.neg(), Err(undefined("-", None, DataType::Date)));
    }

    #[test]
    fn date_arithmetic_with_null_should_be_null() {
        assert_eq!(date(2024, 1, 1).add(&Value::Null), Ok(Value::Null));
        assert_eq!(Value::Null.sub(&date(2024, 1, 1)), Ok(Value::Null));
    }

    #[test]
    fn cast_of_a_date_to_varchar_should_be_its_iso_text_cut_to_n() {
        assert_eq!(
            date(2024, 2, 29).cast(DataType::VARCHAR_MAX),
            Ok(text("2024-02-29"))
        );
        assert_eq!(date(2024, 2, 29).cast(varchar(4)), Ok(text("2024")));
    }

    #[test]
    fn cast_of_text_to_a_date_should_follow_the_input_rules() {
        assert_eq!(
            text(" 2024-02-29 ").cast(DataType::Date),
            Ok(date(2024, 2, 29))
        );
        assert_eq!(
            text("2024/02/29").cast(DataType::Date),
            Err(invalid_text(DataType::Date, "2024/02/29"))
        );
        assert_eq!(
            text("2023-02-29").cast(DataType::Date),
            Err(text_out_of_range(DataType::Date, "2023-02-29"))
        );
    }

    #[test]
    fn cast_of_a_date_to_a_date_should_be_identity() {
        assert_eq!(date(1, 1, 1).cast(DataType::Date), Ok(date(1, 1, 1)));
        assert_eq!(Value::Null.cast(DataType::Date), Ok(Value::Null));
    }

    #[test]
    fn cast_between_dates_and_numbers_or_booleans_should_be_impossible() {
        let cannot = |from, to| Err(TypeError::CannotCast { from, to });
        let day = date(2024, 2, 29);

        assert_eq!(day.cast(Integer), cannot(DataType::Date, Integer));
        assert_eq!(
            day.cast(DataType::Double),
            cannot(DataType::Date, DataType::Double)
        );
        assert_eq!(
            day.cast(DataType::Boolean),
            cannot(DataType::Date, DataType::Boolean)
        );
        assert_eq!(
            Value::Integer(19_782).cast(DataType::Date),
            cannot(Integer, DataType::Date)
        );
        assert_eq!(
            double(1.0).cast(DataType::Date),
            cannot(DataType::Double, DataType::Date)
        );
        assert_eq!(
            Value::Boolean(true).cast(DataType::Date),
            cannot(DataType::Boolean, DataType::Date)
        );
    }

    #[test]
    fn assign_of_dates_should_stay_within_their_family() {
        let mismatch = |value, target| Err(TypeError::DatatypeMismatch { value, target });

        assert_eq!(
            date(2024, 1, 1).assign(DataType::Date),
            Ok(date(2024, 1, 1))
        );
        assert_eq!(
            text("2024-01-01").assign(DataType::Date),
            mismatch(DataType::VARCHAR_MAX, DataType::Date)
        );
        assert_eq!(
            Value::Integer(1).assign(DataType::Date),
            mismatch(Integer, DataType::Date)
        );
        assert_eq!(
            date(2024, 1, 1).assign(varchar(10)),
            mismatch(DataType::Date, varchar(10))
        );
    }

    fn timestamp(day: (i32, u32, u32), hour: u32, minute: u32, second: u32, micro: u32) -> Value {
        let day = Date::from_ymd(day.0, day.1, day.2).unwrap();
        Value::Timestamp(Timestamp::new(day, hour, minute, second, micro).unwrap())
    }

    #[test]
    fn timestamp_should_have_the_timestamp_type() {
        assert_eq!(
            timestamp((2024, 3, 15), 14, 30, 0, 0).data_type(),
            Some(DataType::Timestamp)
        );
    }

    #[test]
    fn encode_of_timestamp_should_be_microseconds_since_1970_little_endian() {
        assert_eq!(
            timestamp((1970, 1, 1), 0, 0, 0, 1).encode().unwrap(),
            [1, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            timestamp((1969, 12, 31), 23, 59, 59, 999_999)
                .encode()
                .unwrap(),
            [0xFF; 8]
        );
    }

    #[test]
    fn decode_should_round_trip_timestamps() {
        let values = [
            Value::Timestamp(Timestamp::MIN),
            Value::Timestamp(Timestamp::MAX),
            timestamp((1970, 1, 1), 0, 0, 0, 0),
            timestamp((2024, 3, 15), 14, 30, 0, 250_000),
        ];

        for value in values {
            assert_eq!(
                Value::decode(DataType::Timestamp, value.encode().as_deref()),
                Ok(value)
            );
        }
    }

    #[test]
    fn decode_of_microseconds_outside_the_range_should_be_corrupted() {
        let min = Timestamp::MIN.micros_since_epoch();
        let max = Timestamp::MAX.micros_since_epoch();

        for micros in [min - 1, max + 1, i64::MIN, i64::MAX] {
            assert_eq!(
                Value::decode(DataType::Timestamp, Some(&micros.to_le_bytes())),
                Err(TypeError::CorruptedValue {
                    ty: DataType::Timestamp,
                    reason: "timestamp out of range"
                }),
                "{micros}"
            );
        }
    }

    #[test]
    fn decode_of_a_timestamp_with_the_wrong_width_should_fail() {
        for actual in [7, 9] {
            assert_eq!(
                Value::decode(DataType::Timestamp, Some(&vec![0; actual])),
                Err(TypeError::InvalidEncoding {
                    ty: DataType::Timestamp,
                    expected: 8,
                    actual
                })
            );
        }
    }

    #[test]
    fn parse_of_timestamp_should_read_the_three_forms() {
        let cases = [
            (
                "2024-03-15 14:30:00",
                timestamp((2024, 3, 15), 14, 30, 0, 0),
            ),
            (
                "2024-03-15T14:30:00",
                timestamp((2024, 3, 15), 14, 30, 0, 0),
            ),
            ("2024-03-15", timestamp((2024, 3, 15), 0, 0, 0, 0)),
            (
                "2024-03-15 14:30:00.25",
                timestamp((2024, 3, 15), 14, 30, 0, 250_000),
            ),
            (
                "2024-03-15T09:05:07.000001",
                timestamp((2024, 3, 15), 9, 5, 7, 1),
            ),
            (
                " 2024-03-15 14:30:00.123456 \t",
                timestamp((2024, 3, 15), 14, 30, 0, 123_456),
            ),
            (
                "1969-12-31 23:59:59.999999",
                timestamp((1969, 12, 31), 23, 59, 59, 999_999),
            ),
            ("0001-01-01 00:00:00", Value::Timestamp(Timestamp::MIN)),
            (
                "9999-12-31 23:59:59.999999",
                Value::Timestamp(Timestamp::MAX),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(
                Value::parse(DataType::Timestamp, input),
                Ok(expected),
                "{input:?}"
            );
        }
    }

    #[test]
    fn parse_of_timestamp_should_reject_other_formats() {
        let cases = [
            "",
            "14:30:00",
            "2024-03-15 14:30",
            "2024-03-15 2:30:00",
            "2024-03-15t14:30:00",
            "2024-03-15  14:30:00",
            "2024-03-15T",
            "2024-03-15_14:30:00",
            "2024-03-15 14:30:00.",
            "2024-03-15 14:30:00.1234567",
            "2024-03-15 14:30:00Z",
            "2024-03-15 14:30:00+01",
            "2024-03-15 14-30-00",
            "2024/03/15 14:30:00",
            "2024-3-15 14:30:00",
            "２０２４-03-15 14:30:00",
            "2023-02-29 25:00",
        ];

        for input in cases {
            let err = Value::parse(DataType::Timestamp, input).unwrap_err();

            assert_eq!(err, invalid_text(DataType::Timestamp, input), "{input:?}");
            assert_eq!(err.sqlstate(), "22007");
        }
    }

    #[test]
    fn parse_of_timestamp_should_reject_values_that_do_not_exist() {
        let cases = [
            "2023-02-29 10:00:00",
            "2023-02-29",
            "2024-03-15 24:00:00",
            "2024-03-15 10:60:00",
            "2024-03-15 10:00:60",
            "0000-01-01 00:00:00",
        ];

        for input in cases {
            let err = Value::parse(DataType::Timestamp, input).unwrap_err();

            assert_eq!(
                err,
                text_out_of_range(DataType::Timestamp, input),
                "{input:?}"
            );
            assert_eq!(err.sqlstate(), "22008");
        }
    }

    #[test]
    fn display_of_timestamp_should_use_a_space_and_trim_the_fraction() {
        assert_eq!(
            timestamp((2024, 3, 15), 14, 30, 0, 0).to_string(),
            "2024-03-15 14:30:00"
        );
        assert_eq!(
            timestamp((2024, 3, 15), 14, 30, 0, 250_000).to_string(),
            "2024-03-15 14:30:00.25"
        );
    }

    #[test]
    fn timestamps_should_round_trip_through_their_text() {
        for value in [
            Value::Timestamp(Timestamp::MIN),
            Value::Timestamp(Timestamp::MAX),
            timestamp((1969, 12, 31), 23, 59, 59, 999_999),
            timestamp((2024, 3, 15), 9, 5, 7, 1),
        ] {
            assert_eq!(
                Value::parse(DataType::Timestamp, &value.to_string()),
                Ok(value)
            );
        }
    }

    #[test]
    fn compare_of_timestamps_should_follow_the_instant() {
        let earlier = timestamp((2024, 3, 15), 14, 30, 0, 0);
        let later = timestamp((2024, 3, 15), 14, 30, 0, 1);

        assert_eq!(earlier.compare(&later), Ok(Some(Ordering::Less)));
        assert_eq!(later.compare(&earlier), Ok(Some(Ordering::Greater)));
        assert_eq!(earlier.compare(&earlier), Ok(Some(Ordering::Equal)));
    }

    #[test]
    fn compare_of_a_date_with_a_timestamp_should_use_midnight() {
        let cases = [
            (
                date(2024, 3, 15),
                timestamp((2024, 3, 15), 0, 0, 0, 0),
                Ordering::Equal,
            ),
            (
                date(2024, 3, 15),
                timestamp((2024, 3, 15), 0, 0, 0, 1),
                Ordering::Less,
            ),
            (
                date(2024, 3, 15),
                timestamp((2024, 3, 14), 23, 59, 59, 999_999),
                Ordering::Greater,
            ),
            (
                date(1969, 12, 31),
                timestamp((1969, 12, 31), 12, 0, 0, 0),
                Ordering::Less,
            ),
            (
                Value::Date(Date::MAX),
                Value::Timestamp(Timestamp::MAX),
                Ordering::Less,
            ),
            (
                Value::Date(Date::MIN),
                Value::Timestamp(Timestamp::MIN),
                Ordering::Equal,
            ),
        ];

        for (day, instant, expected) in cases {
            assert_eq!(
                day.compare(&instant),
                Ok(Some(expected)),
                "{day} vs {instant}"
            );
            assert_eq!(instant.compare(&day), Ok(Some(expected.reverse())));
        }
    }

    #[test]
    fn sql_comparisons_should_mix_dates_and_timestamps() {
        let midnight = timestamp((2024, 3, 15), 0, 0, 0, 0);

        assert_eq!(
            date(2024, 3, 15).sql_eq(&midnight),
            Ok(Value::Boolean(true))
        );
        assert_eq!(
            timestamp((2024, 3, 15), 9, 0, 0, 0).sql_gt(&date(2024, 3, 15)),
            Ok(Value::Boolean(true))
        );
        assert_eq!(date(2024, 3, 15).is_distinct_from(&midnight), Ok(false));
        assert_eq!(midnight.sql_lt(&Value::Null), Ok(Value::Null));
    }

    #[test]
    fn compare_of_a_timestamp_with_another_family_should_be_an_undefined_operator() {
        let instant = timestamp((2024, 3, 15), 0, 0, 0, 0);

        assert_eq!(
            instant.compare(&Value::BigInt(0)),
            Err(undefined("=", Some(DataType::Timestamp), BigInt))
        );
        assert_eq!(
            instant.sql_gt(&text("2024-03-15")),
            Err(undefined(
                ">",
                Some(DataType::Timestamp),
                DataType::VARCHAR_MAX
            ))
        );
    }

    #[test]
    fn sort_cmp_should_interleave_dates_and_timestamps_by_instant() {
        let mut values = vec![
            timestamp((2024, 3, 15), 12, 0, 0, 0),
            date(2024, 3, 15),
            Value::Null,
            Value::Boolean(true),
            date(2024, 3, 16),
            timestamp((2024, 3, 14), 23, 0, 0, 0),
        ];

        values.sort_by(Value::sort_cmp);

        assert_eq!(
            values,
            [
                Value::Boolean(true),
                timestamp((2024, 3, 14), 23, 0, 0, 0),
                date(2024, 3, 15),
                timestamp((2024, 3, 15), 12, 0, 0, 0),
                date(2024, 3, 16),
                Value::Null,
            ]
        );
    }

    #[test]
    fn arithmetic_with_timestamps_should_be_undefined_until_interval_exists() {
        let instant = timestamp((2024, 3, 15), 14, 30, 0, 0);
        let day = date(2024, 3, 15);
        let one = Value::Integer(1);
        let ts = DataType::Timestamp;

        let cases = [
            (instant.add(&one), undefined("+", Some(ts), Integer)),
            (one.add(&instant), undefined("+", Some(Integer), ts)),
            (instant.sub(&instant), undefined("-", Some(ts), ts)),
            (instant.sub(&day), undefined("-", Some(ts), DataType::Date)),
            (day.sub(&instant), undefined("-", Some(DataType::Date), ts)),
            (day.add(&instant), undefined("+", Some(DataType::Date), ts)),
            (instant.mul(&one), undefined("*", Some(ts), Integer)),
            (
                instant.div(&double(2.0)),
                undefined("/", Some(ts), DataType::Double),
            ),
            (instant.rem(&one), undefined("%", Some(ts), Integer)),
            (instant.neg(), undefined("-", None, ts)),
            (
                text("a").concat(&instant),
                undefined("||", Some(DataType::VARCHAR_MAX), ts),
            ),
        ];

        for (result, expected) in cases {
            assert_eq!(result, Err(expected));
        }
    }

    #[test]
    fn arithmetic_with_null_and_a_timestamp_should_be_null() {
        let instant = timestamp((2024, 3, 15), 14, 30, 0, 0);

        assert_eq!(instant.add(&Value::Null), Ok(Value::Null));
        assert_eq!(Value::Null.sub(&instant), Ok(Value::Null));
    }

    #[test]
    fn cast_of_a_timestamp_to_a_date_should_keep_the_day() {
        let cases = [
            (
                timestamp((2024, 3, 15), 23, 59, 59, 999_999),
                date(2024, 3, 15),
            ),
            (timestamp((1969, 12, 31), 12, 0, 0, 0), date(1969, 12, 31)),
            (Value::Timestamp(Timestamp::MIN), Value::Date(Date::MIN)),
            (Value::Timestamp(Timestamp::MAX), Value::Date(Date::MAX)),
        ];

        for (instant, day) in cases {
            assert_eq!(instant.cast(DataType::Date), Ok(day), "{instant}");
        }
    }

    #[test]
    fn cast_of_a_date_to_a_timestamp_should_be_midnight() {
        assert_eq!(
            date(2024, 3, 15).cast(DataType::Timestamp),
            Ok(timestamp((2024, 3, 15), 0, 0, 0, 0))
        );
        assert_eq!(
            Value::Date(Date::MIN).cast(DataType::Timestamp),
            Ok(Value::Timestamp(Timestamp::MIN))
        );
    }

    #[test]
    fn cast_of_a_timestamp_to_varchar_should_be_its_text_cut_to_n() {
        let instant = timestamp((2024, 3, 15), 14, 30, 0, 250_000);

        assert_eq!(
            instant.cast(DataType::VARCHAR_MAX),
            Ok(text("2024-03-15 14:30:00.25"))
        );
        assert_eq!(instant.cast(varchar(10)), Ok(text("2024-03-15")));
    }

    #[test]
    fn cast_of_text_to_a_timestamp_should_follow_the_input_rules() {
        assert_eq!(
            text("2024-03-15T14:30:00").cast(DataType::Timestamp),
            Ok(timestamp((2024, 3, 15), 14, 30, 0, 0))
        );
        assert_eq!(
            text("2024-03-15").cast(DataType::Timestamp),
            Ok(timestamp((2024, 3, 15), 0, 0, 0, 0))
        );
        assert_eq!(
            text("soon").cast(DataType::Timestamp),
            Err(invalid_text(DataType::Timestamp, "soon"))
        );
    }

    #[test]
    fn cast_of_a_timestamp_to_a_timestamp_should_be_identity() {
        let instant = timestamp((2024, 3, 15), 14, 30, 0, 0);

        assert_eq!(instant.cast(DataType::Timestamp), Ok(instant.clone()));
        assert_eq!(Value::Null.cast(DataType::Timestamp), Ok(Value::Null));
    }

    #[test]
    fn cast_between_timestamps_and_numbers_or_booleans_should_be_impossible() {
        let cannot = |from, to| Err(TypeError::CannotCast { from, to });
        let instant = timestamp((2024, 3, 15), 14, 30, 0, 0);
        let ts = DataType::Timestamp;

        assert_eq!(instant.cast(BigInt), cannot(ts, BigInt));
        assert_eq!(instant.cast(DataType::Double), cannot(ts, DataType::Double));
        assert_eq!(
            instant.cast(DataType::Boolean),
            cannot(ts, DataType::Boolean)
        );
        assert_eq!(Value::BigInt(0).cast(ts), cannot(BigInt, ts));
        assert_eq!(real(0.0).cast(ts), cannot(DataType::Real, ts));
        assert_eq!(Value::Boolean(true).cast(ts), cannot(DataType::Boolean, ts));
    }

    #[test]
    fn assign_should_widen_a_date_to_a_timestamp_but_not_narrow_back() {
        let mismatch = |value, target| Err(TypeError::DatatypeMismatch { value, target });
        let instant = timestamp((2024, 3, 15), 14, 30, 0, 0);

        assert_eq!(
            date(2024, 3, 15).assign(DataType::Timestamp),
            Ok(timestamp((2024, 3, 15), 0, 0, 0, 0))
        );
        assert_eq!(
            instant.assign(DataType::Date),
            mismatch(DataType::Timestamp, DataType::Date)
        );
        assert_eq!(instant.assign(DataType::Timestamp), Ok(instant.clone()));
        assert_eq!(
            text("2024-03-15 14:30:00").assign(DataType::Timestamp),
            mismatch(DataType::VARCHAR_MAX, DataType::Timestamp)
        );
        assert_eq!(
            instant.assign(varchar(30)),
            mismatch(DataType::Timestamp, varchar(30))
        );
    }

    #[test]
    fn logical_operators_and_tests_with_timestamps_should_fail() {
        let instant = timestamp((2024, 3, 15), 14, 30, 0, 0);
        let and = instant.and(&Value::Boolean(true));

        assert_eq!(and, Err(not_boolean("AND", DataType::Timestamp)));
        assert_eq!(
            and.unwrap_err().to_string(),
            "argument of AND must be type boolean, not type timestamp"
        );
        assert_eq!(
            instant.is_true(),
            Err(not_boolean("IS TRUE", DataType::Timestamp))
        );
    }

    #[test]
    fn logical_operators_and_tests_with_dates_should_fail() {
        let and = date(2024, 1, 1).and(&Value::Boolean(true));

        assert_eq!(and, Err(not_boolean("AND", DataType::Date)));
        assert_eq!(
            and.unwrap_err().to_string(),
            "argument of AND must be type boolean, not type date"
        );
        assert_eq!(
            date(2024, 1, 1).is_true(),
            Err(not_boolean("IS TRUE", DataType::Date))
        );
    }

    #[test]
    fn concat_with_a_date_should_be_an_undefined_operator() {
        assert_eq!(
            text("a").concat(&date(2024, 1, 1)),
            Err(undefined("||", Some(DataType::VARCHAR_MAX), DataType::Date))
        );
    }

    #[test]
    fn concat_with_a_float_should_be_an_undefined_operator() {
        assert_eq!(
            text("a").concat(&double(1.5)),
            Err(undefined(
                "||",
                Some(DataType::VARCHAR_MAX),
                DataType::Double
            ))
        );
        assert_eq!(
            real(1.5).concat(&real(1.5)),
            Err(undefined("||", Some(DataType::Real), DataType::Real))
        );
    }

    #[test]
    fn logical_operators_and_tests_with_floats_should_fail() {
        let and = double(1.0).and(&Value::Boolean(true));

        assert_eq!(and, Err(not_boolean("AND", DataType::Double)));
        assert_eq!(
            and.unwrap_err().to_string(),
            "argument of AND must be type boolean, not type double precision"
        );
        assert_eq!(real(0.0).not(), Err(not_boolean("NOT", DataType::Real)));
        assert_eq!(
            double(1.0).is_true(),
            Err(not_boolean("IS TRUE", DataType::Double))
        );
    }

    #[test]
    fn parse_of_floats_should_accept_a_signed_exponent() {
        assert_eq!(Value::parse(DataType::Double, "1e+5"), Ok(double(1e5)));
        assert_eq!(Value::parse(DataType::Real, "-2E+3"), Ok(real(-2000.0)));
    }

    fn boolean_out_of_range() -> TypeError {
        TypeError::OutOfRange {
            ty: DataType::Boolean,
        }
    }

    #[test]
    fn cast_of_0_and_1_to_boolean_should_work_for_every_integer_width() {
        for (zero, one) in [
            (Value::SmallInt(0), Value::SmallInt(1)),
            (Value::Integer(0), Value::Integer(1)),
            (Value::BigInt(0), Value::BigInt(1)),
        ] {
            assert_eq!(zero.cast(DataType::Boolean), Ok(Value::Boolean(false)));
            assert_eq!(one.cast(DataType::Boolean), Ok(Value::Boolean(true)));
        }
    }

    #[test]
    fn cast_of_other_integers_to_boolean_should_be_out_of_range() {
        let cases = [
            Value::SmallInt(-1),
            Value::SmallInt(2),
            Value::SmallInt(i16::MIN),
            Value::Integer(i32::MAX),
            Value::BigInt(i64::MIN),
            Value::BigInt(10),
        ];

        for value in cases {
            assert_eq!(
                value.cast(DataType::Boolean),
                Err(boolean_out_of_range()),
                "{value:?}"
            );
        }
    }

    #[test]
    fn cast_of_boolean_to_an_integer_should_be_0_or_1() {
        assert_eq!(Value::Boolean(false).cast(SmallInt), Ok(Value::SmallInt(0)));
        assert_eq!(Value::Boolean(true).cast(Integer), Ok(Value::Integer(1)));
        assert_eq!(Value::Boolean(true).cast(BigInt), Ok(Value::BigInt(1)));
    }

    #[test]
    fn cast_of_text_to_boolean_should_follow_the_input_rules() {
        assert_eq!(
            text("TRUE").cast(DataType::Boolean),
            Ok(Value::Boolean(true))
        );
        assert_eq!(
            text(" 0 ").cast(DataType::Boolean),
            Ok(Value::Boolean(false))
        );
        assert_eq!(
            text("yes").cast(DataType::Boolean),
            Err(invalid_text(DataType::Boolean, "yes"))
        );
    }

    #[test]
    fn cast_of_boolean_to_varchar_should_be_the_sql_literal_cut_to_n() {
        let cases = [
            (true, DataType::VARCHAR_MAX, "TRUE"),
            (false, DataType::VARCHAR_MAX, "FALSE"),
            (true, varchar(1), "T"),
            (false, varchar(4), "FALS"),
            (false, varchar(5), "FALSE"),
        ];

        for (value, target, expected) in cases {
            assert_eq!(Value::Boolean(value).cast(target), Ok(text(expected)));
        }
    }

    #[test]
    fn cast_of_boolean_to_boolean_should_be_identity() {
        for value in [false, true] {
            assert_eq!(
                Value::Boolean(value).cast(DataType::Boolean),
                Ok(Value::Boolean(value))
            );
        }
        assert_eq!(Value::Null.cast(DataType::Boolean), Ok(Value::Null));
    }

    #[test]
    fn assign_of_boolean_should_stay_within_its_family() {
        let mismatch = |value, target| Err(TypeError::DatatypeMismatch { value, target });

        assert_eq!(
            Value::Boolean(true).assign(DataType::Boolean),
            Ok(Value::Boolean(true))
        );
        assert_eq!(Value::Null.assign(DataType::Boolean), Ok(Value::Null));
        assert_eq!(
            Value::Boolean(true).assign(Integer),
            mismatch(DataType::Boolean, Integer)
        );
        assert_eq!(
            Value::Integer(1).assign(DataType::Boolean),
            mismatch(Integer, DataType::Boolean)
        );
        assert_eq!(
            Value::Boolean(false).assign(varchar(5)),
            mismatch(DataType::Boolean, varchar(5))
        );
        assert_eq!(
            text("true").assign(DataType::Boolean),
            mismatch(DataType::VARCHAR_MAX, DataType::Boolean)
        );
    }

    /// TRUE, FALSE and UNKNOWN, in the order of the truth tables below.
    const TRUTH_VALUES: [Value; 3] = [Value::Boolean(true), Value::Boolean(false), Value::Null];

    fn truth(value: Option<bool>) -> Value {
        value.map_or(Value::Null, Value::Boolean)
    }

    fn not_boolean(op: &'static str, actual: DataType) -> TypeError {
        TypeError::NotBoolean { op, actual }
    }

    #[test]
    fn and_should_follow_the_three_valued_truth_table() {
        let (t, f, n) = (Some(true), Some(false), None);
        let table = [[t, f, n], [f, f, f], [n, f, n]];

        for (lhs, row) in TRUTH_VALUES.iter().zip(table) {
            for (rhs, expected) in TRUTH_VALUES.iter().zip(row) {
                assert_eq!(lhs.and(rhs), Ok(truth(expected)), "{lhs} AND {rhs}");
            }
        }
    }

    #[test]
    fn or_should_follow_the_three_valued_truth_table() {
        let (t, f, n) = (Some(true), Some(false), None);
        let table = [[t, t, t], [t, f, n], [t, n, n]];

        for (lhs, row) in TRUTH_VALUES.iter().zip(table) {
            for (rhs, expected) in TRUTH_VALUES.iter().zip(row) {
                assert_eq!(lhs.or(rhs), Ok(truth(expected)), "{lhs} OR {rhs}");
            }
        }
    }

    #[test]
    fn not_should_flip_true_and_false_and_keep_unknown() {
        assert_eq!(Value::Boolean(true).not(), Ok(Value::Boolean(false)));
        assert_eq!(Value::Boolean(false).not(), Ok(Value::Boolean(true)));
        assert_eq!(Value::Null.not(), Ok(Value::Null));
    }

    #[test]
    fn logical_operators_with_a_non_boolean_should_fail_even_if_the_other_side_decides() {
        assert_eq!(
            Value::Boolean(false).and(&Value::Integer(0)),
            Err(not_boolean("AND", Integer))
        );
        assert_eq!(
            text("x").and(&Value::Null),
            Err(not_boolean("AND", DataType::VARCHAR_MAX))
        );
        assert_eq!(
            Value::BigInt(1).or(&Value::Boolean(true)),
            Err(not_boolean("OR", BigInt))
        );
        assert_eq!(Value::SmallInt(0).not(), Err(not_boolean("NOT", SmallInt)));
    }

    #[test]
    fn boolean_tests_should_never_be_unknown() {
        let expected = [
            (Value::Boolean(true), [true, false, false]),
            (Value::Boolean(false), [false, true, false]),
            (Value::Null, [false, false, true]),
        ];

        for (value, [is_true, is_false, is_unknown]) in expected {
            assert_eq!(value.is_true(), Ok(is_true), "{value} IS TRUE");
            assert_eq!(value.is_false(), Ok(is_false), "{value} IS FALSE");
            assert_eq!(value.is_unknown(), Ok(is_unknown), "{value} IS UNKNOWN");
        }
    }

    #[test]
    fn boolean_tests_of_a_non_boolean_should_fail() {
        assert_eq!(
            Value::Integer(1).is_true(),
            Err(not_boolean("IS TRUE", Integer))
        );
        assert_eq!(
            text("false").is_false(),
            Err(not_boolean("IS FALSE", DataType::VARCHAR_MAX))
        );
        assert_eq!(
            Value::BigInt(0).is_unknown(),
            Err(not_boolean("IS UNKNOWN", BigInt))
        );
    }

    #[test]
    fn arithmetic_with_booleans_should_be_an_undefined_operator() {
        let (yes, one) = (Value::Boolean(true), Value::Integer(1));

        for (name, op) in BINARY_OPS {
            assert_eq!(
                op(&yes, &one),
                Err(undefined(name, Some(DataType::Boolean), Integer))
            );
            assert_eq!(
                op(&yes, &yes),
                Err(undefined(name, Some(DataType::Boolean), DataType::Boolean))
            );
        }
        assert_eq!(yes.neg(), Err(undefined("-", None, DataType::Boolean)));
        assert_eq!(
            yes.neg().unwrap_err().to_string(),
            "operator does not exist: - boolean"
        );
    }

    #[test]
    fn concat_with_a_boolean_should_be_an_undefined_operator() {
        let yes = Value::Boolean(true);

        assert_eq!(
            text("a").concat(&yes),
            Err(undefined(
                "||",
                Some(DataType::VARCHAR_MAX),
                DataType::Boolean
            ))
        );
        assert_eq!(
            yes.concat(&yes),
            Err(undefined("||", Some(DataType::Boolean), DataType::Boolean))
        );
    }

    #[test]
    fn logical_operators_with_null_and_a_non_boolean_should_fail() {
        assert_eq!(
            Value::Null.and(&Value::Integer(1)),
            Err(not_boolean("AND", Integer))
        );
        assert_eq!(
            Value::Integer(1).or(&Value::Null),
            Err(not_boolean("OR", Integer))
        );
        assert_eq!(
            Value::Null.or(&text("true")),
            Err(not_boolean("OR", DataType::VARCHAR_MAX))
        );
    }

    type SqlComparison = fn(&Value, &Value) -> Result<Value, TypeError>;

    /// Each operator with the orderings for which it holds.
    const SQL_COMPARISONS: [(&str, SqlComparison, [bool; 3]); 6] = [
        ("=", Value::sql_eq, [false, true, false]),
        ("<>", Value::sql_ne, [true, false, true]),
        ("<", Value::sql_lt, [true, false, false]),
        ("<=", Value::sql_le, [true, true, false]),
        (">", Value::sql_gt, [false, false, true]),
        (">=", Value::sql_ge, [false, true, true]),
    ];

    fn holds(expected: [bool; 3], ordering: Ordering) -> bool {
        match ordering {
            Ordering::Less => expected[0],
            Ordering::Equal => expected[1],
            Ordering::Greater => expected[2],
        }
    }

    #[test]
    fn compare_of_booleans_should_put_false_before_true() {
        let (no, yes) = (Value::Boolean(false), Value::Boolean(true));

        assert_eq!(no.compare(&yes), Ok(Some(Ordering::Less)));
        assert_eq!(yes.compare(&yes), Ok(Some(Ordering::Equal)));
        assert_eq!(yes.compare(&no), Ok(Some(Ordering::Greater)));
    }

    #[test]
    fn compare_of_a_boolean_with_another_family_should_be_an_undefined_operator() {
        assert_eq!(
            Value::Boolean(true).compare(&Value::Integer(1)),
            Err(undefined("=", Some(DataType::Boolean), Integer))
        );
        assert_eq!(
            text("true").compare(&Value::Boolean(true)),
            Err(undefined(
                "=",
                Some(DataType::VARCHAR_MAX),
                DataType::Boolean
            ))
        );
    }

    #[test]
    fn sql_comparisons_should_return_sql_booleans() {
        let pairs = [
            (Value::Integer(1), Value::BigInt(2), Ordering::Less),
            (Value::SmallInt(5), Value::Integer(5), Ordering::Equal),
            (text("b"), text("a"), Ordering::Greater),
            (text("a"), text("a "), Ordering::Less),
            (Value::Boolean(false), Value::Boolean(true), Ordering::Less),
            (Value::Boolean(true), Value::Boolean(true), Ordering::Equal),
        ];

        for (lhs, rhs, ordering) in pairs {
            for (name, op, expected) in SQL_COMPARISONS {
                assert_eq!(
                    op(&lhs, &rhs),
                    Ok(Value::Boolean(holds(expected, ordering))),
                    "{lhs:?} {name} {rhs:?}"
                );
            }
        }
    }

    #[test]
    fn sql_comparisons_with_null_should_be_unknown() {
        for (name, op, _) in SQL_COMPARISONS {
            for value in [
                Value::Integer(1),
                text(""),
                Value::Boolean(false),
                Value::Null,
            ] {
                assert_eq!(
                    op(&value, &Value::Null),
                    Ok(Value::Null),
                    "{value:?} {name}"
                );
                assert_eq!(
                    op(&Value::Null, &value),
                    Ok(Value::Null),
                    "{name} {value:?}"
                );
            }
        }
    }

    #[test]
    fn sql_comparisons_across_families_should_name_the_operator() {
        for (name, op, _) in SQL_COMPARISONS {
            assert_eq!(
                op(&Value::Integer(1), &text("1")),
                Err(undefined(name, Some(Integer), DataType::VARCHAR_MAX))
            );
            assert_eq!(
                op(&Value::Boolean(true), &Value::SmallInt(1)),
                Err(undefined(name, Some(DataType::Boolean), SmallInt))
            );
        }
    }

    #[test]
    fn is_distinct_from_should_compare_booleans() {
        let cases = [
            (Value::Boolean(true), Value::Boolean(true), false),
            (Value::Boolean(true), Value::Boolean(false), true),
            (Value::Boolean(false), Value::Null, true),
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
    fn sort_cmp_should_order_integers_then_text_then_booleans_then_null() {
        let mut values = vec![
            Value::Null,
            Value::Boolean(true),
            text("a"),
            Value::Boolean(false),
            Value::Integer(7),
        ];

        values.sort_by(Value::sort_cmp);

        assert_eq!(
            values,
            [
                Value::Integer(7),
                text("a"),
                Value::Boolean(false),
                Value::Boolean(true),
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
            Value::Real(_)
            | Value::Double(_)
            | Value::Text(_)
            | Value::Boolean(_)
            | Value::Date(_)
            | Value::Timestamp(_) => {
                unreachable!("integer model only")
            }
        }
    }

    fn model_narrow(ty: DataType, value: i128) -> Result<Value, TypeError> {
        let out_of_range = |_| TypeError::OutOfRange { ty };
        Ok(match ty {
            SmallInt => Value::SmallInt(i16::try_from(value).map_err(out_of_range)?),
            Integer => Value::Integer(i32::try_from(value).map_err(out_of_range)?),
            BigInt => Value::BigInt(i64::try_from(value).map_err(out_of_range)?),
            DataType::Real
            | DataType::Double
            | DataType::Varchar(_)
            | DataType::Boolean
            | DataType::Date
            | DataType::Timestamp => unreachable!("integer model only"),
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

    /// Integers (with NULL), text and booleans, so operators meet every family pair.
    fn random_mixed_value(rng: &mut Rng) -> Value {
        match rng.below(8) {
            0 => text(&random_text(rng)),
            1 => Value::Boolean(rng.below(2) == 0),
            2 => random_float(rng),
            3 => random_date(rng),
            4 => random_timestamp(rng),
            _ => random_value(rng),
        }
    }

    /// Anywhere in the range, or a microsecond around some midnight.
    fn random_timestamp(rng: &mut Rng) -> Value {
        let (min, max) = (
            Timestamp::MIN.micros_since_epoch(),
            Timestamp::MAX.micros_since_epoch(),
        );
        let micros = match rng.below(2) {
            0 => {
                let Value::Date(day) = random_date(rng) else {
                    unreachable!()
                };
                let midnight = i64::from(day.days_since_epoch()) * 86_400_000_000;
                (midnight + rng.below(3) as i64 - 1).clamp(min, max)
            }
            _ => min + (rng.next() % (max - min + 1).unsigned_abs()) as i64,
        };
        Value::Timestamp(Timestamp::from_micros_since_epoch(micros).unwrap())
    }

    fn model_instant(value: &Value) -> i64 {
        match value {
            Value::Date(day) => i64::from(day.days_since_epoch()) * 86_400_000_000,
            Value::Timestamp(instant) => instant.micros_since_epoch(),
            other => unreachable!("datetimes only: {other:?}"),
        }
    }

    #[test]
    fn random_timestamps_should_round_trip_through_bytes_and_text() {
        let mut rng = Rng(0x1319_8A2E_0370_7344);

        for _ in 0..10_000 {
            let value = random_timestamp(&mut rng);

            assert_eq!(
                Value::decode(DataType::Timestamp, value.encode().as_deref()).as_ref(),
                Ok(&value)
            );
            assert_eq!(
                Value::parse(DataType::Timestamp, &value.to_string()).as_ref(),
                Ok(&value),
                "{value}"
            );
        }
    }

    #[test]
    fn random_datetime_comparisons_should_match_the_instant_model() {
        let mut rng = Rng(0xA409_3822_299F_31D0);

        for _ in 0..10_000 {
            let [lhs, rhs] = [(); 2].map(|_| match rng.below(2) {
                0 => random_date(&mut rng),
                _ => random_timestamp(&mut rng),
            });

            assert_eq!(
                lhs.compare(&rhs),
                Ok(Some(model_instant(&lhs).cmp(&model_instant(&rhs)))),
                "{lhs} vs {rhs}"
            );
        }
    }

    /// Any day of the range, half of them near its ends.
    fn random_date(rng: &mut Rng) -> Value {
        let (min, max) = (Date::MIN.days_since_epoch(), Date::MAX.days_since_epoch());
        let offset = i32::try_from(rng.below(30)).unwrap();
        let days = match rng.below(4) {
            0 => min + offset,
            1 => max - offset,
            _ => min + i32::try_from(rng.next() % (max - min + 1).unsigned_abs() as u64).unwrap(),
        };
        Value::Date(Date::from_days_since_epoch(days).unwrap())
    }

    fn model_days(value: &Value) -> i64 {
        match value {
            Value::Date(day) => day.days_since_epoch().into(),
            other => unreachable!("dates only: {other:?}"),
        }
    }

    #[test]
    fn random_date_arithmetic_should_match_an_i64_model() {
        let mut rng = Rng(0xBB67_AE85_84CA_A73B);
        let in_range = |days: i64| {
            i32::try_from(days)
                .ok()
                .and_then(Date::from_days_since_epoch)
                .map(Value::Date)
                .ok_or(TypeError::OutOfRange { ty: DataType::Date })
        };

        for _ in 0..10_000 {
            let (day, other) = (random_date(&mut rng), random_date(&mut rng));
            let offset: i64 = match rng.below(3) {
                0 => rng.below(61) as i64 - 30,
                1 => [i64::MIN, i64::MAX, 3_652_058, -3_652_058][rng.below(4)],
                _ => (rng.next() as i64) >> rng.below(64),
            };
            let days = model_days(&day);

            assert_eq!(
                day.add(&Value::BigInt(offset)),
                days.checked_add(offset)
                    .map_or(Err(TypeError::OutOfRange { ty: DataType::Date }), in_range),
                "{day} + {offset}"
            );
            assert_eq!(
                day.sub(&Value::BigInt(offset)),
                days.checked_sub(offset)
                    .map_or(Err(TypeError::OutOfRange { ty: DataType::Date }), in_range),
                "{day} - {offset}"
            );
            assert_eq!(
                day.sub(&other),
                Ok(Value::Integer(
                    i32::try_from(days - model_days(&other)).unwrap()
                ))
            );
        }
    }

    #[test]
    fn random_dates_should_round_trip_through_bytes_and_text() {
        let mut rng = Rng(0x3C6E_F372_FE94_F82B);

        for _ in 0..10_000 {
            let value = random_date(&mut rng);

            assert_eq!(
                Value::decode(DataType::Date, value.encode().as_deref()).as_ref(),
                Ok(&value)
            );
            assert_eq!(
                Value::parse(DataType::Date, &value.to_string()).as_ref(),
                Ok(&value)
            );
        }
    }

    /// REAL or DOUBLE PRECISION: limits, subnormals, round values and random bits.
    fn random_float(rng: &mut Rng) -> Value {
        let raw = match rng.below(4) {
            0 => [
                0.0,
                0.1,
                -1.5,
                f64::MAX,
                -f64::MAX,
                f64::MIN_POSITIVE,
                f64::from_bits(1),
                1e-40,
                3.4e38,
                1e300,
            ][rng.below(10)],
            1 => rng.below(41) as f64 / 4.0 - 5.0,
            2 => (rng.next() as i64) as f64,
            _ => f64::from_bits(rng.next()),
        };
        let value = if rng.below(2) == 0 {
            Finite32::new(raw as f32).map(Value::Real)
        } else {
            Finite64::new(raw).map(Value::Double)
        };
        value.unwrap_or_else(|| double(1.0))
    }

    fn model_as_f64(value: &Value) -> Option<f64> {
        match *value {
            Value::Null => None,
            Value::SmallInt(v) => Some(v.into()),
            Value::Integer(v) => Some(v.into()),
            Value::BigInt(v) => Some(v as f64),
            Value::Real(v) => Some(v.get().into()),
            Value::Double(v) => Some(v.get()),
            Value::Text(_) | Value::Boolean(_) | Value::Date(_) | Value::Timestamp(_) => {
                unreachable!("numeric model only")
            }
        }
    }

    /// IEEE 754 arithmetic in the native type of the result (f32 for REAL op REAL, so the
    /// "compute in f64, round to f32" claim is checked), plus the SQL rules written apart.
    fn model_float_arithmetic(name: &str, lhs: &Value, rhs: &Value) -> Result<Value, TypeError> {
        let (Some(l), Some(r)) = (model_as_f64(lhs), model_as_f64(rhs)) else {
            return Ok(Value::Null);
        };
        if name == "%" {
            return Err(undefined("%", lhs.data_type(), rhs.data_type().unwrap()));
        }
        if name == "/" && r == 0.0 {
            return Err(TypeError::DivisionByZero);
        }
        let is_real = matches!((lhs, rhs), (Value::Real(_), Value::Real(_)));
        let underflow_needs_zero = |a: f64, b: f64| match name {
            "*" => a != 0.0 && b != 0.0,
            "/" => a != 0.0,
            _ => false,
        };
        if is_real {
            let (a, b) = (l as f32, r as f32);
            let result = match name {
                "+" => a + b,
                "-" => a - b,
                "*" => a * b,
                _ => a / b,
            };
            let is_underflow = result == 0.0 && underflow_needs_zero(l, r);
            return match Finite32::new(result) {
                Some(value) if !is_underflow => Ok(Value::Real(value)),
                _ => Err(TypeError::OutOfRange { ty: DataType::Real }),
            };
        }
        let result = match name {
            "+" => l + r,
            "-" => l - r,
            "*" => l * r,
            _ => l / r,
        };
        let is_underflow = result == 0.0 && underflow_needs_zero(l, r);
        match Finite64::new(result) {
            Some(value) if !is_underflow => Ok(Value::Double(value)),
            _ => Err(TypeError::OutOfRange {
                ty: DataType::Double,
            }),
        }
    }

    #[test]
    fn random_float_arithmetic_should_match_native_ieee_754_arithmetic() {
        let mut rng = Rng(0x510E_527F_ADE6_82D1);

        for _ in 0..10_000 {
            let [lhs, rhs] = [(); 2].map(|_| match rng.below(3) {
                0 => random_value(&mut rng),
                _ => random_float(&mut rng),
            });
            let both_integers = [&lhs, &rhs]
                .iter()
                .all(|value| value.data_type().is_some_and(DataType::is_integer));
            if both_integers {
                continue;
            }
            let (name, op) = BINARY_OPS[rng.below(BINARY_OPS.len())];

            assert_eq!(
                op(&lhs, &rhs),
                model_float_arithmetic(name, &lhs, &rhs),
                "{lhs:?} {name} {rhs:?}"
            );
        }
    }

    /// Exact order of an integer and a finite float through i128 and `floor`, a different
    /// route from the implementation's `trunc` in `[-2^63, 2^63)`.
    fn model_compare_integer_with_float(integer: i64, float: f64) -> Ordering {
        let floor = float.floor();
        // Saturates for huge floats, which still orders them past every i64.
        let floor_integer = floor as i128;
        let has_fraction = float != floor;
        i128::from(integer)
            .cmp(&floor_integer)
            .then(if has_fraction {
                Ordering::Less
            } else {
                Ordering::Equal
            })
    }

    #[test]
    fn random_integer_float_comparisons_should_match_an_exact_i128_model() {
        let mut rng = Rng(0x9B05_688C_2B3E_6C1F);
        let interesting = [
            i64::MIN,
            i64::MIN + 1,
            -TWO_POW_53 - 1,
            -TWO_POW_53,
            -1,
            0,
            1,
            TWO_POW_53,
            TWO_POW_53 + 1,
            i64::MAX - 1,
            i64::MAX,
        ];

        for _ in 0..10_000 {
            let integer = match rng.below(2) {
                0 => interesting[rng.below(interesting.len())],
                _ => rng.next() as i64,
            };
            let float = match rng.below(4) {
                0 => integer as f64 + [-1.5, -1.0, -0.5, 0.0, 0.5, 1.0][rng.below(6)],
                1 => [TWO_POW_63, -TWO_POW_63, 1e300, -1e300, 0.25][rng.below(5)],
                2 => (rng.next() as i64) as f64,
                _ => f64::from_bits(rng.next()),
            };
            let Some(finite) = Finite64::new(float) else {
                continue;
            };

            assert_eq!(
                Value::BigInt(integer).compare(&Value::Double(finite)),
                Ok(Some(model_compare_integer_with_float(
                    integer,
                    finite.get()
                ))),
                "{integer} vs {float:e}"
            );
            if let Some(narrow) = Finite32::new(float as f32) {
                assert_eq!(
                    Value::BigInt(integer).compare(&Value::Real(narrow)),
                    Ok(Some(model_compare_integer_with_float(
                        integer,
                        narrow.get().into()
                    )))
                );
            }
        }
    }

    #[test]
    fn random_floats_should_round_trip_through_bytes_and_text() {
        let mut rng = Rng(0x1F83_D9AB_FB41_BD6B);

        for _ in 0..10_000 {
            let value = random_float(&mut rng);
            let ty = value.data_type().unwrap();

            assert_eq!(
                Value::decode(ty, value.encode().as_deref()).as_ref(),
                Ok(&value)
            );
            assert_eq!(
                Value::parse(ty, &value.to_string()).as_ref(),
                Ok(&value),
                "{value}"
            );
        }
    }

    fn random_truth(rng: &mut Rng) -> Value {
        TRUTH_VALUES[rng.below(TRUTH_VALUES.len())].clone()
    }

    /// Kleene's logic as an order FALSE < UNKNOWN < TRUE: AND is the minimum, OR the maximum
    /// and NOT the mirror. Independent of the `match` tables in `boolean.rs`.
    fn kleene_rank(value: &Value) -> u8 {
        match value {
            Value::Boolean(false) => 0,
            Value::Null => 1,
            Value::Boolean(true) => 2,
            other => unreachable!("not a truth value: {other:?}"),
        }
    }

    fn from_kleene_rank(rank: u8) -> Value {
        [Value::Boolean(false), Value::Null, Value::Boolean(true)][usize::from(rank)].clone()
    }

    #[test]
    fn random_logic_should_match_the_kleene_order_model() {
        let mut rng = Rng(0x3C6E_F372_FE94_F82B);

        for _ in 0..10_000 {
            let (a, b) = (random_truth(&mut rng), random_truth(&mut rng));
            let (rank_a, rank_b) = (kleene_rank(&a), kleene_rank(&b));

            assert_eq!(
                a.and(&b),
                Ok(from_kleene_rank(rank_a.min(rank_b))),
                "{a} AND {b}"
            );
            assert_eq!(
                a.or(&b),
                Ok(from_kleene_rank(rank_a.max(rank_b))),
                "{a} OR {b}"
            );
            assert_eq!(a.not(), Ok(from_kleene_rank(2 - rank_a)), "NOT {a}");
            assert_eq!(a.and(&b), b.and(&a));
            assert_eq!(a.or(&b), b.or(&a));
            assert_eq!(a.not().unwrap().not(), Ok(a.clone()));
            assert_eq!(
                a.and(&b).unwrap().not(),
                a.not().unwrap().or(&b.not().unwrap())
            );
            let tests = [a.is_true(), a.is_false(), a.is_unknown()];
            assert_eq!(tests.iter().filter(|test| **test == Ok(true)).count(), 1);
        }
    }

    #[test]
    fn random_sql_comparisons_should_agree_with_compare() {
        let mut rng = Rng(0xBB67_AE85_84CA_A73B);

        for _ in 0..10_000 {
            let (lhs, rhs) = (random_mixed_value(&mut rng), random_mixed_value(&mut rng));
            let (name, op, expected) = SQL_COMPARISONS[rng.below(SQL_COMPARISONS.len())];

            let result = op(&lhs, &rhs);

            let expected = match lhs.compare(&rhs) {
                Ok(Some(ordering)) => Ok(Value::Boolean(holds(expected, ordering))),
                Ok(None) => Ok(Value::Null),
                Err(TypeError::UndefinedOperator { left, right, .. }) => {
                    Err(TypeError::UndefinedOperator {
                        op: name,
                        left,
                        right,
                    })
                }
                Err(other) => panic!("unexpected compare error {other:?}"),
            };
            assert_eq!(result, expected, "{lhs:?} {name} {rhs:?}");
        }
    }

    #[test]
    fn booleans_should_round_trip_through_bytes_text_and_integers() {
        for value in [Value::Boolean(false), Value::Boolean(true)] {
            assert_eq!(
                Value::decode(DataType::Boolean, value.encode().as_deref()).as_ref(),
                Ok(&value)
            );
            assert_eq!(
                Value::parse(DataType::Boolean, &value.to_string()).as_ref(),
                Ok(&value)
            );
            for width in [SmallInt, Integer, BigInt] {
                let number = value.cast(width).unwrap();

                assert_eq!(number.cast(DataType::Boolean).as_ref(), Ok(&value));
            }
        }
    }

    #[test]
    fn sort_cmp_should_be_a_total_order() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);

        for _ in 0..10_000 {
            let [a, b, c] = [(); 3].map(|_| random_mixed_value(&mut rng));

            assert_eq!(a.sort_cmp(&a), Ordering::Equal);
            assert_eq!(a.sort_cmp(&b), b.sort_cmp(&a).reverse());
            if a.sort_cmp(&b).is_le() && b.sort_cmp(&c).is_le() {
                assert!(a.sort_cmp(&c).is_le(), "{a:?} <= {b:?} <= {c:?}");
            }
        }
    }
}
