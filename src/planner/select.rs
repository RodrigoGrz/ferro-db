//! Planning a `SELECT`: its clauses in the standard's order of evaluation, each over the rows of
//! the one before, as a `LogicalPlan`: `FROM` (scans and joins), `WHERE`, `GROUP BY` and the
//! aggregates, `HAVING`, `ORDER BY`, `LIMIT`/`OFFSET`, then the select list.
//!
//! A query is grouped when it has `GROUP BY`, `HAVING` or an aggregate: then everything after
//! `WHERE` reads the grouped row, and without `GROUP BY` the whole input is one group.
//!
//! The `Sort` reads the rows before the projection, so `ORDER BY` reaches columns outside the
//! select list; an output column named by position or name is sorted by its expression,
//! evaluated again (there are no volatile functions). The `Limit` sits right above the `Sort`.

use crate::catalog::Catalog;
use crate::execution::{Expr, JoinType, NullOrder, SortKey, SortOrder};
use crate::sql::ast;

use super::PlannerError;
use super::binder::{
    Grouping, bind, bind_grouped, contains_aggregate, contains_column, grouped_column,
};
use super::logical_plan::{LogicalPlan, OutputColumn};
use super::scope::Scope;

/// The logical plan of `select`, its names resolved against `catalog`, which it only reads.
pub fn plan_select(select: &ast::Select, catalog: &Catalog) -> Result<LogicalPlan, PlannerError> {
    let (mut plan, scope) = match &select.from {
        Some(from) => table_ref(from, catalog)?,
        None => (LogicalPlan::OneRow, Scope::empty()),
    };
    if let Some(selection) = &select.selection {
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: bind(selection, &scope, "WHERE")?,
        };
    }
    let mut grouping = match is_grouped(select) {
        true => Some(Grouping {
            group_by: select
                .group_by
                .iter()
                .map(|expr| group_expr(expr, &scope))
                .collect::<Result<_, _>>()?,
            aggregates: Vec::new(),
        }),
        false => None,
    };
    // Every clause above the groups is bound before the `Aggregate`, which gets the aggregates
    // they call.
    let columns = output_columns(&select.projection, &scope, grouping.as_mut())?;
    let having = select
        .having
        .as_ref()
        .map(|having| bind_over(having, &scope, grouping.as_mut(), "HAVING"))
        .transpose()?;
    let keys = select
        .order_by
        .iter()
        .map(|item| sort_key(item, &columns, &scope, grouping.as_mut()))
        .collect::<Result<Vec<_>, _>>()?;
    let count = select
        .limit
        .as_ref()
        .map(|count| row_count(count, "LIMIT"))
        .transpose()?;
    let offset = select
        .offset
        .as_ref()
        .map(|offset| row_count(offset, "OFFSET"))
        .transpose()?;

    if let Some(grouping) = grouping {
        plan = LogicalPlan::Aggregate {
            input: Box::new(plan),
            group_by: grouping.group_by,
            aggregates: grouping.aggregates,
        };
    }
    if let Some(having) = having {
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: having,
        };
    }
    if !keys.is_empty() {
        plan = LogicalPlan::Sort {
            input: Box::new(plan),
            keys,
        };
    }
    if count.is_some() || offset.is_some() {
        plan = LogicalPlan::Limit {
            input: Box::new(plan),
            count,
            offset,
        };
    }
    Ok(LogicalPlan::Project {
        input: Box::new(plan),
        columns,
    })
}

fn is_grouped(select: &ast::Select) -> bool {
    let in_list = select.projection.iter().any(|item| match item {
        ast::SelectItem::Expr { expr, .. } => contains_aggregate(expr),
        ast::SelectItem::Wildcard | ast::SelectItem::QualifiedWildcard(_) => false,
    });
    let in_order_by = select
        .order_by
        .iter()
        .any(|item| contains_aggregate(&item.expr));
    !select.group_by.is_empty() || select.having.is_some() || in_list || in_order_by
}

/// An expression of `GROUP BY`, over the input. An integer alone is refused rather than read as
/// a constant: it would group everything into one group where a position was meant.
fn group_expr(expr: &ast::Expr, scope: &Scope<'_>) -> Result<Expr, PlannerError> {
    if position(expr).is_some() {
        return Err(PlannerError::Unsupported {
            feature: "GROUP BY position",
        });
    }
    bind(expr, scope, "GROUP BY")
}

/// The expression over the grouped row when there is `grouping`, else over the input.
fn bind_over(
    expr: &ast::Expr,
    scope: &Scope<'_>,
    grouping: Option<&mut Grouping>,
    clause: &'static str,
) -> Result<Expr, PlannerError> {
    match grouping {
        Some(grouping) => bind_grouped(expr, scope, grouping),
        None => bind(expr, scope, clause),
    }
}

/// An item of `ORDER BY`: an output column by position or by name (the output name wins over a
/// column of the input), else an expression over the rows before the projection. Without
/// `NULLS FIRST` or `LAST`, a NULL sorts as the greatest value: last ascending, first
/// descending.
fn sort_key(
    item: &ast::OrderItem,
    columns: &[OutputColumn],
    scope: &Scope<'_>,
    grouping: Option<&mut Grouping>,
) -> Result<SortKey, PlannerError> {
    let expr = match output_column(&item.expr, columns)? {
        Some(column) => column.expr.clone(),
        None => bind_over(&item.expr, scope, grouping, "ORDER BY")?,
    };
    let order = match item.is_descending {
        true => SortOrder::Desc,
        false => SortOrder::Asc,
    };
    let nulls = match item.nulls_first.unwrap_or(item.is_descending) {
        true => NullOrder::First,
        false => NullOrder::Last,
    };
    Ok(SortKey { expr, order, nulls })
}

/// The output column an item of `ORDER BY` names by position or by its output name alone, if it
/// names one. Several output columns of that name are ambiguous only if they differ: `SELECT
/// *, id … ORDER BY id` sorts by the one column both are.
fn output_column<'c>(
    expr: &ast::Expr,
    columns: &'c [OutputColumn],
) -> Result<Option<&'c OutputColumn>, PlannerError> {
    if let Some(digits) = position(expr) {
        let column = digits
            .parse::<usize>()
            .ok()
            .and_then(|position| position.checked_sub(1))
            .and_then(|index| columns.get(index));
        return match column {
            Some(column) => Ok(Some(column)),
            None => Err(PlannerError::InvalidPosition {
                position: digits.to_string(),
            }),
        };
    }
    let ast::Expr::Column { table: None, name } = expr else {
        return Ok(None);
    };
    let mut named = columns.iter().filter(|column| column.name == *name);
    let Some(first) = named.next() else {
        return Ok(None);
    };
    match named.any(|other| other.expr != first.expr) {
        true => Err(PlannerError::AmbiguousOrderBy { name: name.clone() }),
        false => Ok(Some(first)),
    }
}

/// The digits of an unsigned integer written alone, without its leading zeros: a position in
/// the select list for `ORDER BY` (`GROUP BY` refuses one).
fn position(expr: &ast::Expr) -> Option<&str> {
    match expr {
        ast::Expr::Literal(ast::Literal::Number(text))
            if text.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            let digits = text.trim_start_matches('0');
            Some(if digits.is_empty() { "0" } else { digits })
        }
        _ => None,
    }
}

/// The count of `LIMIT` or `OFFSET`, computed once before the first row: it reads no column and
/// no aggregate.
fn row_count(expr: &ast::Expr, clause: &'static str) -> Result<Expr, PlannerError> {
    if contains_aggregate(expr) {
        return Err(PlannerError::AggregateNotAllowed { clause });
    }
    if contains_column(expr) {
        return Err(PlannerError::ColumnInRowCount { clause });
    }
    bind(expr, &Scope::empty(), clause)
}

/// A table or a join of them, with the names its rows are read by. The `ON` of a join sees only
/// the tables of that join (ISO/IEC 9075, 7.7): those after it are not there yet.
fn table_ref<'a>(
    table: &'a ast::TableRef,
    catalog: &'a Catalog,
) -> Result<(LogicalPlan, Scope<'a>), PlannerError> {
    match table {
        ast::TableRef::Table { name, alias } => {
            let info = catalog.table(name)?;
            let plan = LogicalPlan::Scan {
                table: info.name.clone(),
                alias: alias.clone(),
                columns: info
                    .schema
                    .columns()
                    .iter()
                    .map(|column| column.name.clone())
                    .collect(),
            };
            let visible_name = alias.as_deref().unwrap_or(&info.name);
            Ok((plan, Scope::table(visible_name, &info.schema)))
        }
        ast::TableRef::Join {
            left,
            right,
            kind,
            on,
        } => {
            let (left, left_scope) = table_ref(left, catalog)?;
            let (right, right_scope) = table_ref(right, catalog)?;
            let scope = left_scope.join(right_scope)?;
            let on = on
                .as_ref()
                .map(|on| bind(on, &scope, "JOIN conditions"))
                .transpose()?;
            let join_type = match kind {
                ast::JoinKind::Inner | ast::JoinKind::Cross => JoinType::Inner,
                ast::JoinKind::Left => JoinType::Left,
                ast::JoinKind::Right => JoinType::Right,
                ast::JoinKind::Full => JoinType::Full,
            };
            let plan = LogicalPlan::Join {
                left: Box::new(left),
                right: Box::new(right),
                join_type,
                on,
            };
            Ok((plan, scope))
        }
    }
}

/// The select list, `*` and `t.*` expanded in the order of the tables.
/// The select list over the input row, or over the grouped row when `grouping` is given.
fn output_columns(
    items: &[ast::SelectItem],
    scope: &Scope<'_>,
    mut grouping: Option<&mut Grouping>,
) -> Result<Vec<OutputColumn>, PlannerError> {
    let mut columns = Vec::with_capacity(items.len());
    for item in items {
        let table = match item {
            ast::SelectItem::Expr { expr, alias } => {
                let bound = bind_over(expr, scope, grouping.as_deref_mut(), "SELECT")?;
                columns.push(OutputColumn {
                    expr: bound,
                    name: alias.clone().unwrap_or_else(|| default_name(expr)),
                });
                continue;
            }
            ast::SelectItem::Wildcard => None,
            ast::SelectItem::QualifiedWildcard(table) => Some(table.as_str()),
        };
        for (position, name) in scope.wildcard(table)? {
            let expr = match grouping.as_deref() {
                Some(grouping) => grouped_column(position, scope, grouping)?,
                None => Expr::Column(position),
            };
            columns.push(OutputColumn {
                expr,
                name: name.to_string(),
            });
        }
    }
    Ok(columns)
}

/// The name of an output column without an alias: the column's or the function's, else the
/// standard leaves it to the implementation and it is `?column?`.
fn default_name(expr: &ast::Expr) -> String {
    match expr {
        ast::Expr::Column { name, .. } | ast::Expr::Function { name, .. } => name.clone(),
        _ => "?column?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::execution::Aggregate;
    use crate::execution::test_support::TestDb;
    use crate::planner::run_sql;
    use crate::sql::{Statement, parse};

    /// `users (id, name, email)`, `orders (id, user_id, total)` and `items (order_id, qty)`.
    fn db() -> TestDb {
        let mut db = TestDb::new(8);
        run_sql(
            "CREATE TABLE users (id INTEGER NOT NULL, name VARCHAR(20), email VARCHAR(40)); \
             CREATE TABLE orders (id INTEGER, user_id INTEGER, total BIGINT); \
             CREATE TABLE items (order_id INTEGER, qty INTEGER)",
            &mut db.ctx(),
        )
        .unwrap();
        db
    }

    fn select(sql: &str) -> ast::Select {
        match parse(sql).unwrap().as_slice() {
            [Statement::Select(select)] => (**select).clone(),
            other => panic!("{other:?}"),
        }
    }

    fn explain(db: &TestDb, sql: &str) -> String {
        plan_select(&select(sql), &db.catalog)
            .unwrap_or_else(|err| panic!("{err}: {sql}"))
            .to_string()
    }

    fn plan_err(db: &TestDb, sql: &str) -> PlannerError {
        match plan_select(&select(sql), &db.catalog) {
            Ok(plan) => panic!("planned {sql}:\n{plan}"),
            Err(err) => err,
        }
    }

    #[test]
    fn a_select_should_project_its_items_over_a_scan() {
        let db = db();

        let text = explain(&db, "SELECT id, name AS who, id + 1 FROM users");

        assert_eq!(
            text,
            "Project: users.id AS id, users.name AS who, users.id + 1 AS \"?column?\"\n\
             \x20 Scan: users"
        );
    }

    #[test]
    fn output_columns_should_be_named_after_their_column_or_function() {
        let db = db();

        let text = explain(
            &db,
            "SELECT u.email, CAST(id AS BIGINT), 'x', u.id AS \"Id\" FROM users u",
        );

        assert_eq!(
            text.lines().next().unwrap(),
            "Project: u.email AS email, CAST(u.id AS BIGINT) AS \"?column?\", 'x' AS \"?column?\", \
             u.id AS \"Id\""
        );
    }

    #[test]
    fn wildcards_should_expand_in_the_order_of_the_tables() {
        let db = db();

        let every = explain(
            &db,
            "SELECT * FROM users u JOIN orders o ON o.user_id = u.id",
        );
        let one = explain(&db, "SELECT o.*, u.name FROM users u, orders o");

        assert_eq!(
            every.lines().next().unwrap(),
            "Project: u.id AS id, u.name AS name, u.email AS email, o.id AS id, \
             o.user_id AS user_id, o.total AS total"
        );
        assert_eq!(
            one.lines().next().unwrap(),
            "Project: o.id AS id, o.user_id AS user_id, o.total AS total, u.name AS name"
        );
    }

    #[test]
    fn joins_should_plan_every_kind_left_to_right() {
        let db = db();

        let text = explain(
            &db,
            "SELECT 1 FROM users u LEFT JOIN orders o ON o.user_id = u.id \
             RIGHT OUTER JOIN items i ON i.order_id = o.id \
             FULL JOIN users v ON v.id = u.id CROSS JOIN orders p, items",
        );

        assert_eq!(
            text,
            "Project: 1 AS \"?column?\"\n\
             \x20 Join CROSS\n\
             \x20   Join CROSS\n\
             \x20     Join FULL: v.id = u.id\n\
             \x20       Join RIGHT: i.order_id = o.id\n\
             \x20         Join LEFT: o.user_id = u.id\n\
             \x20           Scan: users AS u\n\
             \x20           Scan: orders AS o\n\
             \x20         Scan: items AS i\n\
             \x20       Scan: users AS v\n\
             \x20     Scan: orders AS p\n\
             \x20   Scan: items"
        );
    }

    #[test]
    fn a_parenthesized_join_should_be_one_input() {
        let db = db();

        let text = explain(
            &db,
            "SELECT 1 FROM users u JOIN (orders o JOIN items i ON i.order_id = o.id) \
             ON o.user_id = u.id",
        );

        assert_eq!(
            text,
            "Project: 1 AS \"?column?\"\n\
             \x20 Join INNER: o.user_id = u.id\n\
             \x20   Scan: users AS u\n\
             \x20   Join INNER: i.order_id = o.id\n\
             \x20     Scan: orders AS o\n\
             \x20     Scan: items AS i"
        );
    }

    #[test]
    fn where_should_filter_the_joined_rows() {
        let db = db();

        let text = explain(
            &db,
            "SELECT name FROM users u JOIN orders o ON o.user_id = u.id \
             WHERE total BETWEEN 10 AND 20 AND email IS NOT NULL",
        );

        assert_eq!(
            text,
            "Project: u.name AS name\n\
             \x20 Filter: ((o.total >= 10) AND (o.total <= 20)) AND (u.email IS NOT NULL)\n\
             \x20   Join INNER: o.user_id = u.id\n\
             \x20     Scan: users AS u\n\
             \x20     Scan: orders AS o"
        );
    }

    #[test]
    fn a_select_without_from_should_read_one_row() {
        let db = db();

        let text = explain(&db, "SELECT 1 + 1 AS two WHERE TRUE");

        assert_eq!(text, "Project: 1 + 1 AS two\n  Filter: TRUE\n    OneRow");
    }

    #[test]
    fn unknown_or_ambiguous_names_should_fail() {
        let db = db();
        let cases = [
            ("SELECT 1 FROM nowhere", "42P01"),
            ("SELECT zz FROM users", "42703"),
            ("SELECT u.zz FROM users u", "42703"),
            ("SELECT id FROM users u, orders o", "42702"),
            ("SELECT 1 FROM users, users", "42712"),
            ("SELECT 1 FROM users u JOIN orders u ON TRUE", "42712"),
            // The alias hides the table's own name.
            ("SELECT users.id FROM users u", "42P01"),
            ("SELECT x.* FROM users", "42P01"),
            ("SELECT *", "42601"),
            ("SELECT id", "42703"),
        ];

        for (sql, sqlstate) in cases {
            assert_eq!(plan_err(&db, sql).sqlstate(), sqlstate, "{sql}");
        }
    }

    #[test]
    fn an_on_should_see_only_the_tables_of_its_join() {
        let db = db();

        let later = plan_err(
            &db,
            "SELECT 1 FROM users u JOIN orders o ON i.order_id = o.id JOIN items i ON TRUE",
        );
        let before_a_comma = plan_err(
            &db,
            "SELECT 1 FROM users u, orders o JOIN items i ON i.order_id = u.id",
        );

        assert!(matches!(&later, PlannerError::UndefinedTableReference { table } if table == "i"));
        assert!(
            matches!(&before_a_comma, PlannerError::UndefinedTableReference { table } if table == "u")
        );
    }

    #[test]
    fn aggregates_in_where_or_on_should_fail() {
        let db = db();

        let in_where = plan_err(&db, "SELECT 1 FROM users WHERE count(*) > 1");
        let in_on = plan_err(
            &db,
            "SELECT 1 FROM users u JOIN orders o ON sum(o.total) > 1",
        );

        assert_eq!(
            in_where.to_string(),
            "aggregate functions are not allowed in WHERE"
        );
        assert_eq!(
            in_on.to_string(),
            "aggregate functions are not allowed in JOIN conditions"
        );
        assert_eq!(in_on.sqlstate(), "42803");
    }

    #[test]
    fn group_by_should_aggregate_and_project_over_the_groups() {
        let db = db();

        let text = explain(
            &db,
            "SELECT name, count(*), sum(o.total) AS spent FROM users u \
             JOIN orders o ON o.user_id = u.id GROUP BY name HAVING count(*) > 1",
        );

        assert_eq!(
            text,
            "Project: u.name AS name, count(*) AS count, sum(o.total) AS spent\n\
             \x20 Filter: count(*) > 1\n\
             \x20   Aggregate: group by u.name; count(*), sum(o.total)\n\
             \x20     Join INNER: o.user_id = u.id\n\
             \x20       Scan: users AS u\n\
             \x20       Scan: orders AS o"
        );
    }

    #[test]
    fn aggregates_or_having_without_group_by_should_make_one_group() {
        let db = db();

        let aggregates = explain(&db, "SELECT count(*), max(id) FROM users");
        let having = explain(&db, "SELECT 1 FROM users HAVING TRUE");

        assert_eq!(
            aggregates,
            "Project: count(*) AS count, max(users.id) AS max\n\
             \x20 Aggregate: count(*), max(users.id)\n\
             \x20   Scan: users"
        );
        assert_eq!(
            having,
            "Project: 1 AS \"?column?\"\n\
             \x20 Filter: TRUE\n\
             \x20   Aggregate: one group\n\
             \x20     Scan: users"
        );
    }

    #[test]
    fn a_group_expression_should_match_wherever_it_is_written() {
        let db = db();

        let inside = explain(
            &db,
            "SELECT (id + 1) * 2, ID + 1 FROM users GROUP BY users.id + 1",
        );
        let qualified = explain(&db, "SELECT u.id, id FROM users u GROUP BY id");

        assert_eq!(
            inside,
            "Project: (users.id + 1) * 2 AS \"?column?\", (users.id + 1) AS \"?column?\"\n\
             \x20 Aggregate: group by users.id + 1\n\
             \x20   Scan: users"
        );
        assert_eq!(
            qualified.lines().next().unwrap(),
            "Project: u.id AS id, u.id AS id"
        );
    }

    #[test]
    fn repeated_aggregates_should_be_computed_once() {
        let db = db();

        let text = explain(
            &db,
            "SELECT count(id), avg(id * 2), min(name), COUNT(users.id) FROM users",
        );

        assert_eq!(
            text,
            "Project: count(users.id) AS count, avg(users.id * 2) AS avg, \
             min(users.name) AS min, count(users.id) AS count\n\
             \x20 Aggregate: count(users.id), avg(users.id * 2), min(users.name)\n\
             \x20   Scan: users"
        );
    }

    #[test]
    fn a_column_outside_the_groups_should_fail() {
        let db = db();
        let cases = [
            ("SELECT name FROM users GROUP BY id", "users.name"),
            ("SELECT id, count(*) FROM users", "users.id"),
            ("SELECT id FROM users HAVING count(*) > 0", "users.id"),
            (
                "SELECT count(*) FROM users GROUP BY id HAVING name = 'a'",
                "users.name",
            ),
            ("SELECT * FROM users GROUP BY id", "users.name"),
            ("SELECT u.* FROM users u GROUP BY id, name", "u.email"),
            ("SELECT id + 1 FROM users GROUP BY id + 2", "users.id"),
        ];

        for (sql, column) in cases {
            let err = plan_err(&db, sql);

            assert!(
                matches!(&err, PlannerError::GroupingError { column: found } if found == column),
                "{sql}: {err:?}"
            );
            assert_eq!(err.sqlstate(), "42803");
        }
    }

    #[test]
    fn aggregates_in_the_wrong_place_should_fail() {
        let db = db();

        let nested = plan_err(&db, "SELECT sum(count(*)) FROM users");
        let in_group_by = plan_err(&db, "SELECT 1 FROM users GROUP BY count(*)");
        let calls = [
            "SELECT sum(*) FROM users",
            "SELECT count(id, name) FROM users",
            "SELECT max() FROM users",
            "SELECT f(id), count(*) FROM users",
        ]
        .map(|sql| plan_err(&db, sql).to_string());

        assert!(
            matches!(nested, PlannerError::NestedAggregate),
            "{nested:?}"
        );
        assert_eq!(
            in_group_by.to_string(),
            "aggregate functions are not allowed in GROUP BY"
        );
        assert_eq!(
            calls,
            [
                "function \"sum(*)\" does not exist",
                "function \"count(id, name)\" does not exist",
                "function \"max()\" does not exist",
                "function \"f\" does not exist",
            ]
        );
    }

    #[test]
    fn order_by_should_sort_by_position_alias_or_expression() {
        let db = db();

        let text = explain(
            &db,
            "SELECT name AS who, id FROM users \
             ORDER BY 2 DESC, who, email NULLS FIRST, id + 1 DESC NULLS LAST, -1",
        );

        assert_eq!(
            text,
            "Project: users.name AS who, users.id AS id\n\
             \x20 Sort: users.id DESC NULLS FIRST, users.name ASC NULLS LAST, \
             users.email ASC NULLS FIRST, users.id + 1 DESC NULLS LAST, -1 ASC NULLS LAST\n\
             \x20   Scan: users"
        );
    }

    #[test]
    fn an_output_name_should_win_over_an_input_column() {
        let db = db();

        let bare = explain(&db, "SELECT email AS name FROM users ORDER BY name");
        let qualified = explain(&db, "SELECT email AS name FROM users ORDER BY users.name");

        assert_eq!(
            bare.lines().nth(1).unwrap(),
            "  Sort: users.email ASC NULLS LAST"
        );
        assert_eq!(
            qualified.lines().nth(1).unwrap(),
            "  Sort: users.name ASC NULLS LAST"
        );
    }

    #[test]
    fn an_order_by_that_names_nothing_or_two_columns_should_fail() {
        let db = db();
        let cases = [
            ("SELECT id AS x, name AS x FROM users ORDER BY x", "42702"),
            ("SELECT id FROM users ORDER BY 0", "42P10"),
            ("SELECT id, name FROM users ORDER BY 3", "42P10"),
            (
                "SELECT id FROM users ORDER BY 99999999999999999999",
                "42P10",
            ),
            ("SELECT id FROM users ORDER BY zz", "42703"),
            // An output name is not a column of the expressions.
            ("SELECT id AS x FROM users ORDER BY x + 1", "42703"),
        ];

        for (sql, sqlstate) in cases {
            assert_eq!(plan_err(&db, sql).sqlstate(), sqlstate, "{sql}");
        }
        assert_eq!(
            plan_err(&db, "SELECT id FROM users ORDER BY 2").to_string(),
            "ORDER BY position 2 is not in select list"
        );
        assert_eq!(
            plan_err(&db, "SELECT id AS x, name AS x FROM users ORDER BY x").to_string(),
            "ORDER BY \"x\" is ambiguous"
        );
    }

    #[test]
    fn order_by_in_a_grouped_query_should_read_the_groups() {
        let db = db();

        let by_aggregate = explain(
            &db,
            "SELECT name FROM users GROUP BY name ORDER BY count(*) DESC, name",
        );
        let one_group = explain(&db, "SELECT 1 FROM users ORDER BY max(id)");
        let outside = plan_err(&db, "SELECT name FROM users GROUP BY name ORDER BY id");

        assert_eq!(
            by_aggregate,
            "Project: users.name AS name\n\
             \x20 Sort: count(*) DESC NULLS FIRST, users.name ASC NULLS LAST\n\
             \x20   Aggregate: group by users.name; count(*)\n\
             \x20     Scan: users"
        );
        assert_eq!(
            one_group,
            "Project: 1 AS \"?column?\"\n\
             \x20 Sort: max(users.id) ASC NULLS LAST\n\
             \x20   Aggregate: max(users.id)\n\
             \x20     Scan: users"
        );
        assert!(
            matches!(&outside, PlannerError::GroupingError { column } if column == "users.id"),
            "{outside:?}"
        );
    }

    #[test]
    fn limit_and_offset_should_sit_between_the_sort_and_the_projection() {
        let db = db();

        let sorted = explain(&db, "SELECT id FROM users ORDER BY id LIMIT 1 + 4 OFFSET 2");
        let offset_only = explain(&db, "SELECT id FROM users OFFSET 3 ROWS");

        assert_eq!(
            sorted,
            "Project: users.id AS id\n\
             \x20 Limit: 1 + 4 OFFSET 2\n\
             \x20   Sort: users.id ASC NULLS LAST\n\
             \x20     Scan: users"
        );
        assert_eq!(
            offset_only,
            "Project: users.id AS id\n  Limit: ALL OFFSET 3\n    Scan: users"
        );
    }

    #[test]
    fn a_row_count_with_a_column_or_an_aggregate_should_fail() {
        let db = db();

        let messages = [
            "SELECT id FROM users LIMIT id",
            "SELECT id FROM users OFFSET id + 1",
            "SELECT id FROM users LIMIT count(*)",
            "SELECT id FROM users LIMIT id + count(*)",
        ]
        .map(|sql| plan_err(&db, sql).to_string());

        assert_eq!(
            messages,
            [
                "argument of LIMIT must not contain variables",
                "argument of OFFSET must not contain variables",
                "aggregate functions are not allowed in LIMIT",
                "aggregate functions are not allowed in LIMIT",
            ]
        );
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

        fn chance(&mut self, one_in: usize) -> bool {
            self.below(one_in) == 0
        }

        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[self.below(items.len())]
        }
    }

    const TABLES: [(&str, &[&str]); 3] = [
        ("users", &["id", "name", "email"]),
        ("orders", &["id", "user_id", "total"]),
        ("items", &["order_id", "qty"]),
    ];

    /// Random `SELECT`s over the tables of `db()`: mostly valid names, sometimes a wrong one.
    struct QueryGen {
        rng: Rng,
        /// The visible names of the tables in the `FROM` so far, with their columns.
        tables: Vec<(String, &'static [&'static str])>,
    }

    impl QueryGen {
        fn column(&mut self) -> String {
            if self.rng.chance(40) || self.tables.is_empty() {
                return "zz".to_string();
            }
            let (table, columns) = self.tables[self.rng.below(self.tables.len())].clone();
            if columns.is_empty() {
                return "zz".to_string();
            }
            let column = self.rng.pick(columns);
            match self.rng.chance(2) {
                true => format!("{table}.{column}"),
                false => column.to_string(),
            }
        }

        fn table(&mut self) -> String {
            let (name, columns) = match self.rng.chance(60) {
                true => ("nowhere", &[][..]),
                false => TABLES[self.rng.below(TABLES.len())],
            };
            if self.rng.chance(3) {
                self.tables.push((name.to_string(), columns));
                return name.to_string();
            }
            let alias = format!("t{}", self.tables.len());
            self.tables.push((alias.clone(), columns));
            format!("{name} {alias}")
        }

        fn from(&mut self) -> String {
            let mut from = self.table();
            for _ in 0..self.rng.below(3) {
                let join =
                    self.rng
                        .pick(&[", ", " CROSS JOIN ", " JOIN ", " LEFT JOIN ", " FULL JOIN "]);
                from = format!("{from}{join}{}", self.table());
                if !matches!(join, ", " | " CROSS JOIN ") {
                    from = format!("{from} ON {} = {}", self.column(), self.column());
                }
            }
            from
        }

        fn value(&mut self) -> String {
            match self.rng.below(6) {
                0 => "count(*)".to_string(),
                1 => {
                    let function = self.rng.pick(&["sum", "min", "max", "count", "avg"]);
                    format!("{function}({})", self.column())
                }
                2 => format!("{} + {}", self.column(), self.rng.below(9)),
                3 => self.rng.below(9).to_string(),
                _ => self.column(),
            }
        }

        fn select(&mut self) -> String {
            self.tables.clear();
            let from = match self.rng.chance(8) {
                true => String::new(),
                false => format!(" FROM {}", self.from()),
            };
            let items: Vec<String> = (0..1 + self.rng.below(3))
                .map(|index| match self.rng.below(8) {
                    0 => "*".to_string(),
                    1 if !self.tables.is_empty() => {
                        let table = self.tables[self.rng.below(self.tables.len())].0.clone();
                        format!("{table}.*")
                    }
                    2 => format!("{} AS c{index}", self.value()),
                    _ => self.value(),
                })
                .collect();
            let mut sql = format!("SELECT {}{from}", items.join(", "));
            if self.rng.chance(3) {
                sql += &format!(" WHERE {} > {}", self.column(), self.rng.below(9));
            }
            if self.rng.chance(3) {
                let groups: Vec<String> =
                    (0..1 + self.rng.below(2)).map(|_| self.column()).collect();
                sql += &format!(" GROUP BY {}", groups.join(", "));
            }
            if self.rng.chance(5) {
                sql += " HAVING count(*) > 1";
            }
            if self.rng.chance(2) {
                let key = match self.rng.below(3) {
                    0 => (1 + self.rng.below(items.len() + 1)).to_string(),
                    1 => format!("c{}", self.rng.below(3)),
                    _ => self.value(),
                };
                sql += &format!(" ORDER BY {key} DESC");
            }
            if self.rng.chance(3) {
                sql += &format!(" LIMIT {} OFFSET 1", self.rng.below(9));
            }
            sql
        }
    }

    fn positions(expr: &Expr) -> Vec<usize> {
        match expr {
            Expr::Column(position) => vec![*position],
            Expr::Literal(_) => Vec::new(),
            Expr::Binary { left, right, .. } | Expr::IsDistinctFrom { left, right, .. } => {
                [positions(left), positions(right)].concat()
            }
            Expr::Not(expr)
            | Expr::Negate(expr)
            | Expr::IsNull { expr, .. }
            | Expr::Test { expr, .. }
            | Expr::Cast { expr, .. } => positions(expr),
        }
    }

    /// Every column an expression of a node reads is inside the row it reads: its input's, both
    /// sides' for the `ON` of a join, none for the counts of a `Limit`.
    fn assert_columns_in_range(plan: &LogicalPlan, sql: &str) {
        let width = |plan: &LogicalPlan| plan.columns().len();
        let (exprs, width): (Vec<&Expr>, usize) = match plan {
            LogicalPlan::Scan { .. } | LogicalPlan::OneRow => (Vec::new(), 0),
            LogicalPlan::Filter { input, predicate } => (vec![predicate], width(input)),
            LogicalPlan::Join {
                left, right, on, ..
            } => (on.iter().collect(), width(left) + width(right)),
            LogicalPlan::Aggregate {
                input,
                group_by,
                aggregates,
            } => {
                let arguments = aggregates.iter().filter_map(|aggregate| match aggregate {
                    Aggregate::CountStar => None,
                    Aggregate::Count(expr)
                    | Aggregate::Sum(expr)
                    | Aggregate::Avg(expr)
                    | Aggregate::Min(expr)
                    | Aggregate::Max(expr) => Some(expr),
                });
                (group_by.iter().chain(arguments).collect(), width(input))
            }
            LogicalPlan::Sort { input, keys } => {
                (keys.iter().map(|key| &key.expr).collect(), width(input))
            }
            LogicalPlan::Limit { count, offset, .. } => (count.iter().chain(offset).collect(), 0),
            LogicalPlan::Project { input, columns } => (
                columns.iter().map(|column| &column.expr).collect(),
                width(input),
            ),
        };
        for expr in exprs {
            for position in positions(expr) {
                assert!(position < width, "{sql}: #{position} of {width}\n{plan}");
            }
        }
        for input in plan.inputs() {
            assert_columns_in_range(input, sql);
        }
    }

    /// Without wildcards, each item is one output column: an item written as a column must read
    /// the input column of that name (and table, when qualified).
    fn assert_columns_read_their_names(select: &ast::Select, plan: &LogicalPlan, sql: &str) {
        let LogicalPlan::Project { input, columns } = plan else {
            panic!("{sql}: not a projection\n{plan}");
        };
        let has_wildcard = select
            .projection
            .iter()
            .any(|item| !matches!(item, ast::SelectItem::Expr { .. }));
        if has_wildcard {
            return;
        }
        let input_columns = input.columns();
        for (item, column) in select.projection.iter().zip(columns) {
            let ast::SelectItem::Expr {
                expr: ast::Expr::Column { table, name },
                ..
            } = item
            else {
                continue;
            };
            let Expr::Column(position) = column.expr else {
                panic!("{sql}: {name} is {:?}", column.expr);
            };
            let read = &input_columns[position];
            assert_eq!(&read.name, name, "{sql}\n{plan}");
            if table.is_some() {
                assert_eq!(&read.table, table, "{sql}\n{plan}");
            }
        }
    }

    #[test]
    fn random_selects_should_plan_with_every_column_in_range() {
        let db = db();
        let mut queries = QueryGen {
            rng: Rng(0x9E37_79B9_7F4A_7C15),
            tables: Vec::new(),
        };
        let expected_errors = [
            "42P01", "42702", "42703", "42712", "42803", "42P10", "42601",
        ];
        let mut planned = 0;

        for _ in 0..2_000 {
            let sql = queries.select();
            let select = select(&sql);

            match plan_select(&select, &db.catalog) {
                Ok(plan) => {
                    planned += 1;
                    assert_columns_in_range(&plan, &sql);
                    assert_columns_read_their_names(&select, &plan, &sql);
                }
                Err(err) => assert!(expected_errors.contains(&err.sqlstate()), "{sql}: {err}"),
            }
        }
        // Enough valid queries for the invariants to mean something.
        assert!(planned > 400, "{planned}");
    }

    #[test]
    fn order_by_should_accept_two_output_columns_of_the_same_expression() {
        let db = db();

        let repeated = explain(&db, "SELECT id, id FROM users ORDER BY id");
        let qualified = explain(&db, "SELECT u.id, id FROM users u ORDER BY id DESC");
        let with_wildcard = explain(&db, "SELECT *, id FROM users ORDER BY id");
        let different = plan_err(&db, "SELECT id AS x, name AS x FROM users ORDER BY x");

        assert_eq!(
            repeated.lines().nth(1).unwrap(),
            "  Sort: users.id ASC NULLS LAST"
        );
        assert_eq!(
            qualified.lines().nth(1).unwrap(),
            "  Sort: u.id DESC NULLS FIRST"
        );
        assert_eq!(
            with_wildcard.lines().nth(1).unwrap(),
            "  Sort: users.id ASC NULLS LAST"
        );
        assert_eq!(different.sqlstate(), "42702");
    }

    #[test]
    fn order_by_a_name_of_two_different_wildcard_columns_should_be_ambiguous() {
        let db = db();

        let err = plan_err(
            &db,
            "SELECT * FROM users u JOIN orders o ON o.user_id = u.id ORDER BY id",
        );

        assert!(
            matches!(&err, PlannerError::AmbiguousOrderBy { name } if name == "id"),
            "{err:?}"
        );
    }

    #[test]
    fn an_order_by_position_should_be_named_as_a_number() {
        let db = db();

        let padded = plan_err(&db, "SELECT id FROM users ORDER BY 007");
        let huge = plan_err(&db, "SELECT id FROM users ORDER BY 99999999999999999999");
        let zero = plan_err(&db, "SELECT id FROM users ORDER BY 000");

        assert_eq!(
            padded.to_string(),
            "ORDER BY position 7 is not in select list"
        );
        assert_eq!(
            huge.to_string(),
            "ORDER BY position 99999999999999999999 is not in select list"
        );
        assert_eq!(
            zero.to_string(),
            "ORDER BY position 0 is not in select list"
        );
    }

    #[test]
    fn group_by_should_refuse_a_position() {
        let db = db();

        let position = plan_err(&db, "SELECT name, count(*) FROM users GROUP BY 1");
        let expressions = [
            explain(&db, "SELECT count(*) FROM users GROUP BY 1 + 0"),
            explain(&db, "SELECT count(*) FROM users GROUP BY 1.5"),
        ];

        assert_eq!(position.to_string(), "GROUP BY position is not supported");
        assert_eq!(position.sqlstate(), "0A000");
        assert_eq!(
            expressions.map(|text| text.lines().nth(1).unwrap().to_string()),
            [
                "  Aggregate: group by 1 + 0; count(*)",
                "  Aggregate: group by 1.5; count(*)",
            ]
        );
    }

    #[test]
    fn having_should_not_see_output_names() {
        let db = db();

        let err = plan_err(&db, "SELECT count(*) AS n FROM users HAVING n > 1");

        assert!(
            matches!(&err, PlannerError::UndefinedColumn { column } if column == "n"),
            "{err:?}"
        );
    }

    #[test]
    fn a_group_expression_should_match_only_with_the_same_literal_types() {
        let db = db();

        let err = plan_err(&db, "SELECT id + 1.0 FROM users GROUP BY id + 1");

        assert!(
            matches!(&err, PlannerError::GroupingError { column } if column == "users.id"),
            "{err:?}"
        );
    }

    #[test]
    fn explain_of_deep_expressions_and_joins_should_fit_a_small_stack() {
        let chain = "id + ".repeat(190);
        let joins: String = (0..199)
            .map(|i| format!(" CROSS JOIN users u{i}"))
            .collect();
        let statements = [
            format!("SELECT {}id FROM users", "- ".repeat(190)),
            format!("SELECT {chain}1, count(*) FROM users GROUP BY {chain}1"),
            format!("SELECT 1 FROM users{joins}"),
        ];

        let lines = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let db = db();
                statements.map(|sql| explain(&db, &sql).lines().count())
            })
            .unwrap()
            .join()
            .unwrap();

        assert_eq!(lines, [2, 3, 400]);
    }
}
