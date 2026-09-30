//! Physical tuple format (all integers little-endian, no alignment):
//!
//! ```text
//! [0..2)                     column_count  u16, must match the layout
//! [2..2 + ceil(n/8))         NULL bitmap   bit i % 8 of byte i / 8 set => column i is NULL
//! [.. data_start)            fixed part    per column, in order:
//!                                            Fixed(w) -> w bytes (zeroed when NULL)
//!                                            Variable -> (offset u16, len u16), offset from tuple start
//! [data_start..)             variable data bytes of the variable columns, in column order
//! ```
//!
//! Fields are raw bytes: this layer knows widths, not SQL types. Every fixed column
//! keeps its space even when NULL, so field offsets are known from the layout alone.
//!
//! `TupleRef::new` accepts only the canonical form `encode` produces (zeroed NULL
//! fixed fields, contiguous variable data in column order, no trailing bytes), so
//! a row has exactly one byte representation.

use std::fmt;

use crate::config::MAX_PAGE_SIZE;
use crate::serialization::{ByteReader, ByteWriter, SerializationError};

const COLUMN_COUNT_SIZE: usize = 2;
const BITMAP_OFFSET: usize = COLUMN_COUNT_SIZE;
const VARIABLE_SLOT_SIZE: usize = 4;
/// Limit of the tuple format itself (variable slots store `u16` offsets). The
/// page holding the tuple is stricter: `SlottedPage::max_tuple_size` (8164 B on
/// an 8 KB page) is enforced by `SlottedPage::insert`/`update`.
pub const MAX_TUPLE_SIZE: usize = MAX_PAGE_SIZE;

#[derive(Debug, PartialEq, Eq)]
pub enum TupleError {
    TooManyColumns {
        count: usize,
        max: usize,
    },
    ZeroWidthColumn {
        column: usize,
    },
    ColumnCountMismatch {
        expected: usize,
        actual: usize,
    },
    FixedWidthMismatch {
        column: usize,
        expected: usize,
        actual: usize,
    },
    TupleTooLarge {
        size: usize,
        max: usize,
    },
    ColumnOutOfRange {
        column: usize,
        count: usize,
    },
    Corrupted {
        reason: &'static str,
    },
    Serialization(SerializationError),
}

impl fmt::Display for TupleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TupleError::TooManyColumns { count, max } => {
                write!(f, "tuple has {count} columns, maximum is {max}")
            }
            TupleError::ZeroWidthColumn { column } => {
                write!(f, "fixed column {column} must be at least 1 byte wide")
            }
            TupleError::ColumnCountMismatch { expected, actual } => {
                write!(f, "expected {expected} columns, got {actual}")
            }
            TupleError::FixedWidthMismatch {
                column,
                expected,
                actual,
            } => write!(
                f,
                "column {column} must be {expected} bytes wide, got {actual}"
            ),
            TupleError::TupleTooLarge { size, max } => {
                write!(f, "tuple of {size} bytes exceeds maximum of {max} bytes")
            }
            TupleError::ColumnOutOfRange { column, count } => {
                write!(f, "column {column} does not exist (tuple has {count})")
            }
            TupleError::Corrupted { reason } => write!(f, "tuple is corrupted: {reason}"),
            TupleError::Serialization(err) => write!(f, "tuple is corrupted: {err}"),
        }
    }
}

impl std::error::Error for TupleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TupleError::Serialization(err) => Some(err),
            _ => None,
        }
    }
}

impl From<SerializationError> for TupleError {
    fn from(err: SerializationError) -> Self {
        TupleError::Serialization(err)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnLayout {
    Fixed(u16),
    Variable,
}

impl ColumnLayout {
    /// Bytes the column takes in the fixed part of the tuple.
    fn fixed_part_width(self) -> usize {
        match self {
            ColumnLayout::Fixed(width) => width.into(),
            ColumnLayout::Variable => VARIABLE_SLOT_SIZE,
        }
    }
}

fn bitmap_len(column_count: usize) -> usize {
    column_count.div_ceil(8)
}

fn write_null_bitmap(
    writer: &mut ByteWriter<'_>,
    fields: &[Option<&[u8]>],
) -> Result<(), SerializationError> {
    for chunk in fields.chunks(8) {
        let byte = chunk
            .iter()
            .enumerate()
            .filter(|(_, field)| field.is_none())
            .fold(0u8, |byte, (bit, _)| byte | 1 << bit);
        writer.write_u8(byte)?;
    }
    Ok(())
}

/// Offsets and lengths inside a tuple fit `u16` once the tuple size was checked.
fn to_u16(value: usize) -> Result<u16, TupleError> {
    u16::try_from(value).map_err(|_| TupleError::TupleTooLarge {
        size: value,
        max: MAX_TUPLE_SIZE,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TupleLayout {
    columns: Vec<ColumnLayout>,
    field_offsets: Vec<usize>,
    data_start: usize,
}

impl TupleLayout {
    /// Same limit as PostgreSQL's `MaxHeapAttributeNumber`.
    pub const MAX_COLUMNS: usize = 1600;

    pub fn new(columns: Vec<ColumnLayout>) -> Result<Self, TupleError> {
        if columns.len() > Self::MAX_COLUMNS {
            return Err(TupleError::TooManyColumns {
                count: columns.len(),
                max: Self::MAX_COLUMNS,
            });
        }
        if let Some(column) = columns.iter().position(|&c| c == ColumnLayout::Fixed(0)) {
            return Err(TupleError::ZeroWidthColumn { column });
        }

        let mut field_offsets = Vec::with_capacity(columns.len());
        let mut offset = BITMAP_OFFSET + bitmap_len(columns.len());
        for column in &columns {
            field_offsets.push(offset);
            offset += column.fixed_part_width();
        }
        if offset > MAX_TUPLE_SIZE {
            return Err(TupleError::TupleTooLarge {
                size: offset,
                max: MAX_TUPLE_SIZE,
            });
        }

        Ok(Self {
            columns,
            field_offsets,
            data_start: offset,
        })
    }

    pub fn columns(&self) -> &[ColumnLayout] {
        &self.columns
    }

    /// Size of a tuple whose variable columns are all empty.
    pub fn min_tuple_size(&self) -> usize {
        self.data_start
    }

    /// Encodes one row, `None` meaning NULL. Fails before allocating anything
    /// when a field does not match the layout.
    pub fn encode(&self, fields: &[Option<&[u8]>]) -> Result<Vec<u8>, TupleError> {
        let size = self.encoded_size(fields)?;
        let mut tuple = vec![0; size];
        let mut writer = ByteWriter::new(&mut tuple);

        writer.write_u16(to_u16(self.columns.len())?)?;
        write_null_bitmap(&mut writer, fields)?;

        let mut data_end = self.data_start;
        for (&column, &field) in self.columns.iter().zip(fields) {
            match (column, field) {
                (ColumnLayout::Fixed(width), None) => writer.write_zeros(width.into())?,
                (ColumnLayout::Fixed(_), Some(bytes)) => writer.write_bytes(bytes)?,
                (ColumnLayout::Variable, field) => {
                    let len = field.map_or(0, <[u8]>::len);
                    writer.write_u16(to_u16(data_end)?)?;
                    writer.write_u16(to_u16(len)?)?;
                    data_end += len;
                }
            }
        }

        let variable_fields = self
            .columns
            .iter()
            .zip(fields)
            .filter(|(column, _)| **column == ColumnLayout::Variable)
            .filter_map(|(_, field)| *field);
        for bytes in variable_fields {
            writer.write_bytes(bytes)?;
        }

        Ok(tuple)
    }

    fn encoded_size(&self, fields: &[Option<&[u8]>]) -> Result<usize, TupleError> {
        if fields.len() != self.columns.len() {
            return Err(TupleError::ColumnCountMismatch {
                expected: self.columns.len(),
                actual: fields.len(),
            });
        }

        let mut size = self.data_start;
        for (column, (&layout, &field)) in self.columns.iter().zip(fields).enumerate() {
            match (layout, field) {
                (ColumnLayout::Fixed(width), Some(bytes)) if bytes.len() != usize::from(width) => {
                    return Err(TupleError::FixedWidthMismatch {
                        column,
                        expected: width.into(),
                        actual: bytes.len(),
                    });
                }
                (ColumnLayout::Variable, Some(bytes)) => size = size.saturating_add(bytes.len()),
                _ => {}
            }
        }

        if size > MAX_TUPLE_SIZE {
            return Err(TupleError::TupleTooLarge {
                size,
                max: MAX_TUPLE_SIZE,
            });
        }
        Ok(size)
    }
}

/// Validated zero-copy view over the bytes of one tuple (e.g. from `SlottedPage::get`).
#[derive(Debug, Clone, Copy)]
pub struct TupleRef<'a> {
    layout: &'a TupleLayout,
    bytes: &'a [u8],
}

impl<'a> TupleRef<'a> {
    pub fn new(layout: &'a TupleLayout, bytes: &'a [u8]) -> Result<Self, TupleError> {
        let stored_count = usize::from(read_u16_at(bytes, 0)?);
        if stored_count != layout.columns.len() {
            return Err(TupleError::ColumnCountMismatch {
                expected: layout.columns.len(),
                actual: stored_count,
            });
        }
        if bytes.len() < layout.data_start {
            return Err(TupleError::Corrupted {
                reason: "tuple is shorter than its fixed part",
            });
        }
        let tuple = Self { layout, bytes };
        tuple.validate()?;
        Ok(tuple)
    }

    /// Bytes come from disk, so every field is checked once here and `field`
    /// can trust them afterwards. Only the exact shape `encode` produces is
    /// accepted, so each row has a single byte representation.
    fn validate(&self) -> Result<(), TupleError> {
        self.check_bitmap_padding()?;
        let mut data_end = self.layout.data_start;
        for (column, &layout) in self.layout.columns.iter().enumerate() {
            let (start, len) = self.field_range(column)?;
            let is_null = self.is_null(column)?;
            match layout {
                ColumnLayout::Fixed(_) if is_null => self.check_zeroed(start, len)?,
                ColumnLayout::Fixed(_) => {}
                ColumnLayout::Variable => {
                    self.check_variable_field(start, len, is_null, data_end)?;
                    data_end += len;
                }
            }
        }
        if data_end != self.bytes.len() {
            return Err(TupleError::Corrupted {
                reason: "tuple has trailing bytes",
            });
        }
        Ok(())
    }

    fn check_zeroed(&self, start: usize, len: usize) -> Result<(), TupleError> {
        let is_zeroed = self
            .bytes
            .get(start..start + len)
            .is_some_and(|bytes| bytes.iter().all(|&b| b == 0));
        if !is_zeroed {
            return Err(TupleError::Corrupted {
                reason: "null fixed field has data",
            });
        }
        Ok(())
    }

    fn check_variable_field(
        &self,
        start: usize,
        len: usize,
        is_null: bool,
        expected_start: usize,
    ) -> Result<(), TupleError> {
        let reason = if is_null && len != 0 {
            "null variable field has data"
        } else if start < self.layout.data_start {
            "variable field overlaps the fixed part"
        } else if start + len > self.bytes.len() {
            "variable field exceeds tuple"
        } else if start != expected_start {
            "variable fields are not contiguous"
        } else {
            return Ok(());
        };
        Err(TupleError::Corrupted { reason })
    }

    fn check_bitmap_padding(&self) -> Result<(), TupleError> {
        let used_bits = self.column_count() % 8;
        if used_bits == 0 {
            return Ok(());
        }
        let last_byte = self
            .bytes
            .get(BITMAP_OFFSET + self.column_count() / 8)
            .copied()
            .unwrap_or_default();
        if last_byte >> used_bits != 0 {
            return Err(TupleError::Corrupted {
                reason: "null bitmap has bits past the last column",
            });
        }
        Ok(())
    }

    pub fn column_count(&self) -> usize {
        self.layout.columns.len()
    }

    pub fn is_null(&self, column: usize) -> Result<bool, TupleError> {
        self.check_column(column)?;
        let byte = self
            .bytes
            .get(BITMAP_OFFSET + column / 8)
            .ok_or(TupleError::Corrupted {
                reason: "null bitmap exceeds tuple",
            })?;
        Ok(byte >> (column % 8) & 1 == 1)
    }

    pub fn field(&self, column: usize) -> Result<Option<&'a [u8]>, TupleError> {
        if self.is_null(column)? {
            return Ok(None);
        }
        let (start, len) = self.field_range(column)?;
        self.bytes
            .get(start..start + len)
            .map(Some)
            .ok_or(TupleError::Corrupted {
                reason: "field exceeds tuple",
            })
    }

    /// Fields in column order, `None` for NULL. `new` already validated every
    /// field, so no column is skipped.
    pub fn fields(&self) -> impl Iterator<Item = Option<&'a [u8]>> + 'a {
        let tuple = *self;
        (0..tuple.column_count()).map(move |column| {
            let field = tuple.field(column);
            debug_assert!(field.is_ok(), "validated tuple failed on column {column}");
            field.ok().flatten()
        })
    }

    fn check_column(&self, column: usize) -> Result<(), TupleError> {
        if column >= self.column_count() {
            return Err(self.out_of_range(column));
        }
        Ok(())
    }

    fn out_of_range(&self, column: usize) -> TupleError {
        TupleError::ColumnOutOfRange {
            column,
            count: self.column_count(),
        }
    }

    fn field_range(&self, column: usize) -> Result<(usize, usize), TupleError> {
        let (Some(&layout), Some(&offset)) = (
            self.layout.columns.get(column),
            self.layout.field_offsets.get(column),
        ) else {
            return Err(self.out_of_range(column));
        };
        match layout {
            ColumnLayout::Fixed(width) => Ok((offset, width.into())),
            ColumnLayout::Variable => Ok((
                read_u16_at(self.bytes, offset)?.into(),
                read_u16_at(self.bytes, offset + 2)?.into(),
            )),
        }
    }
}

fn read_u16_at(bytes: &[u8], offset: usize) -> Result<u16, TupleError> {
    let tail = bytes.get(offset..).unwrap_or_default();
    Ok(ByteReader::new(tail).read_u16()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ColumnLayout::{Fixed, Variable};

    #[test]
    fn layout_without_columns_should_hold_only_the_column_count() {
        let layout = TupleLayout::new(vec![]).unwrap();

        assert!(layout.columns().is_empty());
        assert_eq!(layout.min_tuple_size(), 2);
    }

    #[test]
    fn layout_with_one_fixed_column_should_place_it_after_the_bitmap() {
        let layout = TupleLayout::new(vec![Fixed(4)]).unwrap();

        assert_eq!(layout.field_offsets, vec![3]);
        assert_eq!(layout.min_tuple_size(), 2 + 1 + 4);
    }

    #[test]
    fn layout_with_one_variable_column_should_reserve_a_slot() {
        let layout = TupleLayout::new(vec![Variable]).unwrap();

        assert_eq!(layout.field_offsets, vec![3]);
        assert_eq!(layout.min_tuple_size(), 2 + 1 + 4);
    }

    #[test]
    fn layout_should_compute_offsets_of_mixed_columns() {
        let layout = TupleLayout::new(vec![Fixed(4), Variable, Fixed(1)]).unwrap();

        assert_eq!(layout.field_offsets, vec![3, 7, 11]);
        assert_eq!(layout.min_tuple_size(), 12);
        assert_eq!(layout.columns(), &[Fixed(4), Variable, Fixed(1)]);
    }

    #[test]
    fn bitmap_should_grow_one_byte_every_eight_columns() {
        let eight = TupleLayout::new(vec![Fixed(1); 8]).unwrap();
        let nine = TupleLayout::new(vec![Fixed(1); 9]).unwrap();

        assert_eq!(eight.min_tuple_size(), 2 + 1 + 8);
        assert_eq!(nine.min_tuple_size(), 2 + 2 + 9);
    }

    #[test]
    fn layout_should_reject_zero_width_fixed_column() {
        let err = TupleLayout::new(vec![Variable, Fixed(0)]).unwrap_err();

        assert_eq!(err, TupleError::ZeroWidthColumn { column: 1 });
    }

    #[test]
    fn layout_should_accept_the_maximum_number_of_columns() {
        let layout = TupleLayout::new(vec![Fixed(1); TupleLayout::MAX_COLUMNS]).unwrap();

        assert_eq!(layout.columns().len(), TupleLayout::MAX_COLUMNS);
    }

    #[test]
    fn layout_should_reject_more_than_the_maximum_number_of_columns() {
        let err = TupleLayout::new(vec![Fixed(1); TupleLayout::MAX_COLUMNS + 1]).unwrap_err();

        assert_eq!(
            err,
            TupleError::TooManyColumns {
                count: TupleLayout::MAX_COLUMNS + 1,
                max: TupleLayout::MAX_COLUMNS
            }
        );
    }

    #[test]
    fn layout_whose_fixed_part_cannot_fit_a_page_should_fail() {
        let err = TupleLayout::new(vec![Fixed(u16::MAX)]).unwrap_err();

        assert_eq!(
            err,
            TupleError::TupleTooLarge {
                size: 2 + 1 + u16::MAX as usize,
                max: MAX_TUPLE_SIZE
            }
        );
    }

    fn example_layout() -> TupleLayout {
        TupleLayout::new(vec![Fixed(4), Variable, Fixed(1)]).unwrap()
    }

    #[test]
    fn encode_should_produce_the_documented_bytes() {
        let layout = example_layout();

        let tuple = layout
            .encode(&[Some(&42u32.to_le_bytes()), Some(b"ab"), None])
            .unwrap();

        assert_eq!(
            tuple,
            [3, 0, 0b100, 42, 0, 0, 0, 12, 0, 2, 0, 0, b'a', b'b']
        );
    }

    #[test]
    fn encode_should_append_variable_fields_in_column_order() {
        let layout = TupleLayout::new(vec![Variable, Fixed(1), Variable]).unwrap();

        let tuple = layout
            .encode(&[Some(b"xyz"), Some(&[7]), Some(b"q")])
            .unwrap();

        assert_eq!(
            tuple,
            [3, 0, 0, 12, 0, 3, 0, 7, 15, 0, 1, 0, b'x', b'y', b'z', b'q']
        );
    }

    #[test]
    fn encode_all_null_should_set_every_bit_and_zero_the_fixed_part() {
        let mut columns = vec![Fixed(2); 8];
        columns.push(Variable);
        let layout = TupleLayout::new(columns).unwrap();

        let tuple = layout.encode(&[None; 9]).unwrap();

        let data_start = 2 + 2 + 16 + 4;
        let mut expected = vec![9, 0, 0xFF, 0x01];
        expected.extend([0; 16]);
        expected.extend([data_start as u8, 0, 0, 0]);
        assert_eq!(tuple, expected);
    }

    #[test]
    fn encode_should_distinguish_empty_from_null() {
        let layout = TupleLayout::new(vec![Variable]).unwrap();

        let empty = layout.encode(&[Some(&[])]).unwrap();
        let null = layout.encode(&[None]).unwrap();

        assert_eq!(empty, [1, 0, 0, 7, 0, 0, 0]);
        assert_eq!(null, [1, 0, 1, 7, 0, 0, 0]);
    }

    #[test]
    fn encode_of_layout_without_columns_should_write_only_the_count() {
        let layout = TupleLayout::new(vec![]).unwrap();

        assert_eq!(layout.encode(&[]).unwrap(), [0, 0]);
    }

    #[test]
    fn encode_with_wrong_number_of_fields_should_fail() {
        let layout = example_layout();

        let err = layout.encode(&[Some(&[0; 4]), None]).unwrap_err();

        assert_eq!(
            err,
            TupleError::ColumnCountMismatch {
                expected: 3,
                actual: 2
            }
        );
    }

    #[test]
    fn encode_with_wrong_fixed_width_should_fail() {
        let layout = example_layout();

        let err = layout
            .encode(&[Some(&[0; 4]), None, Some(&[1, 2])])
            .unwrap_err();

        assert_eq!(
            err,
            TupleError::FixedWidthMismatch {
                column: 2,
                expected: 1,
                actual: 2
            }
        );
    }

    #[test]
    fn encode_filling_exactly_the_maximum_tuple_size_should_work() {
        let layout = TupleLayout::new(vec![Variable]).unwrap();
        let field = vec![9; MAX_TUPLE_SIZE - layout.min_tuple_size()];

        let tuple = layout.encode(&[Some(&field)]).unwrap();

        assert_eq!(tuple.len(), MAX_TUPLE_SIZE);
    }

    #[test]
    fn encode_larger_than_the_maximum_tuple_size_should_fail() {
        let layout = TupleLayout::new(vec![Variable, Variable]).unwrap();
        let half = vec![9; MAX_TUPLE_SIZE / 2];

        let err = layout.encode(&[Some(&half), Some(&half)]).unwrap_err();

        assert_eq!(
            err,
            TupleError::TupleTooLarge {
                size: layout.min_tuple_size() + 2 * half.len(),
                max: MAX_TUPLE_SIZE
            }
        );
    }

    #[test]
    fn tuple_ref_should_read_back_each_field() {
        let layout = example_layout();
        let bytes = layout
            .encode(&[Some(&42u32.to_le_bytes()), Some(b"ab"), None])
            .unwrap();

        let tuple = TupleRef::new(&layout, &bytes).unwrap();

        assert_eq!(tuple.column_count(), 3);
        assert_eq!(tuple.field(0).unwrap(), Some(&42u32.to_le_bytes()[..]));
        assert_eq!(tuple.field(1).unwrap(), Some(&b"ab"[..]));
        assert_eq!(tuple.field(2).unwrap(), None);
    }

    #[test]
    fn is_null_should_follow_the_bitmap() {
        let layout = example_layout();
        let bytes = layout.encode(&[None, Some(b""), None]).unwrap();

        let tuple = TupleRef::new(&layout, &bytes).unwrap();

        assert!(tuple.is_null(0).unwrap());
        assert!(!tuple.is_null(1).unwrap());
        assert!(tuple.is_null(2).unwrap());
    }

    #[test]
    fn tuple_ref_should_keep_empty_and_null_apart() {
        let layout = TupleLayout::new(vec![Variable, Variable]).unwrap();
        let bytes = layout.encode(&[Some(&[]), None]).unwrap();

        let tuple = TupleRef::new(&layout, &bytes).unwrap();

        assert_eq!(tuple.field(0).unwrap(), Some(&[][..]));
        assert_eq!(tuple.field(1).unwrap(), None);
    }

    #[test]
    fn fields_should_iterate_in_column_order() {
        let layout = TupleLayout::new(vec![Variable, Fixed(2), Variable, Fixed(1)]).unwrap();
        let input: [Option<&[u8]>; 4] = [Some(b"hello"), Some(&[1, 2]), None, Some(&[9])];
        let bytes = layout.encode(&input).unwrap();

        let tuple = TupleRef::new(&layout, &bytes).unwrap();

        assert_eq!(tuple.fields().collect::<Vec<_>>(), input);
    }

    #[test]
    fn tuple_ref_of_layout_without_columns_should_have_no_fields() {
        let layout = TupleLayout::new(vec![]).unwrap();

        let tuple = TupleRef::new(&layout, &[0, 0]).unwrap();

        assert_eq!(tuple.column_count(), 0);
        assert_eq!(tuple.fields().count(), 0);
    }

    #[test]
    fn field_out_of_range_should_fail() {
        let layout = example_layout();
        let bytes = layout.encode(&[None, None, None]).unwrap();
        let tuple = TupleRef::new(&layout, &bytes).unwrap();

        for column in [3, usize::MAX] {
            let expected = TupleError::ColumnOutOfRange { column, count: 3 };
            assert_eq!(tuple.field(column).unwrap_err(), expected);
            assert_eq!(tuple.is_null(column).unwrap_err(), expected);
        }
    }

    fn corrupted(reason: &'static str) -> TupleError {
        TupleError::Corrupted { reason }
    }

    /// `[Fixed(1), Variable]` with fields `(7, "abc")`: slot at 4..8, data at 8..11.
    fn variable_tuple() -> (TupleLayout, Vec<u8>) {
        let layout = TupleLayout::new(vec![Fixed(1), Variable]).unwrap();
        let bytes = layout.encode(&[Some(&[7]), Some(b"abc")]).unwrap();
        (layout, bytes)
    }

    #[test]
    fn tuple_ref_of_empty_or_one_byte_should_fail_with_eof() {
        let layout = example_layout();

        for bytes in [&[][..], &[3][..]] {
            let err = TupleRef::new(&layout, bytes).unwrap_err();

            assert_eq!(
                err,
                TupleError::Serialization(SerializationError::UnexpectedEof {
                    needed: 2,
                    remaining: bytes.len()
                })
            );
        }
    }

    #[test]
    fn tuple_ref_truncated_inside_the_fixed_part_should_fail() {
        let layout = example_layout();
        let bytes = layout.encode(&[None, None, None]).unwrap();

        let err = TupleRef::new(&layout, &bytes[..layout.min_tuple_size() - 1]).unwrap_err();

        assert_eq!(err, corrupted("tuple is shorter than its fixed part"));
    }

    #[test]
    fn tuple_ref_with_other_column_count_should_fail() {
        let layout = example_layout();
        let other = TupleLayout::new(vec![Fixed(4), Variable]).unwrap();
        let bytes = other.encode(&[None, None]).unwrap();

        let err = TupleRef::new(&layout, &bytes).unwrap_err();

        assert_eq!(
            err,
            TupleError::ColumnCountMismatch {
                expected: 3,
                actual: 2
            }
        );
    }

    #[test]
    fn variable_field_starting_inside_the_fixed_part_should_fail() {
        let (layout, mut bytes) = variable_tuple();
        bytes[4] = 7;

        let err = TupleRef::new(&layout, &bytes).unwrap_err();

        assert_eq!(err, corrupted("variable field overlaps the fixed part"));
    }

    #[test]
    fn variable_field_past_the_end_should_fail() {
        let (layout, mut bytes) = variable_tuple();
        bytes[6] = 4;

        let err = TupleRef::new(&layout, &bytes).unwrap_err();

        assert_eq!(err, corrupted("variable field exceeds tuple"));
    }

    #[test]
    fn variable_field_with_huge_offset_and_length_should_fail_without_overflow() {
        let (layout, mut bytes) = variable_tuple();
        bytes[4..8].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);

        let err = TupleRef::new(&layout, &bytes).unwrap_err();

        assert_eq!(err, corrupted("variable field exceeds tuple"));
    }

    #[test]
    fn null_variable_field_with_data_should_fail() {
        let (layout, mut bytes) = variable_tuple();
        bytes[2] = 0b10;

        let err = TupleRef::new(&layout, &bytes).unwrap_err();

        assert_eq!(err, corrupted("null variable field has data"));
    }

    #[test]
    fn bitmap_bits_past_the_last_column_should_fail() {
        let (layout, mut bytes) = variable_tuple();
        bytes[2] = 0b100;

        let err = TupleRef::new(&layout, &bytes).unwrap_err();

        assert_eq!(err, corrupted("null bitmap has bits past the last column"));
    }

    #[test]
    fn null_fixed_field_with_non_zero_bytes_should_fail() {
        let layout = TupleLayout::new(vec![Fixed(2)]).unwrap();
        let mut bytes = layout.encode(&[None]).unwrap();
        bytes[4] = 0xFF;

        let err = TupleRef::new(&layout, &bytes).unwrap_err();

        assert_eq!(err, corrupted("null fixed field has data"));
    }

    #[test]
    fn overlapping_or_reordered_variable_fields_should_fail() {
        let layout = TupleLayout::new(vec![Variable, Variable]).unwrap();
        let bytes = layout.encode(&[Some(b"ab"), Some(b"cd")]).unwrap();
        let mut overlapping = bytes.clone();
        overlapping[7] = 11;
        let mut reordered = bytes.clone();
        reordered[3] = 13;
        reordered[7] = 11;

        for corrupted_bytes in [overlapping, reordered] {
            let err = TupleRef::new(&layout, &corrupted_bytes).unwrap_err();

            assert_eq!(err, corrupted("variable fields are not contiguous"));
        }
    }

    #[test]
    fn trailing_bytes_after_the_last_field_should_fail() {
        let (variable_layout, mut with_variable) = variable_tuple();
        with_variable.push(0);
        let fixed_layout = TupleLayout::new(vec![Fixed(1)]).unwrap();
        let mut fixed_only = fixed_layout.encode(&[Some(&[1])]).unwrap();
        fixed_only.push(0);

        let errors = [
            TupleRef::new(&variable_layout, &with_variable).unwrap_err(),
            TupleRef::new(&fixed_layout, &fixed_only).unwrap_err(),
        ];

        for err in errors {
            assert_eq!(err, corrupted("tuple has trailing bytes"));
        }
    }

    #[test]
    fn full_bitmap_byte_should_have_no_padding_to_check() {
        let layout = TupleLayout::new(vec![Fixed(1); 8]).unwrap();
        let bytes = layout.encode(&[None; 8]).unwrap();

        let tuple = TupleRef::new(&layout, &bytes).unwrap();

        assert!(tuple.fields().all(|field| field.is_none()));
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

        fn bytes(&mut self, len: usize) -> Vec<u8> {
            (0..len).map(|_| self.next() as u8).collect()
        }
    }

    fn random_layout(rng: &mut Rng) -> TupleLayout {
        let columns = (0..rng.below(20))
            .map(|_| match rng.below(3) {
                0 => Variable,
                _ => Fixed(1 + rng.below(16) as u16),
            })
            .collect();
        TupleLayout::new(columns).unwrap()
    }

    fn random_fields(rng: &mut Rng, layout: &TupleLayout) -> Vec<Option<Vec<u8>>> {
        layout
            .columns()
            .iter()
            .map(|&column| match (rng.below(4), column) {
                (0, _) => None,
                (_, Fixed(width)) => Some(rng.bytes(width.into())),
                (_, Variable) => {
                    let len = rng.below(40);
                    Some(rng.bytes(len))
                }
            })
            .collect()
    }

    fn as_borrowed(fields: &[Option<Vec<u8>>]) -> Vec<Option<&[u8]>> {
        fields.iter().map(Option::as_deref).collect()
    }

    #[test]
    fn random_tuples_should_round_trip() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

        for _ in 0..1_000 {
            let layout = random_layout(&mut rng);
            let model = random_fields(&mut rng, &layout);
            let input = as_borrowed(&model);

            let bytes = layout.encode(&input).unwrap();
            let tuple = TupleRef::new(&layout, &bytes).unwrap();

            assert_eq!(tuple.fields().collect::<Vec<_>>(), input);
            for (column, field) in input.iter().enumerate() {
                assert_eq!(tuple.field(column).unwrap(), *field);
                assert_eq!(tuple.is_null(column).unwrap(), field.is_none());
            }
        }
    }

    #[test]
    fn random_corruption_should_never_panic() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);

        for _ in 0..5_000 {
            let layout = random_layout(&mut rng);
            let model = random_fields(&mut rng, &layout);
            let mut bytes = layout.encode(&as_borrowed(&model)).unwrap();
            match rng.below(3) {
                0 => bytes.truncate(rng.below(bytes.len() + 1)),
                _ => {
                    for _ in 0..1 + rng.below(4) {
                        let position = rng.below(bytes.len());
                        bytes[position] ^= 1 << rng.below(8);
                    }
                }
            }

            if let Ok(tuple) = TupleRef::new(&layout, &bytes) {
                for column in 0..tuple.column_count() {
                    assert!(tuple.field(column).is_ok(), "column {column} of {bytes:?}");
                }
            }
        }
    }

    #[test]
    fn layout_with_the_maximum_number_of_columns_should_round_trip() {
        let columns = (0..TupleLayout::MAX_COLUMNS)
            .map(|column| if column % 2 == 0 { Fixed(1) } else { Variable })
            .collect();
        let layout = TupleLayout::new(columns).unwrap();
        let values: Vec<[u8; 1]> = (0..TupleLayout::MAX_COLUMNS)
            .map(|column| [column as u8])
            .collect();
        let input: Vec<Option<&[u8]>> = values
            .iter()
            .enumerate()
            .map(|(column, value)| (column % 3 != 0).then_some(&value[..]))
            .collect();

        let bytes = layout.encode(&input).unwrap();
        let tuple = TupleRef::new(&layout, &bytes).unwrap();

        assert_eq!(tuple.fields().collect::<Vec<_>>(), input);
    }

    #[test]
    fn errors_should_describe_every_variant() {
        let cases = [
            (
                TupleError::TooManyColumns {
                    count: 1601,
                    max: 1600,
                },
                "tuple has 1601 columns, maximum is 1600",
            ),
            (
                TupleError::ZeroWidthColumn { column: 1 },
                "fixed column 1 must be at least 1 byte wide",
            ),
            (
                TupleError::ColumnCountMismatch {
                    expected: 3,
                    actual: 2,
                },
                "expected 3 columns, got 2",
            ),
            (
                TupleError::TupleTooLarge {
                    size: 70000,
                    max: 65535,
                },
                "tuple of 70000 bytes exceeds maximum of 65535 bytes",
            ),
            (
                TupleError::ColumnOutOfRange {
                    column: 5,
                    count: 3,
                },
                "column 5 does not exist (tuple has 3)",
            ),
            (
                corrupted("tuple has trailing bytes"),
                "tuple is corrupted: tuple has trailing bytes",
            ),
        ];

        for (err, message) in cases {
            assert_eq!(err.to_string(), message);
            assert!(std::error::Error::source(&err).is_none());
        }
    }

    #[test]
    fn errors_should_describe_the_problem() {
        let mismatch = TupleError::FixedWidthMismatch {
            column: 2,
            expected: 4,
            actual: 3,
        };
        let serialization = TupleError::from(SerializationError::UnexpectedEof {
            needed: 2,
            remaining: 1,
        });

        assert_eq!(mismatch.to_string(), "column 2 must be 4 bytes wide, got 3");
        assert_eq!(
            serialization.to_string(),
            "tuple is corrupted: needed 2 bytes to read, only 1 remaining"
        );
        assert!(std::error::Error::source(&serialization).is_some());
        assert!(std::error::Error::source(&mismatch).is_none());
    }

    #[test]
    fn layout_filling_exactly_the_maximum_tuple_size_should_work() {
        let width = (MAX_TUPLE_SIZE - 3) as u16;

        let layout = TupleLayout::new(vec![Fixed(width)]).unwrap();

        assert_eq!(layout.min_tuple_size(), MAX_TUPLE_SIZE);
    }
}
