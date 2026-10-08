//! Schema of a table: ordered columns (name, type, nullability) and the `TupleLayout` they
//! map to. It turns a row of `Value`s into tuple bytes and back:
//!
//! - writing applies SQL store assignment: each value is cast to its column type (`22003`
//!   when it does not fit) and NULL is rejected in NOT NULL columns (`23502`);
//! - reading trusts nothing from disk: a corrupted tuple, a tuple of another schema or a NULL
//!   in a NOT NULL column is an error, never a panic.
//!
//! Column names are compared byte for byte; case folding and quoting belong to the parser.

use std::collections::HashSet;
use std::fmt;

use crate::storage::tuple::{TupleError, TupleLayout, TupleRef};
use crate::types::{DataType, TypeError, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

impl Column {
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable: true,
        }
    }

    pub fn not_null(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            nullable: false,
            ..Self::new(name, data_type)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SchemaError {
    NoColumns,
    TooManyColumns {
        count: usize,
        max: usize,
    },
    EmptyColumnName {
        column: usize,
    },
    ColumnNameTooLong {
        name: String,
        max: usize,
    },
    DuplicateColumn {
        name: String,
    },
    /// A row with more or fewer values than the table has columns.
    ColumnCountMismatch {
        expected: usize,
        actual: usize,
    },
    NullViolation {
        column: String,
    },
    ColumnOutOfRange {
        column: usize,
        count: usize,
    },
    /// A value that does not fit its column, or a stored field that does not decode.
    Type {
        column: String,
        source: TypeError,
    },
    Tuple(TupleError),
}

impl SchemaError {
    pub fn sqlstate(&self) -> &'static str {
        match self {
            SchemaError::NoColumns => "42P16",
            SchemaError::TooManyColumns { .. } => "54011",
            SchemaError::EmptyColumnName { .. } => "42602",
            SchemaError::ColumnNameTooLong { .. } => "42622",
            SchemaError::DuplicateColumn { .. } => "42701",
            SchemaError::ColumnCountMismatch { .. } => "42601",
            SchemaError::NullViolation { .. } => "23502",
            SchemaError::ColumnOutOfRange { .. } => "XX000",
            SchemaError::Type { source, .. } => source.sqlstate(),
            SchemaError::Tuple(_) => "XX001",
        }
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchemaError::NoColumns => write!(f, "a table must have at least one column"),
            SchemaError::TooManyColumns { count, max } => {
                write!(f, "tables can have at most {max} columns, got {count}")
            }
            SchemaError::EmptyColumnName { column } => {
                write!(f, "column {column} has an empty name")
            }
            SchemaError::ColumnNameTooLong { name, max } => write!(
                f,
                "column name \"{}…\" is longer than {max} bytes",
                prefix_within(name, *max)
            ),
            SchemaError::DuplicateColumn { name } => {
                write!(f, "column \"{name}\" specified more than once")
            }
            SchemaError::ColumnCountMismatch { expected, actual } => write!(
                f,
                "row has {actual} values but the table has {expected} columns"
            ),
            SchemaError::NullViolation { column } => write!(
                f,
                "null value in column \"{column}\" violates not-null constraint"
            ),
            SchemaError::ColumnOutOfRange { column, count } => {
                write!(f, "column {column} does not exist (table has {count})")
            }
            SchemaError::Type { column, source } => write!(f, "column \"{column}\": {source}"),
            SchemaError::Tuple(err) => write!(f, "stored row does not match the schema: {err}"),
        }
    }
}

impl std::error::Error for SchemaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SchemaError::Type { source, .. } => Some(source),
            SchemaError::Tuple(err) => Some(err),
            _ => None,
        }
    }
}

impl From<TupleError> for SchemaError {
    fn from(err: TupleError) -> Self {
        SchemaError::Tuple(err)
    }
}

/// Longest prefix of `text` within `max_bytes` that ends on a character boundary, so an
/// oversized name never turns into an oversized error message.
pub(crate) fn prefix_within(text: &str, max_bytes: usize) -> &str {
    let end = (0..=max_bytes.min(text.len()))
        .rev()
        .find(|&end| text.is_char_boundary(end))
        .unwrap_or(0);
    text.get(..end).unwrap_or_default()
}

/// Ordered, validated columns of a table and the tuple layout derived from them.
/// Immutable once built, so the layout always matches the columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    columns: Vec<Column>,
    layout: TupleLayout,
}

impl Schema {
    /// Leaves room for a one-byte length prefix when the catalog stores names.
    pub const MAX_COLUMN_NAME_LEN: usize = 63;

    pub fn new(columns: Vec<Column>) -> Result<Self, SchemaError> {
        check_column_count(columns.len())?;
        check_column_names(&columns)?;
        let layout = TupleLayout::new(columns.iter().map(|c| c.data_type.layout()).collect())?;
        Ok(Self { columns, layout })
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Exact byte comparison: case folding and quoting are the parser's job.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    pub fn layout(&self) -> &TupleLayout {
        &self.layout
    }

    /// Size of a row whose variable columns are all empty (today, every row).
    pub fn min_row_size(&self) -> usize {
        self.layout.min_tuple_size()
    }

    /// Store assignment: each value is cast to its column type and NULL is rejected in
    /// NOT NULL columns, all before the tuple is built.
    pub fn encode_row(&self, row: &[Value]) -> Result<Vec<u8>, SchemaError> {
        if row.len() != self.columns.len() {
            return Err(SchemaError::ColumnCountMismatch {
                expected: self.columns.len(),
                actual: row.len(),
            });
        }
        let fields = self
            .columns
            .iter()
            .zip(row)
            .map(|(column, value)| assign(column, value).map(|value| value.encode()))
            .collect::<Result<Vec<_>, _>>()?;
        let borrowed: Vec<Option<&[u8]>> = fields.iter().map(Option::as_deref).collect();
        Ok(self.layout.encode(&borrowed)?)
    }

    /// Reads every field with `TupleRef::field`, so a field the tuple cannot produce is an
    /// error instead of being taken for NULL.
    pub fn decode_row(&self, tuple: &[u8]) -> Result<Vec<Value>, SchemaError> {
        let tuple = TupleRef::new(&self.layout, tuple)?;
        self.columns
            .iter()
            .enumerate()
            .map(|(index, column)| decode_field(column, tuple.field(index)?))
            .collect()
    }

    pub fn decode_column(&self, tuple: &[u8], column: usize) -> Result<Value, SchemaError> {
        let definition = self.column(column)?;
        let tuple = TupleRef::new(&self.layout, tuple)?;
        decode_field(definition, tuple.field(column)?)
    }

    /// Several columns (in the requested order, repeats allowed) validating the tuple once;
    /// the indexes are checked before the tuple is read.
    pub fn decode_columns(
        &self,
        tuple: &[u8],
        columns: &[usize],
    ) -> Result<Vec<Value>, SchemaError> {
        let definitions = columns
            .iter()
            .map(|&index| self.column(index))
            .collect::<Result<Vec<_>, _>>()?;
        let tuple = TupleRef::new(&self.layout, tuple)?;
        columns
            .iter()
            .zip(definitions)
            .map(|(&index, column)| decode_field(column, tuple.field(index)?))
            .collect()
    }

    fn column(&self, index: usize) -> Result<&Column, SchemaError> {
        self.columns
            .get(index)
            .ok_or(SchemaError::ColumnOutOfRange {
                column: index,
                count: self.columns.len(),
            })
    }
}

/// Bytes come from disk: a NULL in a NOT NULL column means corruption or the wrong schema.
fn decode_field(column: &Column, field: Option<&[u8]>) -> Result<Value, SchemaError> {
    if field.is_none() && !column.nullable {
        return Err(SchemaError::NullViolation {
            column: column.name.clone(),
        });
    }
    Value::decode(column.data_type, field).map_err(|source| SchemaError::Type {
        column: column.name.clone(),
        source,
    })
}

fn assign(column: &Column, value: &Value) -> Result<Value, SchemaError> {
    if value.is_null() && !column.nullable {
        return Err(SchemaError::NullViolation {
            column: column.name.clone(),
        });
    }
    value
        .cast(column.data_type)
        .map_err(|source| SchemaError::Type {
            column: column.name.clone(),
            source,
        })
}

fn check_column_count(count: usize) -> Result<(), SchemaError> {
    if count == 0 {
        return Err(SchemaError::NoColumns);
    }
    if count > TupleLayout::MAX_COLUMNS {
        return Err(SchemaError::TooManyColumns {
            count,
            max: TupleLayout::MAX_COLUMNS,
        });
    }
    Ok(())
}

fn check_column_names(columns: &[Column]) -> Result<(), SchemaError> {
    let mut seen = HashSet::with_capacity(columns.len());
    for (index, column) in columns.iter().enumerate() {
        let name = column.name.as_str();
        if name.is_empty() {
            return Err(SchemaError::EmptyColumnName { column: index });
        }
        if name.len() > Schema::MAX_COLUMN_NAME_LEN {
            return Err(SchemaError::ColumnNameTooLong {
                name: name.to_string(),
                max: Schema::MAX_COLUMN_NAME_LEN,
            });
        }
        if !seen.insert(name) {
            return Err(SchemaError::DuplicateColumn {
                name: name.to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tuple::ColumnLayout;
    use DataType::{BigInt, Integer, SmallInt};

    fn people() -> Schema {
        Schema::new(vec![
            Column::not_null("id", Integer),
            Column::new("age", SmallInt),
            Column::new("balance", BigInt),
        ])
        .unwrap()
    }

    fn numbered_columns(count: usize) -> Vec<Column> {
        (0..count)
            .map(|i| Column::new(format!("c{i}"), SmallInt))
            .collect()
    }

    #[test]
    fn new_with_one_column_should_build_its_layout() {
        let schema = Schema::new(vec![Column::new("id", Integer)]).unwrap();

        assert_eq!(schema.columns(), [Column::new("id", Integer)]);
        assert_eq!(schema.layout().columns(), [ColumnLayout::Fixed(4)]);
    }

    #[test]
    fn new_should_map_each_column_type_to_its_layout() {
        let schema = people();

        assert_eq!(
            schema.layout().columns(),
            [
                ColumnLayout::Fixed(4),
                ColumnLayout::Fixed(2),
                ColumnLayout::Fixed(8)
            ]
        );
    }

    #[test]
    fn new_without_columns_should_fail() {
        assert_eq!(Schema::new(vec![]).unwrap_err(), SchemaError::NoColumns);
    }

    #[test]
    fn new_should_accept_the_maximum_number_of_columns() {
        let schema = Schema::new(numbered_columns(TupleLayout::MAX_COLUMNS)).unwrap();

        assert_eq!(schema.columns().len(), TupleLayout::MAX_COLUMNS);
    }

    #[test]
    fn new_with_too_many_columns_should_fail() {
        let err = Schema::new(numbered_columns(TupleLayout::MAX_COLUMNS + 1)).unwrap_err();

        assert_eq!(
            err,
            SchemaError::TooManyColumns {
                count: TupleLayout::MAX_COLUMNS + 1,
                max: TupleLayout::MAX_COLUMNS
            }
        );
    }

    #[test]
    fn new_with_an_empty_name_should_fail() {
        let err =
            Schema::new(vec![Column::new("id", Integer), Column::new("", Integer)]).unwrap_err();

        assert_eq!(err, SchemaError::EmptyColumnName { column: 1 });
    }

    #[test]
    fn name_at_the_length_limit_should_be_accepted() {
        let ascii = "a".repeat(Schema::MAX_COLUMN_NAME_LEN);
        let multibyte = format!("{}a", "é".repeat(31));

        let schema = Schema::new(vec![
            Column::new(ascii, Integer),
            Column::new(multibyte, Integer),
        ]);

        assert!(schema.is_ok());
    }

    #[test]
    fn name_past_the_length_limit_should_fail() {
        for name in ["a".repeat(Schema::MAX_COLUMN_NAME_LEN + 1), "é".repeat(32)] {
            let err = Schema::new(vec![Column::new(name.clone(), Integer)]).unwrap_err();

            assert_eq!(
                err,
                SchemaError::ColumnNameTooLong {
                    name,
                    max: Schema::MAX_COLUMN_NAME_LEN
                }
            );
        }
    }

    #[test]
    fn duplicate_names_should_fail() {
        let err = Schema::new(vec![
            Column::new("id", Integer),
            Column::new("age", SmallInt),
            Column::new("id", BigInt),
        ])
        .unwrap_err();

        assert_eq!(
            err,
            SchemaError::DuplicateColumn {
                name: "id".to_string()
            }
        );
    }

    #[test]
    fn names_differing_only_in_case_should_be_distinct() {
        let schema = Schema::new(vec![Column::new("id", Integer), Column::new("ID", Integer)]);

        assert!(schema.is_ok());
    }

    #[test]
    fn column_index_should_find_columns_by_exact_name() {
        let schema = people();

        assert_eq!(schema.column_index("id"), Some(0));
        assert_eq!(schema.column_index("balance"), Some(2));
        assert_eq!(schema.column_index("ID"), None);
        assert_eq!(schema.column_index("missing"), None);
    }

    #[test]
    fn min_row_size_should_be_the_tuple_size() {
        let schema = people();

        assert_eq!(schema.min_row_size(), 2 + 1 + 4 + 2 + 8);
        assert_eq!(schema.min_row_size(), schema.layout().min_tuple_size());
    }

    #[test]
    fn column_new_should_accept_null() {
        let column = Column::new("age", DataType::SmallInt);

        assert_eq!(
            column,
            Column {
                name: "age".to_string(),
                data_type: DataType::SmallInt,
                nullable: true
            }
        );
    }

    #[test]
    fn column_not_null_should_reject_null() {
        let column = Column::not_null("id", DataType::Integer);

        assert_eq!(column.name, "id");
        assert_eq!(column.data_type, DataType::Integer);
        assert!(!column.nullable);
    }

    fn every_error() -> Vec<(SchemaError, &'static str, &'static str)> {
        vec![
            (
                SchemaError::NoColumns,
                "a table must have at least one column",
                "42P16",
            ),
            (
                SchemaError::TooManyColumns {
                    count: 1601,
                    max: 1600,
                },
                "tables can have at most 1600 columns, got 1601",
                "54011",
            ),
            (
                SchemaError::EmptyColumnName { column: 2 },
                "column 2 has an empty name",
                "42602",
            ),
            (
                SchemaError::ColumnNameTooLong {
                    name: "long".to_string(),
                    max: 3,
                },
                "column name \"lon…\" is longer than 3 bytes",
                "42622",
            ),
            (
                SchemaError::DuplicateColumn {
                    name: "id".to_string(),
                },
                "column \"id\" specified more than once",
                "42701",
            ),
            (
                SchemaError::ColumnCountMismatch {
                    expected: 3,
                    actual: 2,
                },
                "row has 2 values but the table has 3 columns",
                "42601",
            ),
            (
                SchemaError::NullViolation {
                    column: "id".to_string(),
                },
                "null value in column \"id\" violates not-null constraint",
                "23502",
            ),
            (
                SchemaError::ColumnOutOfRange {
                    column: 5,
                    count: 3,
                },
                "column 5 does not exist (table has 3)",
                "XX000",
            ),
            (
                SchemaError::Type {
                    column: "age".to_string(),
                    source: TypeError::OutOfRange { ty: SmallInt },
                },
                "column \"age\": smallint out of range",
                "22003",
            ),
            (
                SchemaError::Tuple(TupleError::ColumnCountMismatch {
                    expected: 3,
                    actual: 2,
                }),
                "stored row does not match the schema: expected 3 columns, got 2",
                "XX001",
            ),
        ]
    }

    #[test]
    fn errors_should_describe_the_problem() {
        for (err, message, _) in every_error() {
            assert_eq!(err.to_string(), message);
        }
    }

    #[test]
    fn errors_should_map_to_sql_standard_sqlstates() {
        for (err, _, sqlstate) in every_error() {
            assert_eq!(err.sqlstate(), sqlstate, "{err:?}");
        }
    }

    #[test]
    fn wrapped_errors_should_expose_their_source() {
        for (err, _, _) in every_error() {
            let has_source = matches!(err, SchemaError::Type { .. } | SchemaError::Tuple(_));

            assert_eq!(
                std::error::Error::source(&err).is_some(),
                has_source,
                "{err:?}"
            );
        }
    }

    #[test]
    fn name_too_long_message_should_show_only_the_allowed_prefix() {
        let ascii = SchemaError::ColumnNameTooLong {
            name: "a".repeat(1_000),
            max: 63,
        };
        let multibyte = SchemaError::ColumnNameTooLong {
            name: "é".repeat(40),
            max: 63,
        };

        assert_eq!(
            ascii.to_string(),
            format!(
                "column name \"{}…\" is longer than 63 bytes",
                "a".repeat(63)
            )
        );
        assert_eq!(
            multibyte.to_string(),
            format!(
                "column name \"{}…\" is longer than 63 bytes",
                "é".repeat(31)
            )
        );
    }

    /// What `TupleLayout::encode` produces for these exact values, bypassing the schema.
    fn raw_tuple(schema: &Schema, row: &[Value]) -> Vec<u8> {
        let fields: Vec<Option<Vec<u8>>> = row.iter().map(Value::encode).collect();
        let borrowed: Vec<Option<&[u8]>> = fields.iter().map(Option::as_deref).collect();
        schema.layout().encode(&borrowed).unwrap()
    }

    fn type_error(column: &str, source: TypeError) -> SchemaError {
        SchemaError::Type {
            column: column.to_string(),
            source,
        }
    }

    #[test]
    fn encode_row_should_produce_the_tuple_of_its_values() {
        let schema = people();
        let row = [Value::Integer(7), Value::SmallInt(-2), Value::BigInt(1)];

        let tuple = schema.encode_row(&row).unwrap();

        assert_eq!(tuple, raw_tuple(&schema, &row));
    }

    #[test]
    fn encode_row_should_cast_values_to_the_column_type() {
        let schema = people();

        let tuple = schema
            .encode_row(&[Value::SmallInt(7), Value::BigInt(30), Value::Integer(-5)])
            .unwrap();

        assert_eq!(
            tuple,
            raw_tuple(
                &schema,
                &[Value::Integer(7), Value::SmallInt(30), Value::BigInt(-5)]
            )
        );
    }

    #[test]
    fn encode_row_with_a_value_that_does_not_fit_should_fail() {
        let schema = people();

        let err = schema
            .encode_row(&[Value::Integer(1), Value::Integer(70_000), Value::Null])
            .unwrap_err();

        assert_eq!(
            err,
            type_error("age", TypeError::OutOfRange { ty: SmallInt })
        );
        assert_eq!(err.sqlstate(), "22003");
    }

    #[test]
    fn encode_row_should_accept_null_in_nullable_columns() {
        let schema = people();
        let row = [Value::Integer(1), Value::Null, Value::Null];

        let tuple = schema.encode_row(&row).unwrap();

        assert_eq!(tuple, raw_tuple(&schema, &row));
    }

    #[test]
    fn encode_row_with_null_in_a_not_null_column_should_fail() {
        let schema = people();

        let err = schema
            .encode_row(&[Value::Null, Value::SmallInt(1), Value::BigInt(1)])
            .unwrap_err();

        assert_eq!(
            err,
            SchemaError::NullViolation {
                column: "id".to_string()
            }
        );
    }

    #[test]
    fn encode_row_with_the_wrong_number_of_values_should_fail() {
        let schema = people();
        let four = [Value::Integer(1), Value::Null, Value::Null, Value::Null];

        for row in [&four[..2], &four[..]] {
            let err = schema.encode_row(row).unwrap_err();

            assert_eq!(
                err,
                SchemaError::ColumnCountMismatch {
                    expected: 3,
                    actual: row.len()
                }
            );
        }
    }

    #[test]
    fn encode_row_should_report_the_first_invalid_column() {
        let schema = people();

        let err = schema
            .encode_row(&[Value::Null, Value::Integer(70_000), Value::Null])
            .unwrap_err();

        assert!(matches!(err, SchemaError::NullViolation { column } if column == "id"));
    }

    #[test]
    fn decode_row_should_round_trip_limits_and_nulls() {
        let schema = people();
        let rows = [
            [
                Value::Integer(i32::MIN),
                Value::SmallInt(i16::MIN),
                Value::BigInt(i64::MIN),
            ],
            [
                Value::Integer(i32::MAX),
                Value::SmallInt(i16::MAX),
                Value::BigInt(i64::MAX),
            ],
            [Value::Integer(0), Value::Null, Value::Null],
        ];

        for row in rows {
            let tuple = schema.encode_row(&row).unwrap();

            assert_eq!(schema.decode_row(&tuple).unwrap(), row);
        }
    }

    #[test]
    fn decode_row_should_return_values_in_the_column_types() {
        let schema = people();
        let tuple = schema
            .encode_row(&[Value::SmallInt(7), Value::BigInt(30), Value::Integer(-5)])
            .unwrap();

        let row = schema.decode_row(&tuple).unwrap();

        assert_eq!(
            row,
            [Value::Integer(7), Value::SmallInt(30), Value::BigInt(-5)]
        );
    }

    #[test]
    fn decode_column_should_read_a_single_column() {
        let schema = people();
        let row = [Value::Integer(7), Value::Null, Value::BigInt(-9)];
        let tuple = schema.encode_row(&row).unwrap();

        for (index, expected) in row.iter().enumerate() {
            assert_eq!(schema.decode_column(&tuple, index).unwrap(), *expected);
        }
    }

    #[test]
    fn decode_column_out_of_range_should_fail() {
        let schema = people();
        let tuple = schema
            .encode_row(&[Value::Integer(1), Value::Null, Value::Null])
            .unwrap();

        for column in [3, usize::MAX] {
            let err = schema.decode_column(&tuple, column).unwrap_err();

            assert_eq!(err, SchemaError::ColumnOutOfRange { column, count: 3 });
        }
    }

    #[test]
    fn decode_of_a_truncated_tuple_should_fail() {
        let schema = people();
        let tuple = schema
            .encode_row(&[Value::Integer(1), Value::Null, Value::Null])
            .unwrap();

        let row_err = schema.decode_row(&tuple[..5]).unwrap_err();
        let column_err = schema.decode_column(&tuple[..1], 0).unwrap_err();

        assert!(matches!(row_err, SchemaError::Tuple(_)));
        assert!(matches!(column_err, SchemaError::Tuple(_)));
    }

    #[test]
    fn decode_of_another_schemas_tuple_should_fail() {
        let schema = people();
        let other =
            Schema::new(vec![Column::new("a", Integer), Column::new("b", Integer)]).unwrap();
        let tuple = other
            .encode_row(&[Value::Integer(1), Value::Integer(2)])
            .unwrap();

        let err = schema.decode_row(&tuple).unwrap_err();

        assert_eq!(
            err,
            SchemaError::Tuple(TupleError::ColumnCountMismatch {
                expected: 3,
                actual: 2
            })
        );
    }

    #[test]
    fn decode_of_null_stored_in_a_not_null_column_should_fail() {
        let schema = people();
        let tuple = raw_tuple(&schema, &[Value::Null, Value::SmallInt(1), Value::Null]);
        let expected = SchemaError::NullViolation {
            column: "id".to_string(),
        };

        assert_eq!(schema.decode_row(&tuple).unwrap_err(), expected);
        assert_eq!(schema.decode_column(&tuple, 0).unwrap_err(), expected);
        assert_eq!(schema.decode_column(&tuple, 1).unwrap(), Value::SmallInt(1));
        assert_eq!(
            schema.decode_columns(&tuple, &[1, 0]).unwrap_err(),
            expected
        );
    }

    #[test]
    fn decode_columns_should_read_the_requested_columns_in_order() {
        let schema = people();
        let tuple = schema
            .encode_row(&[Value::Integer(7), Value::Null, Value::BigInt(-9)])
            .unwrap();

        let values = schema.decode_columns(&tuple, &[2, 0, 2, 1]).unwrap();

        assert_eq!(
            values,
            [
                Value::BigInt(-9),
                Value::Integer(7),
                Value::BigInt(-9),
                Value::Null
            ]
        );
        assert!(schema.decode_columns(&tuple, &[]).unwrap().is_empty());
    }

    #[test]
    fn decode_columns_out_of_range_should_fail_before_reading_the_tuple() {
        let schema = people();
        let truncated = [3, 0];

        let err = schema.decode_columns(&truncated, &[0, 3]).unwrap_err();

        assert_eq!(
            err,
            SchemaError::ColumnOutOfRange {
                column: 3,
                count: 3
            }
        );
    }

    #[test]
    fn decode_columns_of_a_corrupted_tuple_should_fail() {
        let schema = people();

        let err = schema.decode_columns(&[3, 0], &[0]).unwrap_err();

        assert!(matches!(err, SchemaError::Tuple(_)));
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
    }

    const ALL_TYPES: [DataType; 3] = [SmallInt, Integer, BigInt];

    fn random_schema(rng: &mut Rng) -> Schema {
        let columns = (0..1 + rng.below(20))
            .map(|i| Column {
                name: format!("c{i}"),
                data_type: ALL_TYPES[rng.below(3)],
                nullable: rng.below(2) == 0,
            })
            .collect();
        Schema::new(columns).unwrap()
    }

    /// Any width, biased toward the limits where narrowing fails; about 15% NULL.
    fn random_value(rng: &mut Rng) -> Value {
        if rng.below(7) == 0 {
            return Value::Null;
        }
        let raw = match rng.below(3) {
            0 => [
                i64::MIN,
                -70_000,
                -32_769,
                -1,
                0,
                1,
                32_768,
                70_000,
                i64::MAX,
            ][rng.below(9)],
            1 => rng.below(201) as i64 - 100,
            _ => rng.next() as i64,
        };
        match rng.below(3) {
            0 => Value::SmallInt(raw as i16),
            1 => Value::Integer(raw as i32),
            _ => Value::BigInt(raw),
        }
    }

    fn as_i128(value: &Value) -> Option<i128> {
        match *value {
            Value::Null => None,
            Value::SmallInt(v) => Some(v.into()),
            Value::Integer(v) => Some(v.into()),
            Value::BigInt(v) => Some(v.into()),
        }
    }

    /// The assignment rules computed independently of `Value::cast`.
    fn expected_assignment(schema: &Schema, row: &[Value]) -> Result<Vec<Value>, SchemaError> {
        schema
            .columns()
            .iter()
            .zip(row)
            .map(|(column, value)| {
                let Some(number) = as_i128(value) else {
                    return if column.nullable {
                        Ok(Value::Null)
                    } else {
                        Err(SchemaError::NullViolation {
                            column: column.name.clone(),
                        })
                    };
                };
                let fitted = match column.data_type {
                    SmallInt => i16::try_from(number).ok().map(Value::SmallInt),
                    Integer => i32::try_from(number).ok().map(Value::Integer),
                    BigInt => i64::try_from(number).ok().map(Value::BigInt),
                };
                fitted.ok_or_else(|| {
                    type_error(
                        &column.name,
                        TypeError::OutOfRange {
                            ty: column.data_type,
                        },
                    )
                })
            })
            .collect()
    }

    #[test]
    fn random_rows_should_follow_the_assignment_rules() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

        for _ in 0..2_000 {
            let schema = random_schema(&mut rng);
            let row: Vec<Value> = schema
                .columns()
                .iter()
                .map(|_| random_value(&mut rng))
                .collect();

            let encoded = schema.encode_row(&row);

            match expected_assignment(&schema, &row) {
                Ok(expected) => {
                    let tuple = encoded.unwrap();
                    assert_eq!(schema.decode_row(&tuple).unwrap(), expected);
                    for (index, value) in expected.iter().enumerate() {
                        assert_eq!(schema.decode_column(&tuple, index).unwrap(), *value);
                    }
                    let reversed: Vec<usize> = (0..expected.len()).rev().collect();
                    let mut expected_reversed = expected.clone();
                    expected_reversed.reverse();
                    assert_eq!(
                        schema.decode_columns(&tuple, &reversed).unwrap(),
                        expected_reversed
                    );
                }
                Err(expected) => assert_eq!(encoded.unwrap_err(), expected, "{row:?}"),
            }
        }
    }

    fn random_valid_row(rng: &mut Rng, schema: &Schema) -> Vec<Value> {
        schema
            .columns()
            .iter()
            .map(|column| match (rng.below(4), column.nullable) {
                (0, true) => Value::Null,
                _ => Value::BigInt(rng.below(100) as i64)
                    .cast(column.data_type)
                    .unwrap(),
            })
            .collect()
    }

    #[test]
    fn random_corruption_should_never_panic() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);

        for _ in 0..2_000 {
            let schema = random_schema(&mut rng);
            let mut tuple = schema
                .encode_row(&random_valid_row(&mut rng, &schema))
                .unwrap();
            if rng.below(3) == 0 {
                tuple.truncate(rng.below(tuple.len() + 1));
            } else {
                for _ in 0..1 + rng.below(4) {
                    let position = rng.below(tuple.len());
                    tuple[position] ^= 1 << rng.below(8);
                }
            }

            let _ = schema.decode_row(&tuple);
            for column in 0..schema.columns().len() + 1 {
                let _ = schema.decode_column(&tuple, column);
            }
        }
    }
}
