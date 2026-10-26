//! The text the CLI prints, as `psql` does: the rows of a query as an aligned table (numbers to
//! the right, everything else to the left, `NULL` empty, the header centered), the command tag
//! of every other statement, and an error with its SQLSTATE.

use crate::planner::StatementOutcome;
use crate::types::Value;

/// What a statement did, ready to print.
pub(super) fn outcome(outcome: &StatementOutcome) -> String {
    match outcome {
        StatementOutcome::CreateTable => "CREATE TABLE".to_string(),
        StatementOutcome::CreateIndex => "CREATE INDEX".to_string(),
        // The tag's legacy OID field, always 0, which clients still expect.
        StatementOutcome::Insert(count) => format!("INSERT 0 {count}"),
        StatementOutcome::Update(count) => format!("UPDATE {count}"),
        StatementOutcome::Delete(count) => format!("DELETE {count}"),
        StatementOutcome::Explain(plan) => plan.clone(),
        StatementOutcome::Rows { columns, rows } => table(columns, rows),
    }
}

/// The rows under a header of `columns`, then how many there are. Widths count characters,
/// not bytes, so accented text lines up.
pub(super) fn table(columns: &[String], rows: &[Vec<Value>]) -> String {
    let cells: Vec<Vec<Cell>> = rows
        .iter()
        .map(|row| row.iter().map(Cell::of).collect())
        .collect();
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let widest_value = cells
                .iter()
                .filter_map(|row| row.get(index))
                .map(Cell::width)
                .max()
                .unwrap_or(0);
            widest_value.max(name.chars().count())
        })
        .collect();

    let mut lines = Vec::with_capacity(rows.len() + 3);
    let header = columns
        .iter()
        .zip(&widths)
        .map(|(name, width)| centered(name, *width));
    lines.push(line(header));
    let rule: Vec<String> = widths.iter().map(|width| "-".repeat(width + 2)).collect();
    lines.push(rule.join("+"));
    for row in &cells {
        lines.push(line(
            row.iter()
                .zip(&widths)
                .map(|(cell, width)| cell.padded(*width)),
        ));
    }
    lines.push(match rows.len() {
        1 => "(1 row)".to_string(),
        count => format!("({count} rows)"),
    });
    lines.join("\n")
}

pub(super) fn error(message: &str, sqlstate: &str) -> String {
    format!("ERROR: {message} (SQLSTATE {sqlstate})")
}

/// The cells of a line, each with a space on either side, separated by `|`; the spaces
/// after the last one are dropped.
fn line(cells: impl Iterator<Item = String>) -> String {
    let cells: Vec<String> = cells.collect();
    format!(" {}", cells.join(" | ")).trim_end().to_string()
}

/// `text` in the middle of `width`, the odd space on the right.
fn centered(text: &str, width: usize) -> String {
    let free = width.saturating_sub(text.chars().count());
    let left = free / 2;
    format!("{}{text}{}", " ".repeat(left), " ".repeat(free - left))
}

/// A value as it is printed: its text, and whether it lines up to the right as a number does.
struct Cell {
    text: String,
    is_number: bool,
}

impl Cell {
    fn of(value: &Value) -> Self {
        let is_number = matches!(
            value,
            Value::SmallInt(_)
                | Value::Integer(_)
                | Value::BigInt(_)
                | Value::Real(_)
                | Value::Double(_)
        );
        let text = match value {
            Value::Null => String::new(),
            value => value.to_string(),
        };
        Self { text, is_number }
    }

    fn width(&self) -> usize {
        self.text.chars().count()
    }

    fn padded(&self, width: usize) -> String {
        let padding = " ".repeat(width.saturating_sub(self.width()));
        match self.is_number {
            true => format!("{padding}{}", self.text),
            false => format!("{}{padding}", self.text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::types::{DataType, Finite64};

    fn names(columns: &[&str]) -> Vec<String> {
        columns.iter().map(|name| name.to_string()).collect()
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_string())
    }

    #[test]
    fn a_table_should_align_numbers_right_and_the_rest_left() {
        let rows = vec![
            vec![Value::Integer(1), text("ana"), Value::Boolean(true)],
            vec![Value::Integer(250), Value::Null, Value::Boolean(false)],
        ];

        let printed = table(&names(&["id", "name", "active"]), &rows);

        assert_eq!(
            printed,
            " id  | name | active\n\
             -----+------+--------\n\
             \x20  1 | ana  | TRUE\n\
             \x20250 |      | FALSE\n\
             (2 rows)"
        );
    }

    #[test]
    fn a_table_should_center_the_header_over_wider_values() {
        let rows = vec![vec![
            text("lisboa"),
            Value::Double(Finite64::new(1.5).unwrap()),
            Value::parse(DataType::Date, "2024-01-31").unwrap(),
        ]];

        let printed = table(&names(&["c", "x", "day"]), &rows);

        assert_eq!(
            printed,
            "   c    |  x  |    day\n\
             --------+-----+------------\n\
             \x20lisboa | 1.5 | 2024-01-31\n\
             (1 row)"
        );
    }

    #[test]
    fn a_table_should_measure_text_in_characters() {
        let rows = vec![vec![text("João")], vec![text("ç")]];

        let printed = table(&names(&["nome"]), &rows);

        assert_eq!(printed, " nome\n------\n João\n ç\n(2 rows)");
    }

    #[test]
    fn a_table_without_rows_should_still_show_its_header() {
        let printed = table(&names(&["count", "max"]), &[]);

        assert_eq!(printed, " count | max\n-------+-----\n(0 rows)");
    }

    #[test]
    fn other_outcomes_should_be_command_tags_or_the_plan() {
        let cases = [
            (StatementOutcome::CreateTable, "CREATE TABLE"),
            (StatementOutcome::CreateIndex, "CREATE INDEX"),
            (StatementOutcome::Insert(2), "INSERT 0 2"),
            (StatementOutcome::Update(0), "UPDATE 0"),
            (StatementOutcome::Delete(1), "DELETE 1"),
            (
                StatementOutcome::Explain("Projection: 1\n  OneRow".to_string()),
                "Projection: 1\n  OneRow",
            ),
        ];

        for (statement_outcome, expected) in cases {
            assert_eq!(outcome(&statement_outcome), expected);
        }
        let rows = StatementOutcome::Rows {
            columns: names(&["one"]),
            rows: vec![vec![Value::Integer(1)]],
        };
        assert_eq!(outcome(&rows), " one\n-----\n   1\n(1 row)");
    }

    #[test]
    fn an_error_should_carry_its_sqlstate() {
        let printed = error("column \"x\" does not exist", "42703");

        assert_eq!(
            printed,
            "ERROR: column \"x\" does not exist (SQLSTATE 42703)"
        );
    }
}
