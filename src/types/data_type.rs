use std::fmt;
use std::num::NonZeroU16;

use crate::storage::tuple::ColumnLayout;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    SmallInt,
    Integer,
    BigInt,
    /// At most `n` characters (Unicode scalar values); the type rules out `VARCHAR(0)`.
    Varchar(NonZeroU16),
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
            DataType::Varchar(_) => "VARCHAR",
        }
    }

    pub fn fixed_width(self) -> Option<usize> {
        match self {
            DataType::SmallInt => Some(size_of::<i16>()),
            DataType::Integer => Some(size_of::<i32>()),
            DataType::BigInt => Some(size_of::<i64>()),
            DataType::Varchar(_) => None,
        }
    }

    pub fn layout(self) -> ColumnLayout {
        match self.fixed_width() {
            // At most 8 bytes, so the cast never truncates.
            Some(width) => ColumnLayout::Fixed(width as u16),
            None => ColumnLayout::Variable,
        }
    }

    pub fn is_integer(self) -> bool {
        matches!(
            self,
            DataType::SmallInt | DataType::Integer | DataType::BigInt
        )
    }

    pub fn is_text(self) -> bool {
        matches!(self, DataType::Varchar(_))
    }

    /// Result type of `a op b` for integers: the wider operand (`SMALLINT + INTEGER -> INTEGER`).
    /// Meaningless for VARCHAR, so only the integer operators in `types` call it.
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
        let names: Vec<_> = [SmallInt, Integer, BigInt, varchar(5)]
            .iter()
            .map(|ty| ty.name())
            .collect();

        assert_eq!(names, ["SMALLINT", "INTEGER", "BIGINT", "VARCHAR"]);
    }

    #[test]
    fn display_should_show_the_varchar_length() {
        assert_eq!(Integer.to_string(), "INTEGER");
        assert_eq!(varchar(40).to_string(), "VARCHAR(40)");
        assert_eq!(DataType::VARCHAR_MAX.to_string(), "VARCHAR(65535)");
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
