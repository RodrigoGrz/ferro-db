use ferro_db::sql::{Keyword, SqlError, Token, TokenKind, tokenize};

/// A token as short text: keywords in uppercase, names in lowercase, quoted names and strings
/// with their quotes, numbers and symbols as written.
fn describe(token: &Token) -> String {
    match &token.kind {
        TokenKind::Word {
            keyword: Some(keyword),
            ..
        } => keyword.to_string(),
        TokenKind::Word { text, .. } => text.clone(),
        TokenKind::QuotedIdentifier(name) => format!("\"{name}\""),
        TokenKind::String(text) => format!("'{text}'"),
        TokenKind::Number(literal) => literal.clone(),
        symbol => match symbol {
            TokenKind::Plus => "+",
            TokenKind::Minus => "-",
            TokenKind::Star => "*",
            TokenKind::Slash => "/",
            TokenKind::Percent => "%",
            TokenKind::Concat => "||",
            TokenKind::Eq => "=",
            TokenKind::NotEq => "<>",
            TokenKind::Lt => "<",
            TokenKind::LtEq => "<=",
            TokenKind::Gt => ">",
            TokenKind::GtEq => ">=",
            TokenKind::Comma => ",",
            TokenKind::Semicolon => ";",
            TokenKind::Dot => ".",
            TokenKind::LeftParen => "(",
            TokenKind::RightParen => ")",
            other => panic!("{other:?}"),
        }
        .to_string(),
    }
}

fn described(sql: &str) -> Vec<String> {
    tokenize(sql).unwrap().iter().map(describe).collect()
}

#[test]
fn a_select_with_a_join_grouping_ordering_and_limit_should_tokenize() {
    let sql = "SELECT c.Name, COUNT(*) AS \"Orders\"\n\
               FROM customers c LEFT JOIN orders o ON o.customer_id = c.id\n\
               WHERE o.amount >= 10.5 AND c.name <> 'Ana'\n\
               GROUP BY c.name HAVING COUNT(*) > 2\n\
               ORDER BY 2 DESC NULLS LAST LIMIT 10 OFFSET 20;";

    let tokens = described(sql);

    let expected = "SELECT c . name , count ( * ) AS \"Orders\" \
                    FROM customers c LEFT JOIN orders o ON o . customer_id = c . id \
                    WHERE o . amount >= 10.5 AND c . name <> 'Ana' \
                    GROUP BY c . name HAVING count ( * ) > 2 \
                    ORDER BY 2 DESC NULLS LAST LIMIT 10 OFFSET 20 ;";
    assert_eq!(tokens.join(" "), expected);
}

#[test]
fn a_create_table_should_tokenize_types_and_constraints() {
    let sql = "create table Users (id integer not null primary key, email varchar(320), \
               score double precision, born date, \"Created At\" timestamp)";

    let tokens = described(sql);

    let expected = "CREATE TABLE users ( id INTEGER NOT NULL PRIMARY KEY , email VARCHAR ( 320 ) , \
                    score DOUBLE PRECISION , born DATE , \"Created At\" TIMESTAMP )";
    assert_eq!(tokens.join(" "), expected);
}

#[test]
fn an_insert_should_keep_strings_with_apostrophes_accents_and_newlines() {
    let sql = "INSERT INTO notes VALUES (1, 'it''s ação\nsecond line', -2.5e3, NULL, TRUE)";

    let tokens = tokenize(sql).unwrap();

    let strings: Vec<&TokenKind> = tokens
        .iter()
        .map(|token| &token.kind)
        .filter(|kind| matches!(kind, TokenKind::String(_)))
        .collect();
    assert_eq!(
        strings,
        [&TokenKind::String("it's ação\nsecond line".to_string())]
    );
    assert_eq!(
        described(sql).join(" "),
        "INSERT INTO notes VALUES ( 1 , 'it's ação\nsecond line' , - 2.5e3 , NULL , TRUE )"
    );
}

#[test]
fn a_script_with_comments_should_give_the_tokens_of_every_statement() {
    let sql = "-- create the table\n\
               CREATE TABLE t (n integer); /* then\n fill it /* nested */ */\n\
               INSERT INTO t VALUES (1), (2); -- done\n\
               SELECT n FROM t WHERE n != 1;";

    let tokens = tokenize(sql).unwrap();

    let statements = tokens
        .iter()
        .filter(|token| token.kind == TokenKind::Semicolon)
        .count();
    let keywords: Vec<Keyword> = tokens
        .iter()
        .filter_map(|token| match token.kind {
            TokenKind::Word {
                keyword: Some(keyword),
                ..
            } => Some(keyword),
            _ => None,
        })
        .collect();
    assert_eq!(statements, 3);
    assert_eq!(
        keywords,
        [
            Keyword::Create,
            Keyword::Table,
            Keyword::Integer,
            Keyword::Insert,
            Keyword::Into,
            Keyword::Values,
            Keyword::Select,
            Keyword::From,
            Keyword::Where,
        ]
    );
    for token in &tokens {
        let source = &sql[token.span.start..token.span.end];
        assert_eq!(tokenize(source).unwrap()[0].kind, token.kind, "{source:?}");
    }
}

#[test]
fn an_error_deep_in_a_statement_should_report_its_character_position() {
    let prefix = "SELECT 'ação', \"nome\" FROM t WHERE x = ";
    let unterminated = format!("{prefix}'never closed");
    let stray = format!("{prefix}1 # 2");

    let unterminated_err = tokenize(&unterminated).unwrap_err();
    let stray_err = tokenize(&stray).unwrap_err();

    let quote_position = prefix.chars().count() + 1;
    assert_eq!(
        unterminated_err,
        SqlError::UnterminatedString {
            position: quote_position
        }
    );
    assert_eq!(
        stray_err,
        SqlError::UnexpectedCharacter {
            character: '#',
            position: quote_position + 2
        }
    );
    assert_eq!(stray_err.sqlstate(), "42601");
    assert_eq!(
        stray_err.to_string(),
        format!(
            "unexpected character \"#\" at character {}",
            quote_position + 2
        )
    );
}
