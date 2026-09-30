use crate::storage::tuple::ColumnLayout;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    SmallInt,
    Integer,
    BigInt,
}

impl DataType {
    pub fn name(self) -> &'static str {
        match self {
            DataType::SmallInt => "SMALLINT",
            DataType::Integer => "INTEGER",
            DataType::BigInt => "BIGINT",
        }
    }

    pub fn width(self) -> usize {
        match self {
            DataType::SmallInt => size_of::<i16>(),
            DataType::Integer => size_of::<i32>(),
            DataType::BigInt => size_of::<i64>(),
        }
    }

    pub fn layout(self) -> ColumnLayout {
        // At most 8 bytes, so the cast never truncates.
        ColumnLayout::Fixed(self.width() as u16)
    }

    /// Result type of `a op b`: the wider operand (`SMALLINT + INTEGER -> INTEGER`).
    pub fn promote(self, other: DataType) -> DataType {
        if self.width() >= other.width() {
            self
        } else {
            other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DataType::{BigInt, Integer, SmallInt};

    const ALL: [DataType; 3] = [SmallInt, Integer, BigInt];

    #[test]
    fn name_should_be_the_sql_keyword() {
        let names: Vec<_> = ALL.iter().map(|ty| ty.name()).collect();

        assert_eq!(names, ["SMALLINT", "INTEGER", "BIGINT"]);
    }

    #[test]
    fn width_should_match_the_rust_integer() {
        let widths: Vec<_> = ALL.iter().map(|ty| ty.width()).collect();

        assert_eq!(
            widths,
            [size_of::<i16>(), size_of::<i32>(), size_of::<i64>()]
        );
    }

    #[test]
    fn layout_should_be_fixed_with_the_type_width() {
        for ty in ALL {
            assert_eq!(ty.layout(), ColumnLayout::Fixed(ty.width() as u16));
        }
    }

    #[test]
    fn promote_should_pick_the_wider_type() {
        let expected = [
            [SmallInt, Integer, BigInt],
            [Integer, Integer, BigInt],
            [BigInt, BigInt, BigInt],
        ];

        for (lhs, row) in ALL.iter().zip(expected) {
            for (rhs, result) in ALL.iter().zip(row) {
                assert_eq!(lhs.promote(*rhs), result, "{lhs:?} op {rhs:?}");
            }
        }
    }

    #[test]
    fn promote_should_be_commutative() {
        for lhs in ALL {
            for rhs in ALL {
                assert_eq!(lhs.promote(rhs), rhs.promote(lhs));
            }
        }
    }
}
