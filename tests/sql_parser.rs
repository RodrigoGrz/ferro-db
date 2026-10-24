use ferro_db::sql::{
    Expr, JoinKind, Literal, Select, SelectItem, SqlError, Statement, TableRef, parse,
};

fn select(sql: &str) -> Select {
    match parse(sql).unwrap().as_slice() {
        [Statement::Select(select)] => (**select).clone(),
        other => panic!("{other:?}"),
    }
}

/// An optional expression as canonical SQL, for compact assertions.
fn shown(expr: &Option<Expr>) -> Option<String> {
    expr.as_ref().map(Expr::to_string)
}

#[test]
fn a_select_with_a_join_grouping_ordering_and_limit_should_parse_clause_by_clause() {
    let query = select(
        "SELECT c.name, COUNT(*) AS orders\n\
         FROM customers c LEFT JOIN orders o ON o.customer_id = c.id\n\
         WHERE o.amount >= 10.5 AND c.name <> 'Ana'\n\
         GROUP BY c.name HAVING COUNT(*) > 2\n\
         ORDER BY 2 DESC NULLS LAST LIMIT 10 OFFSET 20",
    );

    let projection: Vec<String> = query
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::Expr { expr, alias } => format!("{expr} AS {alias:?}"),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        projection,
        ["c.name AS None", "count(*) AS Some(\"orders\")"]
    );
    let Some(TableRef::Join {
        left,
        right,
        kind: JoinKind::Left,
        on,
    }) = &query.from
    else {
        panic!("{:?}", query.from);
    };
    assert_eq!(
        (**left).clone(),
        TableRef::Table {
            name: "customers".to_string(),
            alias: Some("c".to_string())
        }
    );
    assert_eq!(
        (**right).clone(),
        TableRef::Table {
            name: "orders".to_string(),
            alias: Some("o".to_string())
        }
    );
    assert_eq!(shown(on).as_deref(), Some("(o.customer_id = c.id)"));
    assert_eq!(
        shown(&query.selection).as_deref(),
        Some("((o.amount >= 10.5) AND (c.name <> 'Ana'))")
    );
    assert_eq!(query.group_by.len(), 1);
    assert_eq!(shown(&query.having).as_deref(), Some("(count(*) > 2)"));
    assert_eq!(query.order_by.len(), 1);
    assert!(query.order_by[0].is_descending);
    assert_eq!(query.order_by[0].nulls_first, Some(false));
    assert_eq!(
        query.limit,
        Some(Expr::Literal(Literal::Number("10".to_string())))
    );
    assert_eq!(
        query.offset,
        Some(Expr::Literal(Literal::Number("20".to_string())))
    );
}

#[test]
fn the_standard_offset_and_fetch_form_should_parse() {
    let query = select("SELECT * FROM t ORDER BY a OFFSET 5 ROWS FETCH NEXT 3 ROWS ONLY");

    assert_eq!(query.projection, [SelectItem::Wildcard]);
    assert_eq!(shown(&query.limit).as_deref(), Some("3"));
    assert_eq!(shown(&query.offset).as_deref(), Some("5"));
}

#[test]
fn a_script_with_comments_should_give_every_statement() {
    let script =
        "-- first\nSELECT 1; /* second /* nested */ */ SELECT a FROM t WHERE a IS NOT NULL;";

    let statements = parse(script).unwrap();

    let texts: Vec<String> = statements.iter().map(Statement::to_string).collect();
    assert_eq!(texts, ["SELECT 1", "SELECT a FROM t WHERE (a IS NOT NULL)"]);
}

#[test]
fn a_complex_query_should_print_and_parse_back_to_itself() {
    let sql = "SELECT \"Order Id\", -amount * 2 AS twice, CAST(placed AS DATE) \
               FROM (orders o JOIN customers c ON o.c = c.id) CROSS JOIN regions \
               WHERE status IN ('new', 'paid') AND placed BETWEEN DATE '2026-01-01' AND DATE '2026-12-31' \
               AND note IS DISTINCT FROM NULL ORDER BY twice DESC, 1 LIMIT ALL";

    let parsed = parse(sql).unwrap();
    let printed = parsed[0].to_string();
    let reparsed = parse(&printed).unwrap();

    assert_eq!(reparsed, parsed, "{printed}");
}

#[test]
fn errors_deep_in_a_query_should_report_their_position_and_sqlstate() {
    let prefix = "SELECT a, b FROM t JOIN u ON t.id = u.id WHERE ";
    let at = |offset: usize| prefix.chars().count() + offset;

    let unknown_type = parse(&format!("{prefix}CAST(a AS text) = 1")).unwrap_err();
    let bad_length = parse(&format!("{prefix}CAST(a AS VARCHAR(0)) = 'x'")).unwrap_err();
    let dangling = parse(&format!("{prefix}a = ")).unwrap_err();
    let chained = parse(&format!("{prefix}a < b < c")).unwrap_err();

    assert_eq!(
        unknown_type,
        SqlError::UndefinedType {
            name: "text".to_string(),
            position: at(11)
        }
    );
    assert_eq!(unknown_type.sqlstate(), "42704");
    assert_eq!(bad_length.sqlstate(), "22023");
    assert_eq!(
        dangling,
        SqlError::UnexpectedEnd {
            expected: "an expression".to_string()
        }
    );
    assert_eq!(
        chained,
        SqlError::UnexpectedToken {
            found: "<".to_string(),
            expected: "an operator that is not a comparison".to_string(),
            position: at(7)
        }
    );
}
