//! REAL and DOUBLE PRECISION semantics. ferroDB stores only finite IEEE 754 values and never
//! `-0.0`: there is no NaN or infinity in SQL, and folding `-0.0` into `0.0` leaves every
//! value a single encoding.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use super::integer::{self, is_c_space};
use super::{DataType, TypeError, Value};

macro_rules! finite_float {
    ($name:ident, $float:ty, $sql:literal) => {
        #[doc = concat!("A finite `", stringify!($float), "` that is never `-0.0`: the values a ", $sql, " holds.")]
        #[derive(Debug, Clone, Copy)]
        pub struct $name($float);

        impl $name {
            /// `None` for NaN and the infinities; `-0.0` becomes `0.0`.
            pub fn new(value: $float) -> Option<$name> {
                if !value.is_finite() {
                    return None;
                }
                // `-0.0 == 0.0`, so this also maps `-0.0` to `0.0`.
                Some($name(if value == 0.0 { 0.0 } else { value }))
            }

            pub fn get(self) -> $float {
                self.0
            }
        }

        /// Without NaN and `-0.0`, `total_cmp` is the numeric order and equal values have
        /// equal bits, so these impls are lawful.
        impl Ord for $name {
            fn cmp(&self, other: &$name) -> Ordering {
                self.0.total_cmp(&other.0)
            }
        }

        impl PartialOrd for $name {
            fn partial_cmp(&self, other: &$name) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }

        impl PartialEq for $name {
            fn eq(&self, other: &$name) -> bool {
                self.cmp(other).is_eq()
            }
        }

        impl Eq for $name {}

        impl Hash for $name {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.0.to_bits().hash(state);
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write_shortest(f, &format!("{:e}", self.0), <$float>::DIGITS)
            }
        }
    };
}

finite_float!(Finite32, f32, "REAL");
finite_float!(Finite64, f64, "DOUBLE PRECISION");

/// Text input: optional sign, digits with an optional point and an optional exponent, and
/// surrounding whitespace. The syntax is checked here because Rust's parser, which rounds
/// correctly, would also take `inf` and `NaN`.
pub(crate) fn parse_real(input: &str) -> Result<Value, TypeError> {
    let (text, has_nonzero_digit) = decimal_syntax(DataType::Real, input)?;
    let value: f32 = text
        .parse()
        .map_err(|_| invalid_text(DataType::Real, input))?;
    in_range(
        DataType::Real,
        input,
        value == 0.0 && has_nonzero_digit,
        Finite32::new(value),
    )
    .map(Value::Real)
}

/// Parsed straight to `f64`; REAL parses straight to `f32`, since going through `f64` would
/// round twice.
pub(crate) fn parse_double(input: &str) -> Result<Value, TypeError> {
    let (text, has_nonzero_digit) = decimal_syntax(DataType::Double, input)?;
    let value: f64 = text
        .parse()
        .map_err(|_| invalid_text(DataType::Double, input))?;
    in_range(
        DataType::Double,
        input,
        value == 0.0 && has_nonzero_digit,
        Finite64::new(value),
    )
    .map(Value::Double)
}

/// The trimmed text, and whether its digits (the exponent aside) are not all zeros.
fn decimal_syntax(ty: DataType, input: &str) -> Result<(&str, bool), TypeError> {
    let text = input.trim_matches(is_c_space);
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (unsigned, None),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let is_digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    let is_valid_exponent = exponent.is_none_or(|exponent| {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        !digits.is_empty() && is_digits(digits)
    });
    if whole.is_empty() && fraction.is_empty()
        || !is_digits(whole)
        || !is_digits(fraction)
        || !is_valid_exponent
    {
        return Err(invalid_text(ty, input));
    }
    let has_nonzero_digit = whole
        .bytes()
        .chain(fraction.bytes())
        .any(|byte| byte != b'0');
    Ok((text, has_nonzero_digit))
}

/// Too large for the type (infinite), or a non-zero literal that rounded to zero.
fn in_range<T>(
    ty: DataType,
    input: &str,
    is_underflow: bool,
    finite: Option<T>,
) -> Result<T, TypeError> {
    match finite {
        Some(value) if !is_underflow => Ok(value),
        _ => Err(TypeError::TextOutOfRange {
            ty,
            input: input.to_string(),
        }),
    }
}

fn invalid_text(ty: DataType, input: &str) -> TypeError {
    TypeError::InvalidTextRepresentation {
        ty,
        input: input.to_string(),
    }
}

/// Lays out Rust's shortest round-trip digits (`{:e}`, e.g. `1.5e-7`) the way C's `%g` and
/// PostgreSQL do: positional when the decimal exponent is in `[-4, digits)`, scientific with
/// a signed exponent of at least two digits otherwise. `digits` is 6 for REAL and 15 for
/// DOUBLE PRECISION (`f32::DIGITS`, `f64::DIGITS`).
fn write_shortest(f: &mut fmt::Formatter<'_>, scientific: &str, digits: u32) -> fmt::Result {
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(unsigned) => ("-", unsigned),
        None => ("", mantissa),
    };
    if exponent < -4 || i64::from(exponent) >= i64::from(digits) {
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        return write!(
            f,
            "{sign}{mantissa}e{exponent_sign}{:02}",
            exponent.unsigned_abs()
        );
    }
    let significant: String = mantissa.chars().filter(|&c| c != '.').collect();
    match usize::try_from(exponent) {
        Err(_) => {
            let zeros = "0".repeat(exponent.unsigned_abs() as usize - 1);
            write!(f, "{sign}0.{zeros}{significant}")
        }
        Ok(exponent) => {
            let integer_digits = exponent + 1;
            if significant.len() <= integer_digits {
                let zeros = "0".repeat(integer_digits - significant.len());
                write!(f, "{sign}{significant}{zeros}")
            } else {
                let (whole, fraction) = significant.split_at(integer_digits);
                write!(f, "{sign}{whole}.{fraction}")
            }
        }
    }
}

/// 2^63 is a power of two, so f64 holds it exactly; every i64 lies in `[-2^63, 2^63)`.
const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;

/// `integer` against a finite `float`, exactly, as SQL compares numbers by their value.
/// Converting the integer to f64 would round above 2^53 and call different numbers equal.
pub(crate) fn compare_integer_with_float(integer: i64, float: f64) -> Ordering {
    if float >= TWO_POW_63 {
        return Ordering::Less;
    }
    if float < -TWO_POW_63 {
        return Ordering::Greater;
    }
    let whole = float.trunc();
    // In `[-2^63, 2^63)` and without a fraction, so the cast is exact.
    let whole_integer = whole as i64;
    let fraction = float - whole;
    integer.cmp(&whole_integer).then(if fraction > 0.0 {
        Ordering::Less
    } else if fraction < 0.0 {
        Ordering::Greater
    } else {
        Ordering::Equal
    })
}

/// CAST of an integer: rounds to the nearest float (straight from i64, so REAL rounds once).
/// Every i64 is far inside both ranges, so this never fails in practice.
pub(crate) fn from_integer(ty: DataType, value: i64) -> Result<Value, TypeError> {
    let converted = match ty {
        DataType::Real => Finite32::new(value as f32).map(Value::Real),
        _ => Finite64::new(value as f64).map(Value::Double),
    };
    converted.ok_or(TypeError::OutOfRange { ty })
}

/// CAST between REAL and DOUBLE PRECISION, with the overflow and underflow rules of the
/// arithmetic.
pub(crate) fn convert(ty: DataType, value: f64) -> Result<Value, TypeError> {
    narrow(ty, value, value != 0.0)
}

/// CAST to an integer: rounds half to even (`2.5` -> 2, `3.5` -> 4), as PostgreSQL does for
/// floats, then checks the integer's range.
pub(crate) fn to_integer(ty: DataType, value: f64) -> Result<Value, TypeError> {
    let out_of_range = TypeError::OutOfRange { ty };
    let rounded = value.round_ties_even();
    if !(-TWO_POW_63..TWO_POW_63).contains(&rounded) {
        return Err(out_of_range);
    }
    // Integral and in `[-2^63, 2^63)`, so the cast is exact.
    integer::narrow(ty, rounded as i64).ok_or(out_of_range)
}

/// Result type of an arithmetic operator when a float is involved: REAL only for REAL op
/// REAL, DOUBLE PRECISION otherwise (an integer operand included, as in PostgreSQL).
pub(crate) fn result_type(lhs: DataType, rhs: DataType) -> DataType {
    if lhs == DataType::Real && rhs == DataType::Real {
        DataType::Real
    } else {
        DataType::Double
    }
}

// Operands arrive widened to f64 and results are narrowed to `ty`, like the integers in i64.
// For REAL, rounding the f64 result to f32 equals computing in f32: double rounding is
// harmless for + - * / since 53 >= 2 * 24 + 2.

pub(crate) fn add(ty: DataType, lhs: f64, rhs: f64) -> Result<Value, TypeError> {
    let sum = lhs + rhs;
    narrow(ty, sum, sum != 0.0)
}

pub(crate) fn sub(ty: DataType, lhs: f64, rhs: f64) -> Result<Value, TypeError> {
    narrow(ty, lhs - rhs, lhs != rhs)
}

pub(crate) fn mul(ty: DataType, lhs: f64, rhs: f64) -> Result<Value, TypeError> {
    narrow(ty, lhs * rhs, lhs != 0.0 && rhs != 0.0)
}

/// `x / 0` is an error, not IEEE's infinity.
pub(crate) fn div(ty: DataType, lhs: f64, rhs: f64) -> Result<Value, TypeError> {
    if rhs == 0.0 {
        return Err(TypeError::DivisionByZero);
    }
    narrow(ty, lhs / rhs, lhs != 0.0)
}

pub(crate) fn neg(ty: DataType, value: f64) -> Result<Value, TypeError> {
    narrow(ty, -value, value != 0.0)
}

/// `value` in the type `ty`: infinite is overflow, and zero when the exact result is not
/// (`is_nonzero`) is underflow; both are `22003`, as in PostgreSQL. Subnormals are kept.
fn narrow(ty: DataType, value: f64, is_nonzero: bool) -> Result<Value, TypeError> {
    let out_of_range = TypeError::OutOfRange { ty };
    // `as f32` rounds to the nearest f32, and a value too large becomes infinite, which
    // `Finite32::new` refuses.
    let rounded = if ty == DataType::Real {
        f64::from(value as f32)
    } else {
        value
    };
    if rounded == 0.0 && is_nonzero {
        return Err(out_of_range);
    }
    let narrowed = match ty {
        DataType::Real => Finite32::new(rounded as f32).map(Value::Real),
        _ => Finite64::new(rounded).map(Value::Double),
    };
    narrowed.ok_or(out_of_range)
}

/// A stored field: writes only produce finite values without `-0.0`, so anything else was
/// damaged on disk.
pub(crate) fn decode_real(bytes: [u8; 4]) -> Result<Value, TypeError> {
    let value = f32::from_le_bytes(bytes);
    check_stored(
        DataType::Real,
        is_negative_zero(value.into()),
        Finite32::new(value),
    )
    .map(Value::Real)
}

pub(crate) fn decode_double(bytes: [u8; 8]) -> Result<Value, TypeError> {
    let value = f64::from_le_bytes(bytes);
    check_stored(
        DataType::Double,
        is_negative_zero(value),
        Finite64::new(value),
    )
    .map(Value::Double)
}

fn is_negative_zero(value: f64) -> bool {
    value == 0.0 && value.is_sign_negative()
}

fn check_stored<T>(
    ty: DataType,
    is_negative_zero: bool,
    finite: Option<T>,
) -> Result<T, TypeError> {
    let corrupted = |reason| TypeError::CorruptedValue { ty, reason };
    if is_negative_zero {
        return Err(corrupted("negative zero is never stored"));
    }
    finite.ok_or_else(|| corrupted("float must be finite"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_should_keep_finite_values() {
        let doubles = [
            f64::MIN,
            f64::MAX,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            0.0,
            -1.5,
        ];
        let reals = [
            f32::MIN,
            f32::MAX,
            f32::MIN_POSITIVE,
            f32::from_bits(1),
            0.0,
            -1.5,
        ];

        for value in doubles {
            assert_eq!(
                Finite64::new(value).unwrap().get().to_bits(),
                value.to_bits()
            );
        }
        for value in reals {
            assert_eq!(
                Finite32::new(value).unwrap().get().to_bits(),
                value.to_bits()
            );
        }
    }

    #[test]
    fn new_should_reject_nan_and_infinities() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(Finite64::new(value), None, "{value}");
        }
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(Finite32::new(value), None, "{value}");
        }
    }

    #[test]
    fn new_should_fold_negative_zero_into_zero() {
        assert_eq!(Finite64::new(-0.0).unwrap().get().to_bits(), 0);
        assert_eq!(Finite32::new(-0.0).unwrap().get().to_bits(), 0);
        assert_eq!(Finite64::new(-0.0), Finite64::new(0.0));
    }

    #[test]
    fn order_should_be_numeric() {
        let mut values: Vec<Finite64> = [3.5, -0.0, f64::MIN, 1e-300, -2.0, f64::MAX]
            .into_iter()
            .map(|value| Finite64::new(value).unwrap())
            .collect();

        values.sort();

        let sorted: Vec<f64> = values.into_iter().map(Finite64::get).collect();
        assert_eq!(sorted, [f64::MIN, -2.0, 0.0, 1e-300, 3.5, f64::MAX]);
    }
}
