//! Integer semantics shared by SMALLINT, INTEGER and BIGINT. Every operation runs on `i64`
//! and the result is narrowed back to the target type: `i32 * i32` always fits `i64`, and
//! for BIGINT the `checked_*` operations catch the overflow, so one implementation covers
//! every width.

use super::{DataType, TypeError, Value};

pub(crate) fn narrow(ty: DataType, value: i64) -> Option<Value> {
    match ty {
        DataType::SmallInt => i16::try_from(value).ok().map(Value::SmallInt),
        DataType::Integer => i32::try_from(value).ok().map(Value::Integer),
        DataType::BigInt => Some(Value::BigInt(value)),
        DataType::Real | DataType::Double | DataType::Varchar(_) | DataType::Boolean => None,
    }
}

fn checked(ty: DataType, result: Option<i64>) -> Result<Value, TypeError> {
    result
        .and_then(|value| narrow(ty, value))
        .ok_or(TypeError::OutOfRange { ty })
}

pub(crate) fn add(ty: DataType, lhs: i64, rhs: i64) -> Result<Value, TypeError> {
    checked(ty, lhs.checked_add(rhs))
}

pub(crate) fn sub(ty: DataType, lhs: i64, rhs: i64) -> Result<Value, TypeError> {
    checked(ty, lhs.checked_sub(rhs))
}

pub(crate) fn mul(ty: DataType, lhs: i64, rhs: i64) -> Result<Value, TypeError> {
    checked(ty, lhs.checked_mul(rhs))
}

/// Truncates toward zero (`7 / -2 = -3`); the SQL standard leaves the rounding to us.
pub(crate) fn div(ty: DataType, lhs: i64, rhs: i64) -> Result<Value, TypeError> {
    if rhs == 0 {
        return Err(TypeError::DivisionByZero);
    }
    checked(ty, lhs.checked_div(rhs))
}

/// Sign of the dividend. `MIN % -1` is 0, the exact result, while Rust's `checked_rem`
/// reports it as overflow.
pub(crate) fn rem(ty: DataType, lhs: i64, rhs: i64) -> Result<Value, TypeError> {
    match rhs {
        0 => Err(TypeError::DivisionByZero),
        -1 => checked(ty, Some(0)),
        _ => checked(ty, lhs.checked_rem(rhs)),
    }
}

pub(crate) fn neg(ty: DataType, value: i64) -> Result<Value, TypeError> {
    checked(ty, value.checked_neg())
}

enum ParseFailure {
    Syntax,
    OutOfRange,
}

pub(crate) fn parse(ty: DataType, text: &str) -> Result<Value, TypeError> {
    parse_i64(text)
        .and_then(|value| narrow(ty, value).ok_or(ParseFailure::OutOfRange))
        .map_err(|failure| {
            let input = text.to_string();
            match failure {
                ParseFailure::Syntax => TypeError::InvalidTextRepresentation { ty, input },
                ParseFailure::OutOfRange => TypeError::TextOutOfRange { ty, input },
            }
        })
}

/// C's `isspace` set, allowed around numbers; Rust's ASCII whitespace lacks `\v`.
pub(super) fn is_c_space(c: char) -> bool {
    c.is_ascii_whitespace() || c == '\x0B'
}

/// Accumulates as a negative number so that
/// `-9223372036854775808` fits even though its absolute value does not.
fn parse_i64(text: &str) -> Result<i64, ParseFailure> {
    let text = text.trim_matches(is_c_space);
    let (is_negative, digits) = match text.strip_prefix('-') {
        Some(digits) => (true, digits),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParseFailure::Syntax);
    }

    let mut negated: i64 = 0;
    for digit in digits.bytes().map(|b| i64::from(b - b'0')) {
        negated = negated
            .checked_mul(10)
            .and_then(|n| n.checked_sub(digit))
            .ok_or(ParseFailure::OutOfRange)?;
    }
    if is_negative {
        Ok(negated)
    } else {
        negated.checked_neg().ok_or(ParseFailure::OutOfRange)
    }
}
