//! Pushing filters down: each conjunct of a `WHERE` or of the `ON` of a join goes as far down
//! the logical plan as it may without changing the rows of the query, so that rows are dropped
//! early, a filter right above a table can use its index, and an equality between the two
//! tables of an inner join becomes its condition (and so a hash key).

use crate::execution::{Expr, JoinType};

use super::logical_plan::LogicalPlan;
use super::predicate::{Side, conjunction, conjuncts, rebased, side};

/// Each conjunct of every filter and every `ON`, as far down as it may go.
pub(super) fn push_down_filters(plan: LogicalPlan) -> LogicalPlan {
    push(plan, Vec::new())
}

/// `plan` with `pending`, conjuncts over its rows, applied in it or below it.
fn push(plan: LogicalPlan, mut pending: Vec<Expr>) -> LogicalPlan {
    match plan {
        LogicalPlan::Filter { input, predicate } => {
            pending.extend(conjuncts(predicate));
            push(*input, pending)
        }
        LogicalPlan::Join {
            left,
            right,
            join_type,
            on,
        } => push_into_join(*left, *right, join_type, on, pending),
        // Below these the rows are other rows (grouped, cut, projected): what is pending stays
        // above, and the rewrite goes on inside with nothing pending.
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => filtered(
            LogicalPlan::Aggregate {
                input: Box::new(push_down_filters(*input)),
                group_by,
                aggregates,
            },
            pending,
        ),
        LogicalPlan::Sort { input, keys } => filtered(
            LogicalPlan::Sort {
                input: Box::new(push_down_filters(*input)),
                keys,
            },
            pending,
        ),
        LogicalPlan::Limit {
            input,
            count,
            offset,
        } => filtered(
            LogicalPlan::Limit {
                input: Box::new(push_down_filters(*input)),
                count,
                offset,
            },
            pending,
        ),
        LogicalPlan::Project { input, columns } => filtered(
            LogicalPlan::Project {
                input: Box::new(push_down_filters(*input)),
                columns,
            },
            pending,
        ),
        scan @ (LogicalPlan::Scan { .. } | LogicalPlan::OneRow) => filtered(scan, pending),
    }
}

/// Where each conjunct of the `WHERE` above a join (`pending`) and of its `ON` goes: down one
/// side, into the `ON`, or above the join.
fn push_into_join(
    left: LogicalPlan,
    right: LogicalPlan,
    join_type: JoinType,
    on: Option<Expr>,
    pending: Vec<Expr>,
) -> LogicalPlan {
    let left_width = left.columns().len();
    let mut to_left = Vec::new();
    let mut to_right = Vec::new();
    let mut above = Vec::new();
    let mut on: Vec<Expr> = on.map(conjuncts).unwrap_or_default();

    for conjunct in pending {
        match from_where(side(&conjunct, left_width), join_type) {
            Destination::Left => to_left.push(conjunct),
            Destination::Right => to_right.push(rebased(&conjunct, left_width)),
            Destination::On => on.push(conjunct),
            Destination::Here => above.push(conjunct),
        }
    }
    let mut kept_on = Vec::new();
    for conjunct in on {
        match from_on(side(&conjunct, left_width), join_type) {
            Destination::Left => to_left.push(conjunct),
            Destination::Right => to_right.push(rebased(&conjunct, left_width)),
            Destination::On | Destination::Here => kept_on.push(conjunct),
        }
    }

    let join = LogicalPlan::Join {
        left: Box::new(push(left, to_left)),
        right: Box::new(push(right, to_right)),
        join_type,
        on: conjunction(kept_on),
    };
    filtered(join, above)
}

/// Where a conjunct goes from a join: down its left or right input, into its `ON`, or where it
/// is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Destination {
    Left,
    Right,
    On,
    Here,
}

/// A conjunct of the `WHERE` above a join goes down a side whose rows the join never pads with
/// NULLs (both of an inner join, the left of a `LEFT` join): there it drops the same rows it
/// would drop above. On a padded side it would also drop the padded rows, which it rejects
/// above too, but only after they were made. Comparing both sides, it becomes the condition of
/// an inner join.
fn from_where(side: Side, join_type: JoinType) -> Destination {
    match (side, join_type) {
        (Side::Left, JoinType::Inner | JoinType::Left) => Destination::Left,
        (Side::Right, JoinType::Inner | JoinType::Right) => Destination::Right,
        (Side::Both, JoinType::Inner) => Destination::On,
        _ => Destination::Here,
    }
}

/// A conjunct of an `ON` only chooses pairs, so it goes down a side whose rows it may drop
/// without dropping a row of the result: either side of an inner join, the padded side of an
/// outer one (a `LEFT` join's right rows that fail it match nothing anyway). On the side the
/// join keeps, a row it rejects must still come out, padded, so it stays in the `ON`.
fn from_on(side: Side, join_type: JoinType) -> Destination {
    match (side, join_type) {
        (Side::Left, JoinType::Inner | JoinType::Right) => Destination::Left,
        (Side::Right, JoinType::Inner | JoinType::Left) => Destination::Right,
        _ => Destination::On,
    }
}

/// `plan` under a filter of `conjuncts`, if there are any.
fn filtered(plan: LogicalPlan, conjuncts: Vec<Expr>) -> LogicalPlan {
    match conjunction(conjuncts) {
        Some(predicate) => LogicalPlan::Filter {
            input: Box::new(plan),
            predicate,
        },
        None => plan,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::execution::test_support::TestDb;
    use crate::planner::{plan_select, run_sql};
    use crate::sql::{Statement, parse};

    /// `users (id, name)`, `orders (id, user_id, total)` and `items (order_id, qty)`.
    fn db() -> TestDb {
        let mut db = TestDb::new(8);
        run_sql(
            "CREATE TABLE users (id INTEGER NOT NULL, name VARCHAR(20)); \
             CREATE TABLE orders (id INTEGER NOT NULL, user_id INTEGER, total BIGINT); \
             CREATE TABLE items (order_id INTEGER, qty INTEGER)",
            &mut db.ctx(),
        )
        .unwrap();
        db
    }

    fn logical(db: &TestDb, sql: &str) -> LogicalPlan {
        match parse(sql).unwrap().as_slice() {
            [Statement::Select(select)] => plan_select(select, &db.catalog).unwrap(),
            other => panic!("{other:?}"),
        }
    }

    /// The text of the plan of `sql` after pushing its filters down.
    fn pushed(db: &TestDb, sql: &str) -> String {
        push_down_filters(logical(db, sql)).to_string()
    }

    #[test]
    fn a_conjunct_of_one_table_should_go_down_to_it() {
        let db = db();

        let text = pushed(
            &db,
            "SELECT u.name FROM users u JOIN orders o ON o.user_id = u.id \
             WHERE u.name = 'ana' AND o.total > 10",
        );

        assert_eq!(
            text,
            "Project: u.name AS name\n\
             \x20 Join INNER: o.user_id = u.id\n\
             \x20   Filter: u.name = 'ana'\n\
             \x20     Scan: users AS u\n\
             \x20   Filter: o.total > 10\n\
             \x20     Scan: orders AS o"
        );
    }

    #[test]
    fn an_equality_between_the_tables_should_become_the_condition_of_their_join() {
        let db = db();

        let text = pushed(
            &db,
            "SELECT 1 FROM users u, orders o WHERE o.user_id = u.id AND o.total > 10",
        );

        assert_eq!(
            text,
            "Project: 1 AS \"?column?\"\n\
             \x20 Join INNER: o.user_id = u.id\n\
             \x20   Scan: users AS u\n\
             \x20   Filter: o.total > 10\n\
             \x20     Scan: orders AS o"
        );
    }

    #[test]
    fn the_conjuncts_of_one_side_of_an_inner_on_should_go_down() {
        let db = db();

        let text = pushed(
            &db,
            "SELECT 1 FROM users u JOIN orders o \
             ON o.user_id = u.id AND o.total > 10 AND u.name IS NOT NULL",
        );
        let emptied = pushed(&db, "SELECT 1 FROM users u JOIN orders o ON o.total > 1");

        assert_eq!(
            text,
            "Project: 1 AS \"?column?\"\n\
             \x20 Join INNER: o.user_id = u.id\n\
             \x20   Filter: u.name IS NOT NULL\n\
             \x20     Scan: users AS u\n\
             \x20   Filter: o.total > 10\n\
             \x20     Scan: orders AS o"
        );
        // An ON left without conjuncts is a cross product again.
        assert_eq!(
            emptied,
            "Project: 1 AS \"?column?\"\n\
             \x20 Join CROSS\n\
             \x20   Scan: users AS u\n\
             \x20   Filter: o.total > 1\n\
             \x20     Scan: orders AS o"
        );
    }

    #[test]
    fn a_conjunct_should_cross_every_join_down_to_its_table() {
        let db = db();

        let text = pushed(
            &db,
            "SELECT 1 FROM users u JOIN orders o ON o.user_id = u.id \
             JOIN items i ON i.order_id = o.id \
             WHERE u.name = 'ana' AND i.qty > u.id AND o.total > 1",
        );

        assert_eq!(
            text,
            "Project: 1 AS \"?column?\"\n\
             \x20 Join INNER: (i.order_id = o.id) AND (i.qty > u.id)\n\
             \x20   Join INNER: o.user_id = u.id\n\
             \x20     Filter: u.name = 'ana'\n\
             \x20       Scan: users AS u\n\
             \x20     Filter: o.total > 1\n\
             \x20       Scan: orders AS o\n\
             \x20   Scan: items AS i"
        );
    }

    #[test]
    fn filters_reaching_one_table_should_become_one_filter() {
        let db = db();

        let text = pushed(
            &db,
            "SELECT 1 FROM users u JOIN orders o ON o.user_id = u.id AND u.id < 5 \
             WHERE u.name = 'a' AND u.id > 1",
        );

        assert_eq!(
            text.lines().nth(2).unwrap(),
            "    Filter: ((u.name = 'a') AND (u.id > 1)) AND (u.id < 5)"
        );
    }

    #[test]
    fn a_conjunct_without_columns_should_stay_above_the_join() {
        let db = db();

        let text = pushed(
            &db,
            "SELECT 1 FROM users u JOIN orders o ON o.user_id = u.id WHERE 1 = 1",
        );

        assert_eq!(
            text,
            "Project: 1 AS \"?column?\"\n\
             \x20 Filter: 1 = 1\n\
             \x20   Join INNER: o.user_id = u.id\n\
             \x20     Scan: users AS u\n\
             \x20     Scan: orders AS o"
        );
    }

    #[test]
    fn a_plan_without_a_join_to_cross_should_not_change() {
        let db = db();
        let plans = [
            logical(&db, "SELECT name FROM users WHERE id = 1 AND name = 'a'"),
            logical(
                &db,
                "SELECT 1 FROM users u JOIN orders o ON o.user_id = u.id",
            ),
            logical(&db, "SELECT 1"),
            logical(&db, "SELECT 1 WHERE FALSE"),
        ];

        for plan in plans {
            assert_eq!(push_down_filters(plan.clone()), plan);
        }
    }

    #[test]
    fn a_left_join_should_keep_its_padded_rows() {
        let db = db();

        let text = pushed(
            &db,
            "SELECT 1 FROM users u LEFT JOIN orders o \
             ON o.user_id = u.id AND o.total > 10 AND u.name = 'a' \
             WHERE u.id > 1 AND o.id > 5 AND o.user_id = u.id + 0",
        );

        // The WHERE filters the left rows only below; the ON chooses pairs, so a condition
        // on the right table goes into it, while one on the left stays to pad what it rejects.
        assert_eq!(
            text,
            "Project: 1 AS \"?column?\"\n\
             \x20 Filter: (o.id > 5) AND (o.user_id = (u.id + 0))\n\
             \x20   Join LEFT: (o.user_id = u.id) AND (u.name = 'a')\n\
             \x20     Filter: u.id > 1\n\
             \x20       Scan: users AS u\n\
             \x20     Filter: o.total > 10\n\
             \x20       Scan: orders AS o"
        );
    }

    #[test]
    fn a_right_join_should_mirror_a_left_one() {
        let db = db();

        let text = pushed(
            &db,
            "SELECT 1 FROM users u RIGHT JOIN orders o \
             ON o.user_id = u.id AND o.total > 10 AND u.name = 'a' \
             WHERE u.id > 1 AND o.id > 5",
        );

        assert_eq!(
            text,
            "Project: 1 AS \"?column?\"\n\
             \x20 Filter: u.id > 1\n\
             \x20   Join RIGHT: (o.user_id = u.id) AND (o.total > 10)\n\
             \x20     Filter: u.name = 'a'\n\
             \x20       Scan: users AS u\n\
             \x20     Filter: o.id > 5\n\
             \x20       Scan: orders AS o"
        );
    }

    #[test]
    fn a_full_join_should_keep_every_condition_where_it_is() {
        let db = db();

        let text = pushed(
            &db,
            "SELECT 1 FROM users u FULL JOIN orders o ON o.user_id = u.id AND o.total > 10 \
             WHERE u.id > 1 AND o.id > 5",
        );

        assert_eq!(
            text,
            "Project: 1 AS \"?column?\"\n\
             \x20 Filter: (u.id > 1) AND (o.id > 5)\n\
             \x20   Join FULL: (o.user_id = u.id) AND (o.total > 10)\n\
             \x20     Scan: users AS u\n\
             \x20     Scan: orders AS o"
        );
    }

    #[test]
    fn a_conjunct_should_stop_at_an_outer_join_it_may_not_cross() {
        let db = db();

        let text = pushed(
            &db,
            "SELECT 1 FROM users u LEFT JOIN orders o ON o.user_id = u.id \
             JOIN items i ON i.order_id = o.id WHERE u.name = 'a' AND o.total > 1",
        );

        assert_eq!(
            text,
            "Project: 1 AS \"?column?\"\n\
             \x20 Join INNER: i.order_id = o.id\n\
             \x20   Filter: o.total > 1\n\
             \x20     Join LEFT: o.user_id = u.id\n\
             \x20       Filter: u.name = 'a'\n\
             \x20         Scan: users AS u\n\
             \x20       Scan: orders AS o\n\
             \x20   Scan: items AS i"
        );
    }

    #[test]
    fn a_grouped_query_should_push_its_where_and_keep_its_having() {
        let db = db();

        let text = pushed(
            &db,
            "SELECT u.name, count(*) FROM users u JOIN orders o ON o.user_id = u.id \
             WHERE o.total > 1 GROUP BY u.name HAVING count(*) > 1 ORDER BY 1 LIMIT 5",
        );

        assert_eq!(
            text,
            "Project: u.name AS name, count(*) AS count\n\
             \x20 Limit: 5\n\
             \x20   Sort: u.name ASC NULLS LAST\n\
             \x20     Filter: count(*) > 1\n\
             \x20       Aggregate: group by u.name; count(*)\n\
             \x20         Join INNER: o.user_id = u.id\n\
             \x20           Scan: users AS u\n\
             \x20           Filter: o.total > 1\n\
             \x20             Scan: orders AS o"
        );
    }

    #[test]
    fn a_filter_should_never_go_below_a_limit() {
        let db = db();
        // `SELECT` never puts a filter above a limit; one built by hand must stay there, since
        // below it would cut other rows.
        let limited = LogicalPlan::Limit {
            input: Box::new(logical(&db, "SELECT id FROM users")),
            count: Some(Expr::Literal(crate::types::Value::Integer(1))),
            offset: None,
        };
        let plan = LogicalPlan::Filter {
            input: Box::new(limited),
            predicate: Expr::binary(
                crate::execution::BinaryOp::Gt,
                Expr::Column(0),
                Expr::Literal(crate::types::Value::Integer(1)),
            ),
        };

        let rewritten = push_down_filters(plan.clone());

        assert_eq!(rewritten, plan);
    }

    #[test]
    fn an_anti_join_should_keep_its_filter_above_the_left_join() {
        let db = db();

        let text = pushed(
            &db,
            "SELECT u.id FROM users u LEFT JOIN orders o ON o.user_id = u.id WHERE o.id IS NULL",
        );

        // Down on orders it would keep every user, padded; above, only those without orders.
        assert_eq!(
            text,
            "Project: u.id AS id\n\
             \x20 Filter: o.id IS NULL\n\
             \x20   Join LEFT: o.user_id = u.id\n\
             \x20     Scan: users AS u\n\
             \x20     Scan: orders AS o"
        );
    }

    #[test]
    fn the_on_of_a_left_join_should_reach_a_table_inside_its_right_join() {
        let db = db();

        let text = pushed(
            &db,
            "SELECT 1 FROM users u LEFT JOIN (orders o JOIN items i ON i.order_id = o.id) \
             ON o.user_id = u.id AND i.qty > 1",
        );

        assert_eq!(
            text,
            "Project: 1 AS \"?column?\"\n\
             \x20 Join LEFT: o.user_id = u.id\n\
             \x20   Scan: users AS u\n\
             \x20   Join INNER: i.order_id = o.id\n\
             \x20     Scan: orders AS o\n\
             \x20     Filter: i.qty > 1\n\
             \x20       Scan: items AS i"
        );
    }
}
