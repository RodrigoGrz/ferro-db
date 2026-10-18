//! BOOLEAN semantics. A stored field is one byte, 0 for FALSE and 1 for TRUE; any other
//! byte is corruption, so one value never has two encodings.
//!
//! Logic is Kleene's three-valued logic with `None` for UNKNOWN: a side that alone decides
//! the result wins over UNKNOWN (`FALSE AND UNKNOWN = FALSE`, `TRUE OR UNKNOWN = TRUE`).

use super::integer::is_c_space;
use super::{DataType, TypeError, Value};

pub(crate) fn encode(value: bool) -> u8 {
    u8::from(value)
}

pub(crate) fn decode(byte: u8) -> Result<Value, TypeError> {
    match byte {
        0 => Ok(Value::Boolean(false)),
        1 => Ok(Value::Boolean(true)),
        _ => Err(TypeError::CorruptedValue {
            ty: DataType::Boolean,
            reason: "boolean byte must be 0 or 1",
        }),
    }
}

/// The SQL literal: `Display` and CAST to text.
pub(crate) fn literal(value: bool) -> &'static str {
    if value { "TRUE" } else { "FALSE" }
}

/// CAST from an integer: only 0 and 1, so the conversion loses nothing either way.
pub(crate) fn from_integer(value: i64) -> Result<Value, TypeError> {
    match value {
        0 => Ok(Value::Boolean(false)),
        1 => Ok(Value::Boolean(true)),
        _ => Err(TypeError::OutOfRange {
            ty: DataType::Boolean,
        }),
    }
}

/// Text input and CAST from text: `true`/`false` in any case, or `1`/`0`, with surrounding
/// whitespace. Nothing else, not even PostgreSQL's `t`, `yes` or `on`.
pub(crate) fn parse(input: &str) -> Result<Value, TypeError> {
    let trimmed = input.trim_matches(is_c_space);
    if trimmed.eq_ignore_ascii_case("true") || trimmed == "1" {
        Ok(Value::Boolean(true))
    } else if trimmed.eq_ignore_ascii_case("false") || trimmed == "0" {
        Ok(Value::Boolean(false))
    } else {
        Err(TypeError::InvalidTextRepresentation {
            ty: DataType::Boolean,
            input: input.to_string(),
        })
    }
}

pub(crate) fn and(lhs: Option<bool>, rhs: Option<bool>) -> Option<bool> {
    match (lhs, rhs) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

pub(crate) fn or(lhs: Option<bool>, rhs: Option<bool>) -> Option<bool> {
    match (lhs, rhs) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}
