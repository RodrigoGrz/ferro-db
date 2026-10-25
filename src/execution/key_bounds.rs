//! Bounds of an index scan in the type of the index's key.
//!
//! A bound is converted with store assignment (integers of another width, an integer or a
//! DOUBLE PRECISION for a float key, a DATE for a TIMESTAMP key); text is used as it is, since
//! a key may be any text and a bound longer than the column only compares. Every bound
//! admits exactly the keys the comparison it stands for admits:
//!
//! - A float key type may round the bound (`0.1` as a REAL is 0.10000000149…). No key lies
//!   strictly between the bound and its rounding, so a bound rounded up admits the keys from
//!   it on the lower side and below it on the upper side, inclusive or not; rounded down, the
//!   other way round.
//! - A number the key type cannot hold narrows the range instead of failing: no key is above
//!   `70000` in a SMALLINT index, so a lower bound of `70000` empties the range and an upper
//!   bound of `70000` is no bound at all. A DOUBLE PRECISION too close to zero for a REAL is
//!   zero, rounded as above.
//! - A `NULL` bound matches nothing (`key < NULL` is UNKNOWN).
//!
//! Any other conversion is the assignment's error (`42804`): a float for an integer key, a
//! TIMESTAMP for a DATE key.

use std::cmp::Ordering;
use std::ops::Bound;

use crate::types::{DataType, TypeError, Value};

use super::ExecutionError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Lower,
    Upper,
}

/// The bound in the key's type, or `None` when it alone makes the range empty.
pub(crate) fn key_bound(
    bound: Bound<Value>,
    key_type: DataType,
    side: Side,
) -> Result<Option<Bound<Value>>, ExecutionError> {
    let (value, is_inclusive) = match bound {
        Bound::Unbounded => return Ok(Some(Bound::Unbounded)),
        Bound::Included(value) => (value, true),
        Bound::Excluded(value) => (value, false),
    };
    Ok(match in_key_type(&value, key_type, side)? {
        Converted::Key(key) => {
            // Only a float key type rounds; any other conversion keeps the value.
            let rounding = match key_type.is_float() {
                true => key.sort_cmp(&value),
                false => Ordering::Equal,
            };
            Some(match (rounding, side) {
                (Ordering::Equal, _) if is_inclusive => Bound::Included(key),
                (Ordering::Equal, _) => Bound::Excluded(key),
                (Ordering::Greater, Side::Lower) | (Ordering::Less, Side::Upper) => {
                    Bound::Included(key)
                }
                (Ordering::Greater, Side::Upper) | (Ordering::Less, Side::Lower) => {
                    Bound::Excluded(key)
                }
            })
        }
        Converted::NoBound => Some(Bound::Unbounded),
        Converted::EmptyRange => None,
    })
}

/// The key equal to `value`, or `None` when no key can be: `NULL`, a value the key type
/// rounds, or one it cannot hold.
///
/// It is the range `key >= value AND key <= value` converted as an index scan would: only
/// when both sides keep the value inclusive does a key equal it.
pub(crate) fn point_key(value: Value, key_type: DataType) -> Result<Option<Value>, ExecutionError> {
    let lower = key_bound(Bound::Included(value.clone()), key_type, Side::Lower)?;
    let upper = key_bound(Bound::Included(value), key_type, Side::Upper)?;
    Ok(match (lower, upper) {
        (Some(Bound::Included(lower)), Some(Bound::Included(upper))) if lower == upper => {
            Some(lower)
        }
        _ => None,
    })
}

/// Whether every value of `value_type` converts to a bound of a `key_type` key without an
/// error: the same family, but neither a float for an integer key nor a TIMESTAMP for a DATE
/// key, which assignment refuses (42804) where `=` compares them.
pub(crate) fn converts_to_key(value_type: DataType, key_type: DataType) -> bool {
    value_type.family() == key_type.family()
        && !(value_type.is_float() && key_type.is_integer())
        && !(value_type == DataType::Timestamp && key_type == DataType::Date)
}

enum Converted {
    Key(Value),
    /// Every key is on the right side of the bound.
    NoBound,
    /// No key can be on the right side of the bound.
    EmptyRange,
}

fn in_key_type(value: &Value, key_type: DataType, side: Side) -> Result<Converted, ExecutionError> {
    if value.is_null() {
        return Ok(Converted::EmptyRange);
    }
    if matches!((value, key_type), (Value::Text(_), DataType::Varchar(_))) {
        return Ok(Converted::Key(value.clone()));
    }
    match value.assign(key_type) {
        Ok(key) => Ok(Converted::Key(key)),
        Err(TypeError::OutOfRange { .. }) if key_type.is_integer() || key_type.is_float() => {
            let is_near_zero = value.sort_cmp(&Value::Integer(-1)).is_gt()
                && value.sort_cmp(&Value::Integer(1)).is_lt();
            if is_near_zero {
                return Ok(Converted::Key(Value::Integer(0).assign(key_type)?));
            }
            let is_above_every_key = value.sort_cmp(&Value::BigInt(0)).is_gt();
            Ok(match (side, is_above_every_key) {
                (Side::Lower, true) | (Side::Upper, false) => Converted::EmptyRange,
                (Side::Lower, false) | (Side::Upper, true) => Converted::NoBound,
            })
        }
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Date, Finite32, Finite64, Timestamp};
    use Bound::{Excluded, Included, Unbounded};
    use Side::{Lower, Upper};
    use std::num::NonZeroU16;

    fn converted(bound: Bound<Value>, key_type: DataType, side: Side) -> Option<Bound<Value>> {
        key_bound(bound, key_type, side).unwrap()
    }

    #[test]
    fn converts_to_key_should_agree_with_the_conversion_of_a_bound() {
        let day = Date::from_ymd(2024, 3, 15).unwrap();
        let values = [
            Value::SmallInt(1),
            Value::Integer(1),
            Value::BigInt(i64::MAX),
            Value::Real(Finite32::new(0.5).unwrap()),
            Value::Double(Finite64::new(1e300).unwrap()),
            Value::Text("abcd".to_string()),
            Value::Boolean(true),
            Value::Date(day),
            Value::Timestamp(Timestamp::at_midnight(day)),
        ];
        let key_types = [
            DataType::SmallInt,
            DataType::Integer,
            DataType::BigInt,
            DataType::Real,
            DataType::Double,
            DataType::Varchar(NonZeroU16::new(3).unwrap()),
            DataType::Boolean,
            DataType::Date,
            DataType::Timestamp,
        ];

        for value in &values {
            for key_type in key_types {
                let value_type = value.data_type().unwrap();
                let converts = point_key(value.clone(), key_type).is_ok();

                assert_eq!(
                    converts_to_key(value_type, key_type),
                    converts,
                    "{value_type} into a {key_type} key"
                );
            }
        }
    }

    #[test]
    fn point_key_should_assign_a_value_of_the_same_family() {
        let day = Date::from_ymd(2024, 3, 15).unwrap();
        let half = Value::Double(Finite64::new(0.5).unwrap());

        let integer = point_key(Value::SmallInt(5), DataType::Integer).unwrap();
        let timestamp = point_key(Value::Date(day), DataType::Timestamp).unwrap();
        let real = point_key(half, DataType::Real).unwrap();
        let varchar = DataType::Varchar(NonZeroU16::new(3).unwrap());
        let text = point_key(Value::Text("abcd".to_string()), varchar).unwrap();

        assert_eq!(integer, Some(Value::Integer(5)));
        assert_eq!(
            timestamp,
            Some(Value::Timestamp(Timestamp::at_midnight(day)))
        );
        assert_eq!(real, Some(Value::Real(Finite32::new(0.5).unwrap())));
        assert_eq!(text, Some(Value::Text("abcd".to_string())));
    }

    #[test]
    fn point_key_should_be_none_when_no_key_can_equal_the_value() {
        let cases = [
            (Value::Null, DataType::Integer),
            (Value::Double(Finite64::new(0.1).unwrap()), DataType::Real),
            (Value::Double(Finite64::new(1e-50).unwrap()), DataType::Real),
            (Value::Integer(70_000), DataType::SmallInt),
            (Value::BigInt(i64::MIN), DataType::Integer),
        ];

        for (value, key_type) in cases {
            assert_eq!(
                point_key(value.clone(), key_type).unwrap(),
                None,
                "{value:?}"
            );
        }
    }

    #[test]
    fn point_key_of_another_family_should_fail() {
        let cases = [
            (Value::Text("1".to_string()), DataType::Integer),
            (
                Value::Double(Finite64::new(1.0).unwrap()),
                DataType::Integer,
            ),
        ];

        for (value, key_type) in cases {
            let err = point_key(value, key_type).unwrap_err();

            assert_eq!(err.sqlstate(), "42804");
        }
    }

    #[test]
    fn an_open_bound_should_stay_open() {
        assert_eq!(
            converted(Unbounded, DataType::Integer, Lower),
            Some(Unbounded)
        );
        assert_eq!(
            converted(Unbounded, DataType::Integer, Upper),
            Some(Unbounded)
        );
    }

    #[test]
    fn a_null_bound_should_empty_the_range() {
        assert_eq!(
            converted(Included(Value::Null), DataType::Integer, Lower),
            None
        );
        assert_eq!(
            converted(Excluded(Value::Null), DataType::Integer, Upper),
            None
        );
    }

    #[test]
    fn a_bound_of_the_same_family_should_be_assigned_to_the_key_type() {
        let date = Date::from_ymd(2024, 3, 15).unwrap();

        assert_eq!(
            converted(Included(Value::SmallInt(7)), DataType::Integer, Lower),
            Some(Included(Value::Integer(7)))
        );
        assert_eq!(
            converted(Excluded(Value::Integer(2)), DataType::Double, Upper),
            Some(Excluded(Value::Double(Finite64::new(2.0).unwrap())))
        );
        assert_eq!(
            converted(Included(Value::Date(date)), DataType::Timestamp, Lower),
            Some(Included(Value::Timestamp(Timestamp::at_midnight(date))))
        );
    }

    #[test]
    fn text_should_be_used_as_it_is_even_past_the_varchar_length() {
        let varchar = DataType::Varchar(NonZeroU16::new(3).unwrap());
        let long = Value::Text("longer than three  ".to_string());

        assert_eq!(
            converted(Included(long.clone()), varchar, Lower),
            Some(Included(long))
        );
    }

    #[test]
    fn an_integer_outside_the_key_type_should_narrow_the_range() {
        let above = Value::Integer(70_000);
        let below = Value::Integer(-70_000);

        assert_eq!(
            converted(Included(above.clone()), DataType::SmallInt, Lower),
            None
        );
        assert_eq!(
            converted(Excluded(above), DataType::SmallInt, Upper),
            Some(Unbounded)
        );
        assert_eq!(
            converted(Excluded(below.clone()), DataType::SmallInt, Lower),
            Some(Unbounded)
        );
        assert_eq!(converted(Included(below), DataType::SmallInt, Upper), None);
        assert_eq!(
            converted(Included(Value::BigInt(i64::MAX)), DataType::Integer, Upper),
            Some(Unbounded)
        );
    }

    fn real(value: f32) -> Value {
        Value::Real(Finite32::new(value).unwrap())
    }

    fn double(value: f64) -> Value {
        Value::Double(Finite64::new(value).unwrap())
    }

    #[test]
    fn a_float_bound_should_round_toward_the_keys_it_admits() {
        // 0.1 as REAL is above 0.1 as DOUBLE PRECISION; 16777217 as REAL is 16777216, below it.
        let above = real(0.1);
        let below = real(16_777_216.0);
        let past_24_bits = Value::Integer(16_777_217);
        let cases = [
            (Included(double(0.1)), Lower, Included(above.clone())),
            (Excluded(double(0.1)), Lower, Included(above.clone())),
            (Included(double(0.1)), Upper, Excluded(above.clone())),
            (Excluded(double(0.1)), Upper, Excluded(above)),
            (
                Included(past_24_bits.clone()),
                Lower,
                Excluded(below.clone()),
            ),
            (
                Excluded(past_24_bits.clone()),
                Lower,
                Excluded(below.clone()),
            ),
            (
                Included(past_24_bits.clone()),
                Upper,
                Included(below.clone()),
            ),
            (Excluded(past_24_bits), Upper, Included(below)),
        ];

        for (bound, side, expected) in cases {
            let description = format!("{bound:?} {side:?}");

            assert_eq!(
                converted(bound, DataType::Real, side),
                Some(expected),
                "{description}"
            );
        }
        // 2^53 + 1 as DOUBLE PRECISION is 2^53.
        assert_eq!(
            converted(
                Excluded(Value::BigInt((1 << 53) + 1)),
                DataType::Double,
                Upper
            ),
            Some(Included(double(9_007_199_254_740_992.0)))
        );
    }

    #[test]
    fn a_float_bound_beyond_the_key_type_should_narrow_the_range() {
        let zero = real(0.0);
        let cases = [
            (Included(double(1e39)), Lower, None),
            (Excluded(double(1e39)), Upper, Some(Unbounded)),
            (Excluded(double(-1e39)), Lower, Some(Unbounded)),
            (Included(double(-1e39)), Upper, None),
            // Too close to zero for a REAL: zero, rounded the right way.
            (Included(double(1e-50)), Lower, Some(Excluded(zero.clone()))),
            (Included(double(1e-50)), Upper, Some(Included(zero.clone()))),
            (
                Included(double(-1e-50)),
                Lower,
                Some(Included(zero.clone())),
            ),
            (Excluded(double(-1e-50)), Upper, Some(Excluded(zero))),
        ];

        for (bound, side, expected) in cases {
            let description = format!("{bound:?} {side:?}");

            assert_eq!(
                converted(bound, DataType::Real, side),
                expected,
                "{description}"
            );
        }
    }

    #[test]
    fn a_bound_the_key_type_cannot_take_should_fail() {
        let real = Value::Double(Finite64::new(2.5).unwrap());
        let midnight = Value::Timestamp(Timestamp::at_midnight(Date::MIN));
        let cases = [
            (real, DataType::Integer),
            (midnight, DataType::Date),
            (Value::Text("1".to_string()), DataType::Integer),
            (
                Value::Integer(1),
                DataType::Varchar(NonZeroU16::new(3).unwrap()),
            ),
            (Value::Boolean(true), DataType::Integer),
        ];

        for (value, key_type) in cases {
            let err = key_bound(Included(value), key_type, Lower).unwrap_err();

            assert_eq!(err.sqlstate(), "42804", "{err:?}");
        }
    }
}
