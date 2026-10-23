//! The lexer: the text of a statement as tokens, each with the byte span of its source.
//! Whitespace (`char::is_whitespace`), `--` line comments and `/* */` block comments, which
//! nest, separate tokens and produce none.
//!
//! An unquoted word is folded to lowercase, ASCII letters only, as PostgreSQL does (the
//! standard folds to uppercase), and marked with its keyword if it is one; whether a keyword
//! may also name something is the parser's call. A quoted identifier keeps its text, `""`
//! standing for `"`; a string `''` for `'`. A number keeps its text, the sign being an
//! operator. `!=` is `<>`. Every error is a syntax error (42601) at a character position.

use std::fmt;

use super::SqlError;

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

/// Byte offsets of the token's source text: `&sql[span.start..span.end]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// An unquoted word, folded to lowercase (ASCII letters only); `keyword` when it is one.
    /// Whether a keyword may also name something is the parser's call.
    Word {
        text: String,
        keyword: Option<Keyword>,
    },
    /// A delimited identifier, as written (`""` stands for `"`); never a keyword.
    QuotedIdentifier(String),
    /// A character string literal (`''` stands for `'`).
    String(String),
    /// A numeric literal as written (`42`, `3.14`, `.5`, `1.`, `6.02E23`): the parser gives it a
    /// type. A sign is an operator, never part of the literal.
    Number(String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    /// `||`
    Concat,
    Eq,
    /// `<>`, or `!=`
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Comma,
    Semicolon,
    Dot,
    LeftParen,
    RightParen,
}

/// The words the parser gives a meaning to. A keyword is still a word: whether it may also
/// name a table or a column is decided by the parser, in context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    All,
    And,
    As,
    Asc,
    Between,
    Bigint,
    Boolean,
    By,
    Cast,
    Char,
    Character,
    Create,
    Cross,
    Date,
    Default,
    Delete,
    Desc,
    Distinct,
    Double,
    Drop,
    False,
    Fetch,
    First,
    From,
    Full,
    Group,
    Having,
    In,
    Index,
    Inner,
    Insert,
    Int,
    Integer,
    Into,
    Is,
    Join,
    Key,
    Last,
    Left,
    Limit,
    Next,
    Not,
    Null,
    Nulls,
    Offset,
    On,
    Only,
    Or,
    Order,
    Outer,
    Precision,
    Primary,
    Real,
    Right,
    Row,
    Rows,
    Select,
    Set,
    Smallint,
    Table,
    Timestamp,
    True,
    Unique,
    Unknown,
    Update,
    Values,
    Varchar,
    Varying,
    Where,
}

/// Every keyword by its lowercase text, sorted for a binary search.
const KEYWORDS: [(&str, Keyword); 69] = [
    ("all", Keyword::All),
    ("and", Keyword::And),
    ("as", Keyword::As),
    ("asc", Keyword::Asc),
    ("between", Keyword::Between),
    ("bigint", Keyword::Bigint),
    ("boolean", Keyword::Boolean),
    ("by", Keyword::By),
    ("cast", Keyword::Cast),
    ("char", Keyword::Char),
    ("character", Keyword::Character),
    ("create", Keyword::Create),
    ("cross", Keyword::Cross),
    ("date", Keyword::Date),
    ("default", Keyword::Default),
    ("delete", Keyword::Delete),
    ("desc", Keyword::Desc),
    ("distinct", Keyword::Distinct),
    ("double", Keyword::Double),
    ("drop", Keyword::Drop),
    ("false", Keyword::False),
    ("fetch", Keyword::Fetch),
    ("first", Keyword::First),
    ("from", Keyword::From),
    ("full", Keyword::Full),
    ("group", Keyword::Group),
    ("having", Keyword::Having),
    ("in", Keyword::In),
    ("index", Keyword::Index),
    ("inner", Keyword::Inner),
    ("insert", Keyword::Insert),
    ("int", Keyword::Int),
    ("integer", Keyword::Integer),
    ("into", Keyword::Into),
    ("is", Keyword::Is),
    ("join", Keyword::Join),
    ("key", Keyword::Key),
    ("last", Keyword::Last),
    ("left", Keyword::Left),
    ("limit", Keyword::Limit),
    ("next", Keyword::Next),
    ("not", Keyword::Not),
    ("null", Keyword::Null),
    ("nulls", Keyword::Nulls),
    ("offset", Keyword::Offset),
    ("on", Keyword::On),
    ("only", Keyword::Only),
    ("or", Keyword::Or),
    ("order", Keyword::Order),
    ("outer", Keyword::Outer),
    ("precision", Keyword::Precision),
    ("primary", Keyword::Primary),
    ("real", Keyword::Real),
    ("right", Keyword::Right),
    ("row", Keyword::Row),
    ("rows", Keyword::Rows),
    ("select", Keyword::Select),
    ("set", Keyword::Set),
    ("smallint", Keyword::Smallint),
    ("table", Keyword::Table),
    ("timestamp", Keyword::Timestamp),
    ("true", Keyword::True),
    ("unique", Keyword::Unique),
    ("unknown", Keyword::Unknown),
    ("update", Keyword::Update),
    ("values", Keyword::Values),
    ("varchar", Keyword::Varchar),
    ("varying", Keyword::Varying),
    ("where", Keyword::Where),
];

impl Keyword {
    /// `text` is already folded to lowercase.
    pub(super) fn from_word(text: &str) -> Option<Keyword> {
        KEYWORDS
            .binary_search_by(|(name, _)| (*name).cmp(text))
            .ok()
            .and_then(|index| KEYWORDS.get(index))
            .map(|(_, keyword)| *keyword)
    }
}

/// In uppercase, as SQL is usually written.
impl fmt::Display for Keyword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = KEYWORDS
            .iter()
            .find(|(_, keyword)| keyword == self)
            .map_or("", |(name, _)| name);
        write!(f, "{}", name.to_ascii_uppercase())
    }
}

pub fn tokenize(sql: &str) -> Result<Vec<Token>, SqlError> {
    let mut lexer = Lexer { sql, offset: 0 };
    let mut tokens = Vec::new();
    while let Some(token) = lexer.next_token()? {
        tokens.push(token);
    }
    Ok(tokens)
}

/// A cursor over the statement. `offset` is a byte offset that only ever moves by whole
/// characters, so it is always on a character boundary.
struct Lexer<'a> {
    sql: &'a str,
    offset: usize,
}

impl Lexer<'_> {
    fn next_token(&mut self) -> Result<Option<Token>, SqlError> {
        self.skip_whitespace_and_comments()?;
        let start = self.offset;
        let Some(first) = self.bump() else {
            return Ok(None);
        };
        let kind = match first {
            '+' => TokenKind::Plus,
            '-' => TokenKind::Minus,
            '*' => TokenKind::Star,
            '/' => TokenKind::Slash,
            '%' => TokenKind::Percent,
            '=' => TokenKind::Eq,
            ',' => TokenKind::Comma,
            ';' => TokenKind::Semicolon,
            '.' if self.peek().is_some_and(|next| next.is_ascii_digit()) => self.number(start)?,
            '.' => TokenKind::Dot,
            '0'..='9' => self.number(start)?,
            '(' => TokenKind::LeftParen,
            ')' => TokenKind::RightParen,
            '|' if self.eat('|') => TokenKind::Concat,
            '!' if self.eat('=') => TokenKind::NotEq,
            '<' if self.eat('=') => TokenKind::LtEq,
            '<' if self.eat('>') => TokenKind::NotEq,
            '<' => TokenKind::Lt,
            '>' if self.eat('=') => TokenKind::GtEq,
            '>' => TokenKind::Gt,
            first if is_word_start(first) => self.word(start),
            '"' => self.quoted_identifier(start)?,
            '\'' => self.string(start)?,
            character => {
                return Err(SqlError::UnexpectedCharacter {
                    character,
                    position: self.position(start),
                });
            }
        };
        Ok(Some(Token {
            kind,
            span: Span {
                start,
                end: self.offset,
            },
        }))
    }

    /// The rest of a number whose first character (a digit, or a dot before one) is read:
    /// `digits [. [digits]] [e [+|-] digits]`. An exponent without digits, or a letter or `_`
    /// right after the number, makes the whole run an invalid literal.
    fn number(&mut self, start: usize) -> Result<TokenKind, SqlError> {
        let begins_with_dot = self
            .sql
            .get(start..)
            .is_some_and(|rest| rest.starts_with('.'));
        self.digits();
        if !begins_with_dot && self.eat('.') {
            self.digits();
        }
        let mut is_valid = true;
        if self.eat('e') || self.eat('E') {
            let _ = self.eat('+') || self.eat('-');
            is_valid = self.digits();
        }
        if self.peek().is_some_and(is_word_part) {
            is_valid = false;
            while self.peek().is_some_and(is_word_part) {
                self.bump();
            }
        }
        let literal = self
            .sql
            .get(start..self.offset)
            .unwrap_or_default()
            .to_string();
        if is_valid {
            Ok(TokenKind::Number(literal))
        } else {
            Err(SqlError::InvalidNumber {
                literal,
                position: self.position(start),
            })
        }
    }

    /// Moves past a run of ASCII digits; whether there was any.
    fn digits(&mut self) -> bool {
        let start = self.offset;
        while self.peek().is_some_and(|next| next.is_ascii_digit()) {
            self.bump();
        }
        self.offset > start
    }

    /// The rest of a word that began at `start`. Only `A`-`Z` are folded, as PostgreSQL does:
    /// other letters keep their case.
    fn word(&mut self, start: usize) -> TokenKind {
        while self.peek().is_some_and(is_word_part) {
            self.bump();
        }
        let text = self
            .sql
            .get(start..self.offset)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let keyword = Keyword::from_word(&text);
        TokenKind::Word { text, keyword }
    }

    fn quoted_identifier(&mut self, start: usize) -> Result<TokenKind, SqlError> {
        let position = self.position(start);
        match self.quoted_text('"') {
            None => Err(SqlError::UnterminatedQuotedIdentifier { position }),
            Some(name) if name.is_empty() => Err(SqlError::EmptyQuotedIdentifier { position }),
            Some(name) => Ok(TokenKind::QuotedIdentifier(name)),
        }
    }

    fn string(&mut self, start: usize) -> Result<TokenKind, SqlError> {
        self.quoted_text('\'')
            .map(TokenKind::String)
            .ok_or(SqlError::UnterminatedString {
                position: self.position(start),
            })
    }

    /// The text up to the `quote` that closes it, a doubled quote standing for one; `None`
    /// when the input ends first.
    fn quoted_text(&mut self, quote: char) -> Option<String> {
        let mut text = String::new();
        loop {
            match self.bump()? {
                character if character == quote && self.eat(quote) => text.push(quote),
                character if character == quote => return Some(text),
                character => text.push(character),
            }
        }
    }

    fn skip_whitespace_and_comments(&mut self) -> Result<(), SqlError> {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.bump();
            }
            if self.is_next("--") {
                while self.bump().is_some_and(|character| character != '\n') {}
            } else if self.is_next("/*") {
                self.skip_block_comment()?;
            } else {
                return Ok(());
            }
        }
    }

    /// Block comments nest, as in the standard and PostgreSQL: `/* a /* b */ c */` is one.
    fn skip_block_comment(&mut self) -> Result<(), SqlError> {
        let position = self.position(self.offset);
        let mut depth = 0_usize;
        loop {
            if self.is_next("/*") {
                self.offset += "/*".len();
                depth += 1;
            } else if self.is_next("*/") {
                self.offset += "*/".len();
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            } else if self.bump().is_none() {
                return Err(SqlError::UnterminatedComment { position });
            }
        }
    }

    fn is_next(&self, text: &str) -> bool {
        self.sql
            .get(self.offset..)
            .is_some_and(|rest| rest.starts_with(text))
    }

    fn peek(&self) -> Option<char> {
        self.sql.get(self.offset..)?.chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.offset += character.len_utf8();
        Some(character)
    }

    /// Moves past `expected` if it is the next character.
    fn eat(&mut self, expected: char) -> bool {
        let is_next = self.peek() == Some(expected);
        if is_next {
            self.bump();
        }
        is_next
    }

    fn position(&self, offset: usize) -> usize {
        character_position(self.sql, offset)
    }
}

/// The character at byte `offset` of `sql`, counting from 1, as error messages show it.
pub(super) fn character_position(sql: &str, offset: usize) -> usize {
    sql.get(..offset).map_or(0, |before| before.chars().count()) + 1
}

fn is_word_start(character: char) -> bool {
    character.is_alphabetic() || character == '_'
}

fn is_word_part(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind::*;

    fn kinds(sql: &str) -> Vec<TokenKind> {
        tokenize(sql)
            .unwrap()
            .into_iter()
            .map(|token| token.kind)
            .collect()
    }

    fn lex_err(sql: &str) -> SqlError {
        tokenize(sql).unwrap_err()
    }

    #[test]
    fn operators_and_punctuation_should_become_their_tokens() {
        let sql = "+ - * / % || = <> < <= > >= , ; . ( )";

        let output = kinds(sql);

        assert_eq!(
            output,
            [
                Plus, Minus, Star, Slash, Percent, Concat, Eq, NotEq, Lt, LtEq, Gt, GtEq, Comma,
                Semicolon, Dot, LeftParen, RightParen,
            ]
        );
    }

    #[test]
    fn bang_equals_should_be_the_same_token_as_not_equals() {
        assert_eq!(kinds("!="), [NotEq]);
        assert_eq!(kinds("<>"), [NotEq]);
    }

    #[test]
    fn operators_should_not_need_spaces_between_them() {
        assert_eq!(kinds("<=>=<>="), [LtEq, GtEq, NotEq, Eq]);
        assert_eq!(kinds("(||)"), [LeftParen, Concat, RightParen]);
    }

    #[test]
    fn empty_or_blank_input_should_give_no_tokens() {
        assert!(kinds("").is_empty());
        assert!(kinds(" \t\r\n\u{a0}").is_empty());
    }

    #[test]
    fn spans_should_point_to_the_source_text() {
        let tokens = tokenize(" <= ,\n(").unwrap();

        let spans: Vec<(usize, usize)> = tokens
            .iter()
            .map(|token| (token.span.start, token.span.end))
            .collect();

        assert_eq!(spans, [(1, 3), (4, 5), (6, 7)]);
    }

    #[test]
    fn a_lone_bar_or_bang_should_be_an_unexpected_character() {
        assert_eq!(
            lex_err("|"),
            SqlError::UnexpectedCharacter {
                character: '|',
                position: 1
            }
        );
        assert_eq!(
            lex_err("= !"),
            SqlError::UnexpectedCharacter {
                character: '!',
                position: 3
            }
        );
        assert_eq!(
            lex_err("|||"),
            SqlError::UnexpectedCharacter {
                character: '|',
                position: 3
            }
        );
    }

    fn word(text: &str, keyword: Option<Keyword>) -> TokenKind {
        Word {
            text: text.to_string(),
            keyword,
        }
    }

    #[test]
    fn words_should_be_folded_to_lowercase_and_marked_as_keywords() {
        let output = kinds("SELECT Select select");

        assert_eq!(output, vec![word("select", Some(Keyword::Select)); 3]);
    }

    #[test]
    fn a_word_that_is_not_a_keyword_should_have_none() {
        assert_eq!(kinds("Users"), [word("users", None)]);
    }

    #[test]
    fn words_should_take_letters_digits_and_underscores() {
        assert_eq!(
            kinds("_tmp1 col_2,x"),
            [
                word("_tmp1", None),
                word("col_2", None),
                Comma,
                word("x", None)
            ]
        );
    }

    #[test]
    fn only_ascii_letters_should_be_folded() {
        assert_eq!(kinds("AÇÃO"), [word("aÇÃo", None)]);
        assert_eq!(kinds("Émile"), [word("Émile", None)]);
    }

    #[test]
    fn keywords_should_round_trip_through_their_names() {
        for (name, keyword) in KEYWORDS {
            assert_eq!(keyword.to_string(), name.to_ascii_uppercase());
            assert_eq!(kinds(name), [word(name, Some(keyword))]);
        }
    }

    #[test]
    fn words_that_only_resemble_keywords_should_not_be_keywords() {
        for text in ["selects", "sel", "_select", "nullss", "a", "zzz"] {
            assert_eq!(kinds(text), [word(text, None)], "{text}");
        }
    }

    #[test]
    fn the_keyword_table_should_be_sorted_for_binary_search() {
        assert!(KEYWORDS.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }

    fn quoted(name: &str) -> TokenKind {
        QuotedIdentifier(name.to_string())
    }

    #[test]
    fn quoted_identifiers_should_keep_their_case_and_never_be_keywords() {
        assert_eq!(kinds("\"Users\""), [quoted("Users")]);
        assert_eq!(kinds("\"select\""), [quoted("select")]);
        assert_eq!(kinds("\"two words\""), [quoted("two words")]);
    }

    #[test]
    fn a_doubled_quote_in_a_quoted_identifier_should_stand_for_one() {
        assert_eq!(kinds("\"a\"\"b\""), [quoted("a\"b")]);
    }

    #[test]
    fn quoted_identifiers_should_not_need_spaces_around_them() {
        assert_eq!(kinds("\"a\".\"b\""), [quoted("a"), Dot, quoted("b")]);
    }

    #[test]
    fn an_empty_quoted_identifier_should_fail() {
        assert_eq!(
            lex_err("a = \"\""),
            SqlError::EmptyQuotedIdentifier { position: 5 }
        );
    }

    #[test]
    fn an_unterminated_quoted_identifier_should_fail_at_its_opening_quote() {
        assert_eq!(
            lex_err("select \"abc"),
            SqlError::UnterminatedQuotedIdentifier { position: 8 }
        );
        assert_eq!(
            lex_err("\"a\"\""),
            SqlError::UnterminatedQuotedIdentifier { position: 1 }
        );
    }

    fn string(text: &str) -> TokenKind {
        String(text.to_string())
    }

    #[test]
    fn a_doubled_apostrophe_in_a_string_should_stand_for_one() {
        assert_eq!(kinds("'it''s'"), [string("it's")]);
        assert_eq!(kinds("''"), [string("")]);
        assert_eq!(kinds("''''"), [string("'")]);
    }

    #[test]
    fn strings_should_hold_any_text_across_lines() {
        assert_eq!(
            kinds("'a\nb -- c /* d \"e'"),
            [string("a\nb -- c /* d \"e")]
        );
        assert_eq!(kinds("'ação €'"), [string("ação €")]);
    }

    #[test]
    fn an_unterminated_string_should_fail_at_its_opening_quote() {
        assert_eq!(
            lex_err("x = 'abc"),
            SqlError::UnterminatedString { position: 5 }
        );
        assert_eq!(
            lex_err("'it''"),
            SqlError::UnterminatedString { position: 1 }
        );
    }

    fn number(text: &str) -> TokenKind {
        Number(text.to_string())
    }

    #[test]
    fn numbers_should_keep_their_text() {
        let output = kinds("42 1. .5 1.5 1e10 2.5E-3 6.02e+23 007");

        assert_eq!(
            output,
            ["42", "1.", ".5", "1.5", "1e10", "2.5E-3", "6.02e+23", "007"].map(number)
        );
    }

    #[test]
    fn a_sign_should_be_an_operator_not_part_of_the_number() {
        assert_eq!(kinds("-1"), [Minus, number("1")]);
        assert_eq!(kinds("1-2"), [number("1"), Minus, number("2")]);
        assert_eq!(kinds("1e-2-3"), [number("1e-2"), Minus, number("3")]);
    }

    #[test]
    fn a_dot_should_start_a_number_only_before_a_digit() {
        assert_eq!(kinds("t.c1"), [word("t", None), Dot, word("c1", None)]);
        assert_eq!(kinds("t.5"), [word("t", None), number(".5")]);
        assert_eq!(kinds("1..2"), [number("1."), number(".2")]);
    }

    #[test]
    fn numbers_with_a_bare_fraction_point_should_take_an_exponent() {
        assert_eq!(kinds("1.e5 1.E-2"), [number("1.e5"), number("1.E-2")]);
    }

    #[test]
    fn a_number_followed_by_a_dot_and_letters_should_be_invalid() {
        assert_eq!(
            lex_err("1.a"),
            SqlError::InvalidNumber {
                literal: "1.a".to_string(),
                position: 1
            }
        );
    }

    #[test]
    fn control_characters_should_be_unexpected() {
        assert_eq!(
            lex_err("a\0"),
            SqlError::UnexpectedCharacter {
                character: '\0',
                position: 2
            }
        );
        assert_eq!(
            lex_err("é \u{7}"),
            SqlError::UnexpectedCharacter {
                character: '\u{7}',
                position: 3
            }
        );
    }

    #[test]
    fn a_number_with_trailing_letters_or_an_empty_exponent_should_fail() {
        let cases = [
            ("12abc", "12abc", 1),
            ("x = 3x", "3x", 5),
            ("1.5_", "1.5_", 1),
            ("1e", "1e", 1),
            ("1e+", "1e+", 1),
            ("2.5E-x", "2.5E-x", 1),
        ];

        for (sql, literal, position) in cases {
            assert_eq!(
                lex_err(sql),
                SqlError::InvalidNumber {
                    literal: literal.to_string(),
                    position
                },
                "{sql}"
            );
        }
    }

    #[test]
    fn line_comments_should_run_to_the_end_of_the_line() {
        assert_eq!(kinds("a -- b c\nd"), [word("a", None), word("d", None)]);
        assert_eq!(kinds("a -- no newline at the end"), [word("a", None)]);
        assert_eq!(kinds("1--2"), [number("1")]);
        assert_eq!(
            kinds("a - -b"),
            [word("a", None), Minus, Minus, word("b", None)]
        );
    }

    #[test]
    fn block_comments_should_be_skipped_and_may_nest() {
        assert_eq!(kinds("a /* b */ c"), [word("a", None), word("c", None)]);
        assert_eq!(kinds("a/**/c"), [word("a", None), word("c", None)]);
        assert_eq!(
            kinds("a /* x /* y */ z */ c"),
            [word("a", None), word("c", None)]
        );
        assert_eq!(kinds("/* * / -- */ x"), [word("x", None)]);
        assert_eq!(kinds("*/"), [Star, Slash]);
    }

    #[test]
    fn an_unterminated_block_comment_should_fail_at_its_opening() {
        assert_eq!(
            lex_err("a /* b"),
            SqlError::UnterminatedComment { position: 3 }
        );
        assert_eq!(
            lex_err("/* /* */"),
            SqlError::UnterminatedComment { position: 1 }
        );
    }

    #[test]
    fn comment_markers_should_not_count_inside_quotes_and_quotes_not_inside_comments() {
        assert_eq!(kinds("\"a--b\" \"/*\""), [quoted("a--b"), quoted("/*")]);
        assert_eq!(kinds("'--' '/*'"), [string("--"), string("/*")]);
        assert_eq!(
            kinds("a -- it's\nb /* \" */ c"),
            [word("a", None), word("b", None), word("c", None)]
        );
    }

    #[test]
    fn error_positions_should_count_characters_not_bytes() {
        // Two no-break spaces, two bytes each.
        let err = lex_err("\u{a0}\u{a0}€");

        assert_eq!(
            err,
            SqlError::UnexpectedCharacter {
                character: '€',
                position: 3
            }
        );
        assert_eq!(err.sqlstate(), "42601");
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

        fn pick<T: Copy>(&mut self, items: &[T]) -> T {
            items[self.below(items.len())]
        }

        fn text(
            &mut self,
            alphabet: &[char],
            min_len: usize,
            max_len: usize,
        ) -> std::string::String {
            let len = min_len + self.below(max_len - min_len + 1);
            (0..len).map(|_| self.pick(alphabet)).collect()
        }

        /// A token, and source text that spells it.
        fn token(&mut self) -> (TokenKind, std::string::String) {
            const SYMBOLS: [(&str, TokenKind); 18] = [
                ("+", Plus),
                ("-", Minus),
                ("*", Star),
                ("/", Slash),
                ("%", Percent),
                ("||", Concat),
                ("=", Eq),
                ("<>", NotEq),
                ("!=", NotEq),
                ("<", Lt),
                ("<=", LtEq),
                (">", Gt),
                (">=", GtEq),
                (",", Comma),
                (";", Semicolon),
                (".", Dot),
                ("(", LeftParen),
                (")", RightParen),
            ];
            const NUMBERS: [&str; 8] = ["0", "42", "1.", ".5", "3.25", "1e9", "2.5E-3", "6.02e+23"];
            const QUOTED: [char; 9] = ['a', 'Z', ' ', '"', '\'', '-', '/', '*', 'ç'];
            match self.below(6) {
                0 => {
                    let (source, kind) = SYMBOLS[self.below(SYMBOLS.len())].clone();
                    (kind, source.to_string())
                }
                1 => {
                    let (name, keyword) = self.pick(&KEYWORDS);
                    let source: std::string::String = name
                        .chars()
                        .map(|letter| match self.below(2) {
                            0 => letter.to_ascii_uppercase(),
                            _ => letter,
                        })
                        .collect();
                    (word(name, Some(keyword)), source)
                }
                2 => {
                    let source = format!("w{}", self.text(&['a', 'B', '_', '7', 'ã'], 0, 6));
                    let text = source.to_ascii_lowercase();
                    let keyword = Keyword::from_word(&text);
                    (Word { text, keyword }, source)
                }
                3 => {
                    let name = self.text(&QUOTED, 1, 8);
                    (quoted(&name), format!("\"{}\"", name.replace('"', "\"\"")))
                }
                4 => {
                    let text = self.text(&['x', ' ', '\'', '\n', '"', '-', '*', '€'], 0, 8);
                    (string(&text), format!("'{}'", text.replace('\'', "''")))
                }
                _ => {
                    let literal = self.pick(&NUMBERS);
                    (number(literal), literal.to_string())
                }
            }
        }

        /// Whitespace or comments, always starting with whitespace so that no two tokens merge.
        fn separator(&mut self) -> &'static str {
            self.pick(&[
                " ",
                "\n",
                "\t ",
                " /* c */ ",
                " -- c\n",
                " /* a /* b */ */\n",
            ])
        }
    }

    #[test]
    fn random_tokens_written_out_should_lex_back_with_their_spans() {
        let mut rng = Rng(0x94D0_49BB_1331_11EB);

        for round in 0..2_000 {
            let mut sql = std::string::String::new();
            let mut expected = Vec::new();
            for _ in 0..rng.below(12) {
                sql.push_str(rng.separator());
                let (kind, source) = rng.token();
                let start = sql.len();
                sql.push_str(&source);
                expected.push((kind, start..sql.len()));
            }

            let tokens =
                tokenize(&sql).unwrap_or_else(|err| panic!("round {round}: {err} in {sql:?}"));

            let actual: Vec<(TokenKind, std::ops::Range<usize>)> = tokens
                .into_iter()
                .map(|token| (token.kind, token.span.start..token.span.end))
                .collect();
            assert_eq!(actual, expected, "round {round}: {sql:?}");
        }
    }

    #[test]
    fn random_text_should_never_panic_and_spans_should_hold_their_tokens() {
        const ALPHABET: [char; 26] = [
            'a', 'Z', '_', '1', '.', 'e', '+', '-', '*', '/', '|', '!', '<', '>', '=', '\'', '"',
            ' ', '\n', '(', ')', ';', 'é', '€', '𝄞', '\u{a0}',
        ];
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);

        for _ in 0..20_000 {
            let sql = rng.text(&ALPHABET, 0, 40);

            match tokenize(&sql) {
                Ok(tokens) => {
                    let mut previous_end = 0;
                    for token in tokens {
                        let Span { start, end } = token.span;
                        assert!(
                            previous_end <= start && start < end && end <= sql.len(),
                            "{sql:?}"
                        );
                        assert!(
                            sql.is_char_boundary(start) && sql.is_char_boundary(end),
                            "{sql:?}"
                        );
                        let alone = tokenize(&sql[start..end]).unwrap();
                        assert_eq!(alone.len(), 1, "{sql:?}");
                        assert_eq!(alone[0].kind, token.kind, "{sql:?}");
                        previous_end = end;
                    }
                }
                Err(err) => {
                    let message = err.to_string();
                    assert!(message.contains(" at character "), "{message}");
                    assert_eq!(err.sqlstate(), "42601");
                }
            }
        }
    }
}
