//! The scope of names: the tables an expression may read, each under the name it is visible by
//! (its alias, or else its own) with its columns at an offset of the row. A join puts the
//! right table's columns after the left's, as the join executors do.

use crate::table::{Column, Schema};

use super::PlannerError;

pub(super) struct Scope<'a> {
    tables: Vec<ScopeTable<'a>>,
}

struct ScopeTable<'a> {
    name: &'a str,
    columns: &'a [Column],
    offset: usize,
}

impl<'a> Scope<'a> {
    /// No columns: a row of `VALUES`, a `LIMIT`, a `SELECT` without `FROM`.
    pub(super) fn empty() -> Self {
        Self { tables: Vec::new() }
    }

    pub(super) fn table(name: &'a str, schema: &'a Schema) -> Self {
        Self {
            tables: vec![ScopeTable {
                name,
                columns: schema.columns(),
                offset: 0,
            }],
        }
    }

    /// The tables of `self`, then those of `right`, their columns after these.
    pub(super) fn join(mut self, right: Scope<'a>) -> Result<Self, PlannerError> {
        let offset = self.width();
        for table in right.tables {
            if self.tables.iter().any(|other| other.name == table.name) {
                return Err(PlannerError::DuplicateAlias {
                    table: table.name.to_string(),
                });
            }
            self.tables.push(ScopeTable {
                offset: offset + table.offset,
                ..table
            });
        }
        Ok(self)
    }

    /// How many values a row of these tables has.
    pub(super) fn width(&self) -> usize {
        self.tables
            .last()
            .map_or(0, |table| table.offset + table.columns.len())
    }

    /// The position of a column in the row. A qualified column is looked for in its table only;
    /// an unqualified one in every table, and it must be in exactly one.
    pub(super) fn column(&self, table: Option<&str>, name: &str) -> Result<usize, PlannerError> {
        let tables = match table {
            Some(qualifier) => std::slice::from_ref(self.named(qualifier)?),
            None => self.tables.as_slice(),
        };
        let mut matches = tables.iter().filter_map(|table| {
            let position = table
                .columns
                .iter()
                .position(|column| column.name == name)?;
            Some(table.offset + position)
        });
        match (matches.next(), matches.next()) {
            (Some(position), None) => Ok(position),
            (Some(_), Some(_)) => Err(PlannerError::AmbiguousColumn {
                column: name.to_string(),
            }),
            (None, _) => Err(PlannerError::UndefinedColumn {
                column: name.to_string(),
            }),
        }
    }

    /// The columns `*` (`table` is `None`) or `table.*` stand for, in the order of the tables:
    /// each position with the column's name.
    pub(super) fn wildcard(
        &self,
        table: Option<&str>,
    ) -> Result<Vec<(usize, &'a str)>, PlannerError> {
        let tables = match table {
            Some(qualifier) => std::slice::from_ref(self.named(qualifier)?),
            None if self.tables.is_empty() => return Err(PlannerError::WildcardWithoutTables),
            None => self.tables.as_slice(),
        };
        Ok(tables
            .iter()
            .flat_map(|table| {
                let columns: &'a [Column] = table.columns;
                let offset = table.offset;
                columns
                    .iter()
                    .enumerate()
                    .map(move |(position, column)| (offset + position, column.name.as_str()))
            })
            .collect())
    }

    /// The column at `position` as `table.column`, the way errors name it.
    pub(super) fn qualified_name(&self, position: usize) -> Option<String> {
        self.tables.iter().find_map(|table| {
            let column = table.columns.get(position.checked_sub(table.offset)?)?;
            Some(format!("{}.{}", table.name, column.name))
        })
    }

    fn named(&self, name: &str) -> Result<&ScopeTable<'a>, PlannerError> {
        self.tables
            .iter()
            .find(|table| table.name == name)
            .ok_or_else(|| PlannerError::UndefinedTableReference {
                table: name.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::types::DataType;

    fn users() -> Schema {
        Schema::new(vec![
            Column::new("id", DataType::Integer),
            Column::new("name", DataType::VARCHAR_MAX),
        ])
        .unwrap()
    }

    fn orders() -> Schema {
        Schema::new(vec![
            Column::new("id", DataType::Integer),
            Column::new("user_id", DataType::Integer),
            Column::new("total", DataType::BigInt),
        ])
        .unwrap()
    }

    #[test]
    fn a_table_should_resolve_its_columns_qualified_or_not() {
        let users = users();
        let scope = Scope::table("u", &users);

        let positions = [
            scope.column(None, "id").unwrap(),
            scope.column(Some("u"), "name").unwrap(),
        ];

        assert_eq!(positions, [0, 1]);
        assert_eq!(scope.width(), 2);
    }

    #[test]
    fn unknown_names_should_fail() {
        let users = users();
        let scope = Scope::table("u", &users);

        let column = scope.column(None, "email").unwrap_err();
        let qualified = scope.column(Some("u"), "email").unwrap_err();
        let table = scope.column(Some("users"), "id").unwrap_err();
        let in_empty = Scope::empty().column(None, "id").unwrap_err();

        assert!(matches!(&column, PlannerError::UndefinedColumn { column } if column == "email"));
        assert!(matches!(qualified, PlannerError::UndefinedColumn { .. }));
        // The alias hides the table's own name.
        assert!(
            matches!(&table, PlannerError::UndefinedTableReference { table } if table == "users")
        );
        assert!(matches!(in_empty, PlannerError::UndefinedColumn { .. }));
    }

    #[test]
    fn a_join_should_put_the_right_columns_after_the_left() {
        let (users, orders) = (users(), orders());

        let scope = Scope::table("u", &users)
            .join(Scope::table("o", &orders))
            .unwrap();

        assert_eq!(scope.width(), 5);
        assert_eq!(scope.column(None, "name").unwrap(), 1);
        assert_eq!(scope.column(None, "user_id").unwrap(), 3);
        assert_eq!(scope.column(Some("o"), "id").unwrap(), 2);
        assert_eq!(scope.column(Some("u"), "id").unwrap(), 0);
        assert_eq!(scope.qualified_name(4).as_deref(), Some("o.total"));
        assert_eq!(scope.qualified_name(5), None);
    }

    #[test]
    fn a_column_in_two_tables_should_be_ambiguous() {
        let (users, orders) = (users(), orders());
        let scope = Scope::table("u", &users)
            .join(Scope::table("o", &orders))
            .unwrap();

        let err = scope.column(None, "id").unwrap_err();

        assert!(matches!(&err, PlannerError::AmbiguousColumn { column } if column == "id"));
        assert_eq!(err.sqlstate(), "42702");
    }

    #[test]
    fn a_name_twice_in_one_from_should_fail() {
        let (users, orders) = (users(), orders());

        let err = Scope::table("t", &users)
            .join(Scope::table("o", &orders))
            .unwrap()
            .join(Scope::table("t", &orders))
            .err()
            .unwrap();

        assert!(matches!(&err, PlannerError::DuplicateAlias { table } if table == "t"));
        assert_eq!(err.sqlstate(), "42712");
    }

    #[test]
    fn a_wildcard_should_give_the_columns_of_every_table_or_of_one() {
        let (users, orders) = (users(), orders());
        let scope = Scope::table("u", &users)
            .join(Scope::table("o", &orders))
            .unwrap();

        let every = scope.wildcard(None).unwrap();
        let of_orders = scope.wildcard(Some("o")).unwrap();
        let unknown = scope.wildcard(Some("x")).unwrap_err();
        let without_tables = Scope::empty().wildcard(None).unwrap_err();

        assert_eq!(
            every,
            [
                (0, "id"),
                (1, "name"),
                (2, "id"),
                (3, "user_id"),
                (4, "total")
            ]
        );
        assert_eq!(of_orders, [(2, "id"), (3, "user_id"), (4, "total")]);
        assert!(matches!(
            unknown,
            PlannerError::UndefinedTableReference { .. }
        ));
        assert!(matches!(
            without_tables,
            PlannerError::WildcardWithoutTables
        ));
    }
}
