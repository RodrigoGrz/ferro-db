use std::fmt;
use std::num::NonZeroU16;

use crate::storage::tuple::ColumnLayout;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    SmallInt,
    Integer,
    BigInt,
    /// IEEE 754 binary32.
    Real,
    /// IEEE 754 binary64.
    Double,
    /// At most `n` characters (Unicode scalar values); the type rules out `VARCHAR(0)`.
    Varchar(NonZeroU16),
    Boolean,
    /// A calendar day, 0001-01-01 to 9999-12-31.
    Date,
    /// A date and time of day to the microsecond, in the range of DATE.
    Timestamp,
}

/// Values of one family can be assigned and compared to each other; across families only an
/// explicit CAST converts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TypeFamily {
    /// Integers and floats: they mix in arithmetic and comparison.
    Numeric,
    Text,
    Boolean,
    /// DATE and TIMESTAMP; TIME will join them.
    Datetime,
}

impl DataType {
    /// The widest VARCHAR: the type of a text value that did not come from a column, such
    /// as the result of `||` or of a CAST from an integer.
    pub const VARCHAR_MAX: DataType = DataType::Varchar(NonZeroU16::MAX);

    pub fn name(self) -> &'static str {
        match self {
            DataType::SmallInt => "SMALLINT",
            DataType::Integer => "INTEGER",
            DataType::BigInt => "BIGINT",
            DataType::Real => "REAL",
            DataType::Double => "DOUBLE PRECISION",
            DataType::Varchar(_) => "VARCHAR",
            DataType::Boolean => "BOOLEAN",
            DataType::Date => "DATE",
            DataType::Timestamp => "TIMESTAMP",
        }
    }

    pub fn fixed_width(self) -> Option<usize> {
        match self {
            DataType::SmallInt => Some(size_of::<i16>()),
            DataType::Integer => Some(size_of::<i32>()),
            DataType::BigInt => Some(size_of::<i64>()),
            DataType::Real => Some(size_of::<f32>()),
            DataType::Double => Some(size_of::<f64>()),
            DataType::Varchar(_) => None,
            DataType::Boolean => Some(size_of::<bool>()),
            DataType::Date => Some(size_of::<i32>()),
            DataType::Timestamp => Some(size_of::<i64>()),
        }
    }

    pub fn layout(self) -> ColumnLayout {
        match self.fixed_width() {
            // At most 8 bytes, so the cast never truncates.
            Some(width) => ColumnLayout::Fixed(width as u16),
            None => ColumnLayout::Variable,
        }
    }

    /// The exact numeric types; `is_numeric` also takes the floats.
    pub fn is_integer(self) -> bool {
        matches!(
            self,
            DataType::SmallInt | DataType::Integer | DataType::BigInt
        )
    }

    pub fn is_float(self) -> bool {
        matches!(self, DataType::Real | DataType::Double)
    }

    pub fn is_numeric(self) -> bool {
        self.family() == TypeFamily::Numeric
    }

    pub fn is_text(self) -> bool {
        self.family() == TypeFamily::Text
    }

    pub fn is_datetime(self) -> bool {
        self.family() == TypeFamily::Datetime
    }

    pub fn is_boolean(self) -> bool {
        self.family() == TypeFamily::Boolean
    }

    pub(crate) fn family(self) -> TypeFamily {
        match self {
            DataType::SmallInt
            | DataType::Integer
            | DataType::BigInt
            | DataType::Real
            | DataType::Double => TypeFamily::Numeric,
            DataType::Varchar(_) => TypeFamily::Text,
            DataType::Boolean => TypeFamily::Boolean,
            DataType::Date | DataType::Timestamp => TypeFamily::Datetime,
        }
    }

    /// Result type of `a op b` for integers: the wider operand (`SMALLINT + INTEGER -> INTEGER`).
    /// Integers only: REAL has the width of INTEGER, so floats would get a wrong answer here;
    /// float arithmetic uses `float::result_type`.
    pub(crate) fn promote(self, other: DataType) -> DataType {
        if self.fixed_width() >= other.fixed_width() {
            self
        } else {
            other
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Varchar(max_chars) => write!(f, "VARCHAR({max_chars})"),
            integer => f.write_str(integer.name()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DataType::{BigInt, Integer, SmallInt};

    const INTEGERS: [DataType; 3] = [SmallInt, Integer, BigInt];

    fn varchar(max_chars: u16) -> DataType {
        DataType::Varchar(NonZeroU16::new(max_chars).unwrap())
    }

    #[test]
    fn name_should_be_the_sql_keyword() {
        let names: Vec<_> = [
            SmallInt,
            Integer,
            BigInt,
            DataType::Real,
            DataType::Double,
            varchar(5),
            DataType::Boolean,
        ]
        .iter()
        .map(|ty| ty.name())
        .collect();

        assert_eq!(
            names,
            [
                "SMALLINT",
                "INTEGER",
                "BIGINT",
                "REAL",
                "DOUBLE PRECISION",
                "VARCHAR",
                "BOOLEAN"
            ]
        );
    }

    #[test]
    fn display_should_show_the_varchar_length() {
        assert_eq!(Integer.to_string(), "INTEGER");
        assert_eq!(varchar(40).to_string(), "VARCHAR(40)");
        assert_eq!(DataType::VARCHAR_MAX.to_string(), "VARCHAR(65535)");
        assert_eq!(DataType::Boolean.to_string(), "BOOLEAN");
        assert_eq!(DataType::Double.to_string(), "DOUBLE PRECISION");
    }

    #[test]
    fn date_should_be_a_fixed_four_byte_datetime() {
        assert_eq!(DataType::Date.name(), "DATE");
        assert_eq!(DataType::Date.to_string(), "DATE");
        assert_eq!(DataType::Date.fixed_width(), Some(4));
        assert_eq!(DataType::Date.layout(), ColumnLayout::Fixed(4));
        assert!(DataType::Date.is_datetime());
        assert!(!DataType::Date.is_numeric());
        assert_eq!(DataType::Date.family(), TypeFamily::Datetime);
        for ty in [Integer, DataType::Double, varchar(3), DataType::Boolean] {
            assert!(!ty.is_datetime(), "{ty}");
        }
    }

    #[test]
    fn timestamp_should_be_a_fixed_eight_byte_datetime() {
        assert_eq!(DataType::Timestamp.name(), "TIMESTAMP");
        assert_eq!(DataType::Timestamp.to_string(), "TIMESTAMP");
        assert_eq!(DataType::Timestamp.fixed_width(), Some(8));
        assert_eq!(DataType::Timestamp.layout(), ColumnLayout::Fixed(8));
        assert!(DataType::Timestamp.is_datetime());
        assert_eq!(DataType::Timestamp.family(), DataType::Date.family());
    }

    #[test]
    fn floats_should_be_fixed_ieee_754_widths() {
        assert_eq!(DataType::Real.fixed_width(), Some(4));
        assert_eq!(DataType::Double.fixed_width(), Some(8));
        assert_eq!(DataType::Real.layout(), ColumnLayout::Fixed(4));
        assert_eq!(DataType::Double.layout(), ColumnLayout::Fixed(8));
    }

    #[test]
    fn floats_should_be_numeric_but_not_integers() {
        for ty in [DataType::Real, DataType::Double] {
            assert!(ty.is_float());
            assert!(ty.is_numeric());
            assert!(!ty.is_integer());
            assert_eq!(ty.family(), TypeFamily::Numeric);
        }
        for ty in INTEGERS {
            assert!(ty.is_numeric());
            assert!(!ty.is_float());
        }
        assert!(!varchar(1).is_numeric());
        assert!(!DataType::Boolean.is_numeric());
    }

    #[test]
    fn boolean_should_be_one_fixed_byte() {
        assert_eq!(DataType::Boolean.fixed_width(), Some(1));
        assert_eq!(DataType::Boolean.layout(), ColumnLayout::Fixed(1));
    }

    #[test]
    fn fixed_width_should_match_the_rust_integer() {
        let widths: Vec<_> = INTEGERS.iter().map(|ty| ty.fixed_width()).collect();

        assert_eq!(
            widths,
            [
                Some(size_of::<i16>()),
                Some(size_of::<i32>()),
                Some(size_of::<i64>())
            ]
        );
    }

    #[test]
    fn varchar_should_have_no_fixed_width() {
        assert_eq!(varchar(1).fixed_width(), None);
        assert_eq!(DataType::VARCHAR_MAX.fixed_width(), None);
    }

    #[test]
    fn layout_should_be_fixed_for_integers_and_variable_for_varchar() {
        assert_eq!(SmallInt.layout(), ColumnLayout::Fixed(2));
        assert_eq!(Integer.layout(), ColumnLayout::Fixed(4));
        assert_eq!(BigInt.layout(), ColumnLayout::Fixed(8));
        assert_eq!(varchar(40).layout(), ColumnLayout::Variable);
    }

    #[test]
    fn families_should_be_told_apart() {
        for ty in INTEGERS {
            assert!(ty.is_integer());
            assert!(!ty.is_text());
        }
        assert!(varchar(3).is_text());
        assert!(!varchar(3).is_integer());
        assert!(!varchar(3).is_boolean());
        assert!(DataType::Boolean.is_boolean());
        assert!(!DataType::Boolean.is_integer());
        assert!(!DataType::Boolean.is_text());
    }

    #[test]
    fn family_should_group_the_integer_widths_and_every_varchar() {
        for ty in INTEGERS {
            assert_eq!(ty.family(), TypeFamily::Numeric);
        }
        assert_eq!(varchar(1).family(), TypeFamily::Text);
        assert_eq!(DataType::VARCHAR_MAX.family(), TypeFamily::Text);
        assert_eq!(DataType::Boolean.family(), TypeFamily::Boolean);
    }

    #[test]
    fn varchar_max_should_be_the_widest_varchar() {
        assert_eq!(DataType::VARCHAR_MAX, varchar(u16::MAX));
    }

    #[test]
    fn promote_should_pick_the_wider_type() {
        let expected = [
            [SmallInt, Integer, BigInt],
            [Integer, Integer, BigInt],
            [BigInt, BigInt, BigInt],
        ];

        for (lhs, row) in INTEGERS.iter().zip(expected) {
            for (rhs, result) in INTEGERS.iter().zip(row) {
                assert_eq!(lhs.promote(*rhs), result, "{lhs:?} op {rhs:?}");
            }
        }
    }

    #[test]
    fn promote_should_be_commutative() {
        for lhs in INTEGERS {
            for rhs in INTEGERS {
                assert_eq!(lhs.promote(rhs), rhs.promote(lhs));
            }
        }
    }
}
