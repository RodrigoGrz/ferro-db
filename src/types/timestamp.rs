//! TIMESTAMP semantics: a date and time of day between 0001-01-01 00:00:00 and
//! 9999-12-31 23:59:59.999999, held as microseconds since 1970-01-01 00:00:00 (the epoch of
//! `Date`, so a day is exactly `MICROS_PER_DAY`). The day and the time of day are split with
//! floor division, so an instant before 1970 still has a time of day between 00:00 and 24:00.

use std::fmt;

use super::date::{self, DatetimeTextError};
use super::integer::is_c_space;
use super::{DataType, Date, TypeError, Value};

const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;

/// A date and time of day, as microseconds since 1970-01-01 00:00:00.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    pub const MIN: Timestamp = Timestamp(Date::MIN.days_since_epoch() as i64 * MICROS_PER_DAY);
    pub const MAX: Timestamp =
        Timestamp((Date::MAX.days_since_epoch() as i64 + 1) * MICROS_PER_DAY - 1);

    /// `None` for an hour past 23, a minute or second past 59, or a microsecond past 999,999.
    pub fn new(date: Date, hour: u32, minute: u32, second: u32, micro: u32) -> Option<Timestamp> {
        let is_valid = hour < 24 && minute < 60 && second < 60 && micro < 1_000_000;
        // Computed only once the fields are valid: a huge hour would overflow the i64.
        is_valid.then(|| {
            let seconds = (i64::from(hour) * 60 + i64::from(minute)) * 60 + i64::from(second);
            let time_of_day = seconds * MICROS_PER_SECOND + i64::from(micro);
            Timestamp(Timestamp::at_midnight(date).0 + time_of_day)
        })
    }

    pub fn at_midnight(date: Date) -> Timestamp {
        Timestamp(midnight_micros(date.days_since_epoch()))
    }

    pub fn from_micros_since_epoch(micros: i64) -> Option<Timestamp> {
        (Timestamp::MIN.0..=Timestamp::MAX.0)
            .contains(&micros)
            .then_some(Timestamp(micros))
    }

    pub fn micros_since_epoch(self) -> i64 {
        self.0
    }

    /// Floor division: -1 µs is on 1969-12-31, not on 1970-01-01.
    pub fn date(self) -> Date {
        day_of(self.0)
    }

    /// Hour, minute, second and microsecond.
    pub fn time_of_day(self) -> (u32, u32, u32, u32) {
        let micros = self.0.rem_euclid(MICROS_PER_DAY); // [0, MICROS_PER_DAY)
        let seconds = micros / MICROS_PER_SECOND;
        // Every field is below 1,000,000, so the casts are exact.
        (
            (seconds / 3_600) as u32,
            (seconds / 60 % 60) as u32,
            (seconds % 60) as u32,
            (micros % MICROS_PER_SECOND) as u32,
        )
    }
}

/// The day of an instant in range (`MIN..=MAX`), by floor division.
pub(crate) fn day_of(micros: i64) -> Date {
    // Within `MIN..=MAX`, so the day is one of `Date`'s and fits an i32.
    Date::from_valid_days(micros.div_euclid(MICROS_PER_DAY) as i32)
}

/// The instant of midnight on a day, in microseconds: how a DATE compares with a TIMESTAMP.
/// Every day of `Date`'s range times `MICROS_PER_DAY` fits an i64.
pub(crate) fn midnight_micros(days_since_epoch: i32) -> i64 {
    i64::from(days_since_epoch) * MICROS_PER_DAY
}

/// Text input: surrounding whitespace, then `YYYY-MM-DD`, optionally followed by a space or
/// `T` and `HH:MM:SS` with an optional fraction of 1 to 6 digits; a date alone is midnight.
/// The whole shape is checked before any range, so malformed text is always 22007.
pub(crate) fn parse(input: &str) -> Result<Value, TypeError> {
    parse_text(input.trim_matches(is_c_space))
        .map(Value::Timestamp)
        .map_err(|error| error.for_type(DataType::Timestamp, input))
}

fn parse_text(text: &str) -> Result<Timestamp, DatetimeTextError> {
    let (date_text, time_text) = match text.split_at_checked(10) {
        Some((date_text, "")) => (date_text, None),
        Some((date_text, rest)) => match rest.strip_prefix([' ', 'T']) {
            Some(time_text) => (date_text, Some(time_text)),
            None => return Err(DatetimeTextError::Syntax),
        },
        None => return Err(DatetimeTextError::Syntax),
    };
    let time = match time_text {
        Some(time_text) => parse_time_fields(time_text),
        None => Ok((0, 0, 0, 0)),
    };
    match (date::parse_ymd(date_text), time) {
        (Ok(date), Ok((hour, minute, second, micro))) => {
            Timestamp::new(date, hour, minute, second, micro).ok_or(DatetimeTextError::OutOfRange)
        }
        (Err(DatetimeTextError::Syntax), _) | (_, Err(DatetimeTextError::Syntax)) => {
            Err(DatetimeTextError::Syntax)
        }
        _ => Err(DatetimeTextError::OutOfRange),
    }
}

/// `HH:MM:SS` and an optional `.f` to `.ffffff`, by shape only; `Timestamp::new` checks the
/// ranges.
fn parse_time_fields(text: &str) -> Result<(u32, u32, u32, u32), DatetimeTextError> {
    let syntax = DatetimeTextError::Syntax;
    let [h0, h1, b':', m0, m1, b':', s0, s1, ref fraction @ ..] = *text.as_bytes() else {
        return Err(syntax);
    };
    let micro = match fraction {
        [] => 0,
        [b'.', digits @ ..] if (1..=6).contains(&digits.len()) => {
            let scale = 10_u32.pow(6 - digits.len() as u32);
            date::decimal(digits).ok_or(syntax)? * scale
        }
        _ => return Err(syntax),
    };
    let hour = date::decimal(&[h0, h1]).ok_or(syntax)?;
    let minute = date::decimal(&[m0, m1]).ok_or(syntax)?;
    let second = date::decimal(&[s0, s1]).ok_or(syntax)?;
    Ok((hour, minute, second, micro))
}

/// A stored field: writes only produce instants in range, so anything else was damaged on
/// disk.
pub(crate) fn decode(bytes: [u8; 8]) -> Result<Value, TypeError> {
    Timestamp::from_micros_since_epoch(i64::from_le_bytes(bytes))
        .map(Value::Timestamp)
        .ok_or(TypeError::CorruptedValue {
            ty: DataType::Timestamp,
            reason: "timestamp out of range",
        })
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (hour, minute, second, micro) = self.time_of_day();
        write!(f, "{} {hour:02}:{minute:02}:{second:02}", self.date())?;
        if micro != 0 {
            let fraction = format!("{micro:06}");
            write!(f, ".{}", fraction.trim_end_matches('0'))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> Date {
        Date::from_ymd(year, month, day).unwrap()
    }

    fn timestamp(day: Date, hour: u32, minute: u32, second: u32, micro: u32) -> Timestamp {
        Timestamp::new(day, hour, minute, second, micro).unwrap()
    }

    #[test]
    fn microseconds_should_count_from_1970_01_01_midnight() {
        assert_eq!(
            timestamp(date(1970, 1, 1), 0, 0, 0, 0).micros_since_epoch(),
            0
        );
        assert_eq!(
            timestamp(date(1970, 1, 1), 0, 0, 1, 5).micros_since_epoch(),
            1_000_005
        );
        assert_eq!(
            timestamp(date(1970, 1, 2), 0, 0, 0, 0).micros_since_epoch(),
            MICROS_PER_DAY
        );
    }

    #[test]
    fn the_microsecond_before_1970_should_be_the_last_of_1969() {
        let instant = Timestamp::from_micros_since_epoch(-1).unwrap();

        assert_eq!(instant.date(), date(1969, 12, 31));
        assert_eq!(instant.time_of_day(), (23, 59, 59, 999_999));
    }

    #[test]
    fn the_range_should_be_the_days_of_date() {
        assert_eq!(timestamp(Date::MIN, 0, 0, 0, 0), Timestamp::MIN);
        assert_eq!(timestamp(Date::MAX, 23, 59, 59, 999_999), Timestamp::MAX);
        assert_eq!(Timestamp::MIN.micros_since_epoch(), -62_135_596_800_000_000);
        assert_eq!(Timestamp::MAX.micros_since_epoch(), 253_402_300_799_999_999);
    }

    #[test]
    fn from_micros_since_epoch_should_accept_only_the_range() {
        let min = Timestamp::MIN.micros_since_epoch();
        let max = Timestamp::MAX.micros_since_epoch();

        assert_eq!(
            Timestamp::from_micros_since_epoch(min),
            Some(Timestamp::MIN)
        );
        assert_eq!(
            Timestamp::from_micros_since_epoch(max),
            Some(Timestamp::MAX)
        );
        for micros in [min - 1, max + 1, i64::MIN, i64::MAX] {
            assert_eq!(Timestamp::from_micros_since_epoch(micros), None, "{micros}");
        }
    }

    #[test]
    fn new_should_reject_time_fields_out_of_range() {
        let day = date(2024, 3, 15);
        let cases = [
            (24, 0, 0, 0),
            (0, 60, 0, 0),
            (0, 0, 60, 0),
            (0, 0, 0, 1_000_000),
        ];

        for (hour, minute, second, micro) in cases {
            assert_eq!(
                Timestamp::new(day, hour, minute, second, micro),
                None,
                "{hour}:{minute}:{second}.{micro}"
            );
        }
    }

    #[test]
    fn new_should_reject_huge_fields_without_overflowing() {
        let day = date(2024, 3, 15);
        let cases = [
            (u32::MAX, 0, 0, 0),
            (0, u32::MAX, 0, 0),
            (0, 0, u32::MAX, 0),
            (0, 0, 0, u32::MAX),
            (u32::MAX, u32::MAX, u32::MAX, u32::MAX),
        ];

        for (hour, minute, second, micro) in cases {
            assert_eq!(Timestamp::new(day, hour, minute, second, micro), None);
        }
    }

    #[test]
    fn date_and_time_of_day_should_invert_new() {
        let cases = [
            (Date::MIN, (0, 0, 0, 0)),
            (date(1969, 12, 31), (23, 59, 59, 999_999)),
            (date(1970, 1, 1), (0, 0, 0, 0)),
            (date(2024, 3, 15), (14, 30, 0, 250_000)),
            (Date::MAX, (23, 59, 59, 999_999)),
        ];

        for (day, (hour, minute, second, micro)) in cases {
            let instant = timestamp(day, hour, minute, second, micro);

            assert_eq!(instant.date(), day);
            assert_eq!(instant.time_of_day(), (hour, minute, second, micro));
        }
    }

    #[test]
    fn at_midnight_should_start_the_day() {
        for day in [Date::MIN, date(1969, 12, 31), date(2024, 2, 29), Date::MAX] {
            let midnight = Timestamp::at_midnight(day);

            assert_eq!(midnight, timestamp(day, 0, 0, 0, 0));
            assert_eq!(midnight.date(), day);
        }
    }

    /// Day and time of day by truncating `/` and `%`, corrected for negative instants: a
    /// different route from the implementation's `div_euclid` / `rem_euclid`.
    fn model_split(micros: i64) -> (i64, i64) {
        let (mut days, mut time) = (micros / MICROS_PER_DAY, micros % MICROS_PER_DAY);
        if time < 0 {
            days -= 1;
            time += MICROS_PER_DAY;
        }
        (days, time)
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

    /// Anywhere in the range, or next to a midnight, to the epoch or to the range ends.
    fn random_micros(rng: &mut Rng) -> i64 {
        let (min, max) = (Timestamp::MIN.0, Timestamp::MAX.0);
        let jitter = (rng.next() % 5) as i64 - 2;
        match rng.next() % 4 {
            0 => (min + jitter.abs()).min(max),
            1 => (max - jitter.abs()).max(min),
            2 => {
                let day = (rng.next() % 3_652_059) as i64 - 719_162;
                (day * MICROS_PER_DAY + jitter).clamp(min, max)
            }
            _ => min + (rng.next() % (max - min + 1).unsigned_abs()) as i64,
        }
    }

    #[test]
    fn random_instants_should_split_into_day_and_time_like_the_model() {
        let mut rng = Rng(0x243F_6A88_85A3_08D3);

        for _ in 0..10_000 {
            let micros = random_micros(&mut rng);
            let instant = Timestamp::from_micros_since_epoch(micros).unwrap();
            let (days, time) = model_split(micros);
            let seconds = time / MICROS_PER_SECOND;
            let expected_time = (
                (seconds / 3_600) as u32,
                (seconds / 60 % 60) as u32,
                (seconds % 60) as u32,
                (time % MICROS_PER_SECOND) as u32,
            );

            assert_eq!(
                instant.date(),
                Date::from_days_since_epoch(i32::try_from(days).unwrap()).unwrap(),
                "{micros}"
            );
            assert_eq!(instant.time_of_day(), expected_time, "{micros}");
            let (hour, minute, second, micro) = expected_time;
            assert_eq!(
                Timestamp::new(instant.date(), hour, minute, second, micro),
                Some(instant)
            );
        }
    }

    #[test]
    fn display_should_show_the_fraction_only_when_it_is_not_zero() {
        let day = date(2024, 3, 15);
        let cases = [
            (timestamp(day, 14, 30, 0, 0), "2024-03-15 14:30:00"),
            (timestamp(day, 14, 30, 0, 250_000), "2024-03-15 14:30:00.25"),
            (timestamp(day, 9, 5, 7, 1), "2024-03-15 09:05:07.000001"),
            (Timestamp::MIN, "0001-01-01 00:00:00"),
            (Timestamp::MAX, "9999-12-31 23:59:59.999999"),
        ];

        for (instant, expected) in cases {
            assert_eq!(instant.to_string(), expected);
        }
    }
}
