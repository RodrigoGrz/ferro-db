//! What the physical plan and the rewrites share about expressions over the row of a join:
//! splitting a predicate into its conjuncts and joining them again, which side of a join an
//! expression reads, and moving its columns from the joined row to the right input's row.

use crate::execution::{BinaryOp, Expr};

/// Which side of a join the columns of an expression come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    Neither,
    Left,
    Right,
    Both,
}

pub(super) fn side(expr: &Expr, left_width: usize) -> Side {
    let mut positions = Vec::new();
    columns_read(expr, &mut positions);
    let reads_left = positions.iter().any(|position| *position < left_width);
    let reads_right = positions.iter().any(|position| *position >= left_width);
    match (reads_left, reads_right) {
        (false, false) => Side::Neither,
        (true, false) => Side::Left,
        (false, true) => Side::Right,
        (true, true) => Side::Both,
    }
}

fn columns_read(expr: &Expr, positions: &mut Vec<usize>) {
    match expr {
        Expr::Column(position) => positions.push(*position),
        Expr::Literal(_) => {}
        Expr::Binary { left, right, .. } | Expr::IsDistinctFrom { left, right, .. } => {
            columns_read(left, positions);
            columns_read(right, positions);
        }
        Expr::Not(expr)
        | Expr::Negate(expr)
        | Expr::IsNull { expr, .. }
        | Expr::Test { expr, .. }
        | Expr::Cast { expr, .. } => columns_read(expr, positions),
    }
}

/// `expr` with every column moved `by` positions to the left: from the joined row to the right
/// row of a join.
pub(super) fn rebased(expr: &Expr, by: usize) -> Expr {
    let rebase = |expr: &Expr| Box::new(rebased(expr, by));
    match expr {
        Expr::Column(position) => Expr::Column(position.saturating_sub(by)),
        Expr::Literal(value) => Expr::Literal(value.clone()),
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: rebase(left),
            right: rebase(right),
        },
        Expr::Not(expr) => Expr::Not(rebase(expr)),
        Expr::Negate(expr) => Expr::Negate(rebase(expr)),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: rebase(expr),
            negated: *negated,
        },
        Expr::Test {
            expr,
            test,
            negated,
        } => Expr::Test {
            expr: rebase(expr),
            test: *test,
            negated: *negated,
        },
        Expr::IsDistinctFrom {
            left,
            right,
            negated,
        } => Expr::IsDistinctFrom {
            left: rebase(left),
            right: rebase(right),
            negated: *negated,
        },
        Expr::Cast { expr, to } => Expr::Cast {
            expr: rebase(expr),
            to: *to,
        },
    }
}

/// The operands of the `AND`s at the top of `expr`, borrowed, to look at them without copying a
/// predicate that may be large (an `IN` of many values).
pub(super) fn conjunct_refs(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => [conjunct_refs(left), conjunct_refs(right)].concat(),
        other => vec![other],
    }
}

/// The operands of the `AND`s at the top of `expr`.
pub(super) fn conjuncts(expr: Expr) -> Vec<Expr> {
    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => [conjuncts(*left), conjuncts(*right)].concat(),
        other => vec![other],
    }
}

/// The conjuncts joined by `AND` again, or nothing for none.
pub(super) fn conjunction(conjuncts: Vec<Expr>) -> Option<Expr> {
    conjuncts
        .into_iter()
        .reduce(|all, next| Expr::binary(BinaryOp::And, all, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::types::Value;

    fn int(value: i32) -> Expr {
        Expr::Literal(Value::Integer(value))
    }

    fn eq(left: Expr, right: Expr) -> Expr {
        Expr::binary(BinaryOp::Eq, left, right)
    }

    #[test]
    fn side_should_tell_which_input_an_expression_reads() {
        // A left input of two columns: 0 and 1 are its, 2 and on the right's.
        let cases = [
            (int(1), Side::Neither),
            (eq(Expr::Column(0), int(1)), Side::Left),
            (eq(Expr::Column(1), Expr::Column(0)), Side::Left),
            (eq(Expr::Column(2), int(1)), Side::Right),
            (eq(Expr::Column(1), Expr::Column(2)), Side::Both),
            (
                Expr::IsNull {
                    expr: Box::new(Expr::Not(Box::new(Expr::Column(3)))),
                    negated: false,
                },
                Side::Right,
            ),
        ];

        for (expr, expected) in cases {
            assert_eq!(side(&expr, 2), expected, "{expr:?}");
        }
    }

    #[test]
    fn rebased_should_move_every_column_to_the_right_row() {
        let expr = Expr::binary(
            BinaryOp::And,
            eq(Expr::Column(3), int(1)),
            Expr::IsDistinctFrom {
                left: Box::new(Expr::Column(2)),
                right: Box::new(Expr::Negate(Box::new(Expr::Column(4)))),
                negated: true,
            },
        );

        let moved = rebased(&expr, 2);

        assert_eq!(
            moved,
            Expr::binary(
                BinaryOp::And,
                eq(Expr::Column(1), int(1)),
                Expr::IsDistinctFrom {
                    left: Box::new(Expr::Column(0)),
                    right: Box::new(Expr::Negate(Box::new(Expr::Column(2)))),
                    negated: true,
                },
            )
        );
    }

    #[test]
    fn conjuncts_should_split_every_and_and_conjunction_join_them_again() {
        let (a, b, c) = (
            eq(Expr::Column(0), int(1)),
            eq(Expr::Column(1), int(2)),
            Expr::binary(BinaryOp::Or, Expr::Column(2), Expr::Column(3)),
        );
        // (a AND b) AND (c), and a AND (b AND c): the same three conjuncts.
        let left_deep = Expr::binary(
            BinaryOp::And,
            Expr::binary(BinaryOp::And, a.clone(), b.clone()),
            c.clone(),
        );
        let right_deep = Expr::binary(
            BinaryOp::And,
            a.clone(),
            Expr::binary(BinaryOp::And, b.clone(), c.clone()),
        );

        let from_left = conjuncts(left_deep.clone());
        let from_right = conjuncts(right_deep);

        let expected = [a, b, c];
        assert_eq!(from_left, expected);
        assert_eq!(from_right, expected);
        assert_eq!(
            conjunct_refs(&left_deep),
            expected.iter().collect::<Vec<_>>()
        );
        assert_eq!(conjunction(from_left), Some(left_deep));
        assert_eq!(conjunction(Vec::new()), None);
    }
}
