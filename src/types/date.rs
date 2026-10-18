//! DATE semantics: a day of the proleptic Gregorian calendar (today's leap rules applied to
//! every year) between 0001-01-01 and 9999-12-31, the range of the SQL standard, held as the
//! number of days since 1970-01-01.
//!
//! Days and calendar dates convert in O(1) with Howard Hinnant's `days_from_civil` and
//! `civil_from_days` (http://howardhinnant.github.io/date_algorithms.html): years are counted
//! from March, so February and its leap day close the year, and the calendar repeats every
//! 400-year era of 146,097 days.

use std::fmt;

use super::integer::is_c_space;
use super::{DataType, TypeError, Value};

/// A day between 0001-01-01 and 9999-12-31, as days since 1970-01-01.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(i32);

impl Date {
    pub const MIN: Date = Date(-719_162);
    pub const MAX: Date = Date(2_932_896);

    /// `None` for a day that does not exist (`2023-02-29`) or a year outside 1..=9999.
    pub fn from_ymd(year: i32, month: u32, day: u32) -> Option<Date> {
        let month = i32::try_from(month).ok()?;
        let day = i32::try_from(day).ok()?;
        let is_valid = (1..=9999).contains(&year)
            && (1..=12).contains(&month)
            && (1..=days_in_month(year, month)).contains(&day);
        is_valid.then(|| Date(days_from_civil(year, month, day)))
    }

    pub fn from_days_since_epoch(days: i32) -> Option<Date> {
        (Date::MIN.0..=Date::MAX.0)
            .contains(&days)
            .then_some(Date(days))
    }

    pub fn days_since_epoch(self) -> i32 {
        self.0
    }

    pub fn year_month_day(self) -> (i32, u32, u32) {
        let (year, month, day) = civil_from_days(self.0);
        (year, month.unsigned_abs(), day.unsigned_abs())
    }
}

/// Text input: surrounding whitespace, then exactly `YYYY-MM-DD`. A well-formed day that does
/// not exist or is outside 0001..=9999 is out of range (22008); any other text is invalid
/// syntax (22007).
pub(crate) fn parse(input: &str) -> Result<Value, TypeError> {
    let text = input.trim_matches(is_c_space);
    let fields = match *text.as_bytes() {
        [y0, y1, y2, y3, b'-', m0, m1, b'-', d0, d1] => decimal(&[y0, y1, y2, y3])
            .zip(decimal(&[m0, m1]))
            .zip(decimal(&[d0, d1])),
        _ => None,
    };
    let Some(((year, month), day)) = fields else {
        return Err(TypeError::InvalidTextRepresentation {
            ty: DataType::Date,
            input: input.to_string(),
        });
    };
    i32::try_from(year)
        .ok()
        .and_then(|year| Date::from_ymd(year, month, day))
        .map(Value::Date)
        .ok_or_else(|| TypeError::TextOutOfRange {
            ty: DataType::Date,
            input: input.to_string(),
        })
}

/// The number written by a few ASCII digits; `None` if any byte is not a digit.
fn decimal(digits: &[u8]) -> Option<u32> {
    digits.iter().try_fold(0, |number, &byte| {
        byte.is_ascii_digit()
            .then(|| number * 10 + u32::from(byte - b'0'))
    })
}

/// `DATE + n` (and `DATE - n` as `-n`) in days, the shorthand PostgreSQL keeps until INTERVAL
/// exists. Computed in i64, so a huge `n` is out of range (22008), never a wrap.
pub(crate) fn add_days(days: i32, offset: i64) -> Result<Value, TypeError> {
    i64::from(days)
        .checked_add(offset)
        .and_then(|total| i32::try_from(total).ok())
        .and_then(Date::from_days_since_epoch)
        .map(Value::Date)
        .ok_or(TypeError::OutOfRange { ty: DataType::Date })
}

pub(crate) fn subtract_days(days: i32, offset: i64) -> Result<Value, TypeError> {
    offset
        .checked_neg()
        .map_or(Err(TypeError::OutOfRange { ty: DataType::Date }), |back| {
            add_days(days, back)
        })
}

/// `DATE - DATE` in days. Both are within 0001..=9999, so the difference fits an INTEGER.
pub(crate) fn days_between(lhs: i32, rhs: i32) -> Value {
    Value::Integer(lhs - rhs)
}

/// A stored field: writes only produce days in range, so anything else was damaged on disk.
pub(crate) fn decode(bytes: [u8; 4]) -> Result<Value, TypeError> {
    Date::from_days_since_epoch(i32::from_le_bytes(bytes))
        .map(Value::Date)
        .ok_or(TypeError::CorruptedValue {
            ty: DataType::Date,
            reason: "date out of range",
        })
}

fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i32, month: i32) -> i32 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 0000-03-01 to 1970-01-01.
const EPOCH_FROM_MARCH_0000: i32 = 719_468;
const DAYS_PER_ERA: i32 = 146_097;

/// Days since 1970-01-01 of a valid date.
fn days_from_civil(year: i32, month: i32, day: i32) -> i32 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400; // [0, 399]
    let month_from_march = (month + 9) % 12; // March = 0 ... February = 11
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1; // [0, 365]
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * DAYS_PER_ERA + day_of_era - EPOCH_FROM_MARCH_0000
}

/// The (year, month, day) of a number of days since 1970-01-01; month and day are positive.
fn civil_from_days(days: i32) -> (i32, i32, i32) {
    let days = days + EPOCH_FROM_MARCH_0000;
    let era = days.div_euclid(DAYS_PER_ERA);
    let day_of_era = days - era * DAYS_PER_ERA; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153; // [0, 11]
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1; // [1, 31]
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + i32::from(month <= 2);
    (year, month, day)
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = self.year_month_day();
        write!(f, "{year:04}-{month:02}-{day:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> Date {
        Date::from_ymd(year, month, day).unwrap()
    }

    #[test]
    fn days_should_count_from_1970_01_01() {
        assert_eq!(date(1970, 1, 1).days_since_epoch(), 0);
        assert_eq!(date(1969, 12, 31).days_since_epoch(), -1);
        assert_eq!(date(2000, 3, 1).days_since_epoch(), 11_017);
        assert_eq!(date(2024, 2, 29).days_since_epoch(), 19_782);
    }

    #[test]
    fn the_range_should_be_years_0001_to_9999() {
        assert_eq!(date(1, 1, 1), Date::MIN);
        assert_eq!(date(9999, 12, 31), Date::MAX);
        assert_eq!(Date::MIN.days_since_epoch(), -719_162);
        assert_eq!(Date::MAX.days_since_epoch(), 2_932_896);
    }

    #[test]
    fn february_29_should_exist_only_in_leap_years() {
        for year in [4, 1600, 2000, 2024, 9996] {
            assert!(Date::from_ymd(year, 2, 29).is_some(), "{year}");
        }
        for year in [1, 1900, 2023, 2100, 9999] {
            assert_eq!(Date::from_ymd(year, 2, 29), None, "{year}");
        }
    }

    #[test]
    fn from_ymd_should_respect_month_lengths() {
        let last_days = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

        for (month, last_day) in (1..=12).zip(last_days) {
            assert!(Date::from_ymd(2023, month, last_day).is_some(), "{month}");
            assert_eq!(Date::from_ymd(2023, month, last_day + 1), None, "{month}");
        }
    }

    #[test]
    fn from_ymd_should_reject_fields_out_of_range() {
        let cases = [
            (0, 1, 1),
            (10_000, 1, 1),
            (-5, 1, 1),
            (2024, 0, 1),
            (2024, 13, 1),
            (2024, 1, 0),
            (2024, 1, 32),
        ];

        for (year, month, day) in cases {
            assert_eq!(
                Date::from_ymd(year, month, day),
                None,
                "{year}-{month}-{day}"
            );
        }
    }

    #[test]
    fn from_ymd_should_reject_months_and_days_beyond_i32() {
        assert_eq!(Date::from_ymd(2024, u32::MAX, 1), None);
        assert_eq!(Date::from_ymd(2024, 1, u32::MAX), None);
    }

    #[test]
    fn from_days_since_epoch_should_accept_only_the_range() {
        assert_eq!(Date::from_days_since_epoch(-719_162), Some(Date::MIN));
        assert_eq!(Date::from_days_since_epoch(2_932_896), Some(Date::MAX));
        for days in [-719_163, 2_932_897, i32::MIN, i32::MAX] {
            assert_eq!(Date::from_days_since_epoch(days), None, "{days}");
        }
    }

    #[test]
    fn year_month_day_should_invert_from_ymd() {
        let cases = [
            (1, 1, 1),
            (1969, 12, 31),
            (1970, 1, 1),
            (2000, 2, 29),
            (2024, 12, 31),
            (9999, 12, 31),
        ];

        for (year, month, day) in cases {
            assert_eq!(date(year, month, day).year_month_day(), (year, month, day));
        }
    }

    #[test]
    fn consecutive_days_should_cross_month_and_year_boundaries() {
        let start = date(2024, 2, 28).days_since_epoch();
        let next = |offset| {
            Date::from_days_since_epoch(start + offset)
                .unwrap()
                .year_month_day()
        };

        assert_eq!(next(1), (2024, 2, 29));
        assert_eq!(next(2), (2024, 3, 1));
        assert_eq!(
            Date::from_days_since_epoch(date(1999, 12, 31).days_since_epoch() + 1)
                .unwrap()
                .year_month_day(),
            (2000, 1, 1)
        );
    }

    /// The calendar walked year by year and month by month from 0001-01-01: slow, obviously
    /// right, and independent of Hinnant's closed formulas.
    fn naive_civil_from_days(days_since_epoch: i32) -> (i32, u32, u32) {
        let mut remaining = days_since_epoch + 719_162; // days since 0001-01-01
        let mut year = 1;
        loop {
            let year_length = if is_leap_year(year) { 366 } else { 365 };
            if remaining < year_length {
                break;
            }
            remaining -= year_length;
            year += 1;
        }
        let mut month = 1;
        while remaining >= days_in_month(year, month) {
            remaining -= days_in_month(year, month);
            month += 1;
        }
        (year, month.unsigned_abs(), (remaining + 1).unsigned_abs())
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn random_days_should_match_a_naive_calendar() {
        let mut rng = Rng(0x6A09_E667_F3BC_C908);
        let span = u64::from((Date::MAX.0 - Date::MIN.0).unsigned_abs()) + 1;

        for _ in 0..2_000 {
            let days = Date::MIN.0 + i32::try_from(rng.next() % span).unwrap();
            let day = Date::from_days_since_epoch(days).unwrap();
            let (year, month, day_of_month) = naive_civil_from_days(days);

            assert_eq!(day.year_month_day(), (year, month, day_of_month), "{days}");
            assert_eq!(Date::from_ymd(year, month, day_of_month), Some(day));
            assert_eq!(
                day.to_string(),
                format!("{year:04}-{month:02}-{day_of_month:02}")
            );
        }
    }

    /// The calendar day after `(year, month, day)`, by the month lengths alone.
    fn next_civil_day((year, month, day): (i32, u32, u32)) -> (i32, u32, u32) {
        let month_length = days_in_month(year, i32::try_from(month).unwrap()).unsigned_abs();
        match (month, day) {
            (12, 31) => (year + 1, 1, 1),
            (_, day) if day == month_length => (year, month + 1, 1),
            _ => (year, month, day + 1),
        }
    }

    /// Every one of the 3,652,059 days, both directions. Random samples miss the few days per
    /// 400 years where a wrong century term in the formulas shows.
    #[test]
    fn every_day_of_the_range_should_follow_the_previous_one() {
        let mut expected = (1, 1, 1);

        for days in Date::MIN.0..=Date::MAX.0 {
            let day = Date(days);

            assert_eq!(day.year_month_day(), expected, "{days}");
            assert_eq!(
                Date::from_ymd(expected.0, expected.1, expected.2),
                Some(day)
            );
            expected = next_civil_day(expected);
        }
        assert_eq!(expected, (10_000, 1, 1));
    }

    #[test]
    fn display_should_be_iso_with_a_four_digit_year() {
        assert_eq!(date(1, 1, 1).to_string(), "0001-01-01");
        assert_eq!(date(1970, 1, 1).to_string(), "1970-01-01");
        assert_eq!(date(2024, 2, 29).to_string(), "2024-02-29");
        assert_eq!(date(9999, 12, 31).to_string(), "9999-12-31");
    }
}
