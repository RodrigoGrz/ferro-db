//! The parser: tokens as statements, by recursive descent, one function per rule of the
//! grammar: `SELECT`, `CREATE TABLE`, `CREATE [UNIQUE] INDEX`, `INSERT`, `UPDATE`, `DELETE` and
//! `EXPLAIN SELECT`. A `SELECT` is `[ALL] items [FROM tables] [WHERE] [GROUP BY] [HAVING]
//! [ORDER BY] [LIMIT | FETCH] [OFFSET]`, joins read left to right and a comma being a
//! `CROSS JOIN`.
//!
//! Expressions use precedence climbing over `Precedence`, PostgreSQL's order: `OR`, `AND`,
//! `NOT`, `IS`, comparisons (which do not associate: `a < b < c` is an error), `BETWEEN` and
//! `IN`, `||`, `+ -`, `* / %`, then a prefix sign. Names are not resolved and numbers keep their
//! text: that is the planner's job. A keyword names something only when `is_reserved` says it
//! may; after `AS`, a column label may be any word, as in PostgreSQL. Expressions and table
//! references nest at most `MAX_NESTING` levels deep, each operator or join of a chain counting
//! as one (54001 beyond), so no statement can exhaust the stack.

use std::num::NonZeroU16;

use crate::types::DataType;

use super::lexer::character_position;
use super::{
    Assignment, BinaryOp, BooleanTest, ColumnDef, Expr, FunctionArgs, JoinKind, Keyword, Literal,
    OrderItem, Select, SelectItem, SqlError, Statement, TableRef, Token, TokenKind, UnaryOp,
    tokenize,
};

/// How deep the tree of a statement may be: a nested expression or table reference is a level,
/// and so is each operator or join of a chain, since `a + b + c` is `(a + b) + c`. The parser,
/// `Display`, dropping the tree, the binder and evaluation all recurse once per level, so a
/// limit here keeps any statement from exhausting the stack.
const MAX_NESTING: usize = 200;

/// Every statement of the text, separated by `;` (empty statements are skipped).
pub fn parse(sql: &str) -> Result<Vec<Statement>, SqlError> {
    let mut parser = Parser {
        sql,
        tokens: tokenize(sql)?,
        next: 0,
        depth: 0,
    };
    parser.statements()
}

/// A cursor over the tokens of one text; `next` is the index of the first unread token and
/// `depth` how deep in the tree the parser is (see `MAX_NESTING`).
struct Parser<'a> {
    sql: &'a str,
    tokens: Vec<Token>,
    next: usize,
    depth: usize,
}

impl Parser<'_> {
    fn statements(&mut self) -> Result<Vec<Statement>, SqlError> {
        let mut statements = Vec::new();
        loop {
            while self.eat(&TokenKind::Semicolon) {}
            if self.peek().is_none() {
                return Ok(statements);
            }
            statements.push(self.statement()?);
            if self.peek().is_some() {
                self.expect(&TokenKind::Semicolon, "\";\"")?;
            }
        }
    }

    fn statement(&mut self) -> Result<Statement, SqlError> {
        match self.peek_keyword() {
            Some(Keyword::Select) => {
                self.next += 1;
                Ok(Statement::Select(Box::new(self.select()?)))
            }
            Some(Keyword::Create) => {
                self.next += 1;
                self.create()
            }
            Some(Keyword::Insert) => {
                self.next += 1;
                self.insert()
            }
            Some(Keyword::Update) => {
                self.next += 1;
                self.update()
            }
            Some(Keyword::Delete) => {
                self.next += 1;
                self.delete()
            }
            Some(Keyword::Explain) => {
                self.next += 1;
                self.explain()
            }
            _ => Err(self.error_here("a statement")),
        }
    }

    /// The rest of `EXPLAIN SELECT …`, after `EXPLAIN`; the plan of another statement is valid
    /// SQL elsewhere, not yet here.
    fn explain(&mut self) -> Result<Statement, SqlError> {
        match self.peek_keyword() {
            Some(Keyword::Select) => {
                self.next += 1;
                Ok(Statement::Explain(Box::new(self.select()?)))
            }
            Some(
                Keyword::Create
                | Keyword::Insert
                | Keyword::Update
                | Keyword::Delete
                | Keyword::Explain,
            ) => Err(self.unsupported("EXPLAIN of statements other than SELECT")),
            _ => Err(self.error_here("SELECT")),
        }
    }

    /// The rest of `INSERT INTO table [(columns)] VALUES (…) [, (…) …]`, after `INSERT`.
    fn insert(&mut self) -> Result<Statement, SqlError> {
        self.expect_keyword(Keyword::Into)?;
        let table = self.name("a table name")?;
        let columns = match self.eat(&TokenKind::LeftParen) {
            true => {
                let columns = self.comma_list(|parser| parser.name("a column name"))?;
                self.expect(&TokenKind::RightParen, "\",\" or \")\"")?;
                columns
            }
            false => Vec::new(),
        };
        if self.peek_keyword() == Some(Keyword::Select) {
            return Err(self.unsupported("INSERT … SELECT"));
        }
        self.expect_keyword(Keyword::Values)?;
        let rows = self.comma_list(Self::row)?;
        Ok(Statement::Insert {
            table,
            columns,
            rows,
        })
    }

    /// `(expr [, …])`: one row of `VALUES`.
    fn row(&mut self) -> Result<Vec<Expr>, SqlError> {
        self.expect(&TokenKind::LeftParen, "\"(\"")?;
        let values = self.comma_list(Self::expression)?;
        self.expect(&TokenKind::RightParen, "\",\" or \")\"")?;
        Ok(values)
    }

    /// The rest of `UPDATE table SET column = expr [, …] [WHERE expr]`, after `UPDATE`.
    fn update(&mut self) -> Result<Statement, SqlError> {
        let table = self.name("a table name")?;
        self.expect_keyword(Keyword::Set)?;
        let assignments = self.comma_list(Self::assignment)?;
        let selection = self.optional_clause(Keyword::Where)?;
        Ok(Statement::Update {
            table,
            assignments,
            selection,
        })
    }

    fn assignment(&mut self) -> Result<Assignment, SqlError> {
        let column = self.name("a column name")?;
        self.expect(&TokenKind::Eq, "\"=\"")?;
        let value = self.expression()?;
        Ok(Assignment { column, value })
    }

    /// The rest of `DELETE FROM table [WHERE expr]`, after `DELETE`.
    fn delete(&mut self) -> Result<Statement, SqlError> {
        self.expect_keyword(Keyword::From)?;
        let table = self.name("a table name")?;
        let selection = self.optional_clause(Keyword::Where)?;
        Ok(Statement::Delete { table, selection })
    }

    /// The rest of `CREATE TABLE` or `CREATE [UNIQUE] INDEX`, after `CREATE`.
    fn create(&mut self) -> Result<Statement, SqlError> {
        if self.eat_keyword(Keyword::Table) {
            return self.create_table();
        }
        if self.eat_keyword(Keyword::Unique) {
            self.expect_keyword(Keyword::Index)?;
            return self.create_index(true);
        }
        if self.eat_keyword(Keyword::Index) {
            return self.create_index(false);
        }
        Err(self.error_here("TABLE, INDEX or UNIQUE INDEX"))
    }

    fn create_table(&mut self) -> Result<Statement, SqlError> {
        let name = self.name("a table name")?;
        self.expect(&TokenKind::LeftParen, "\"(\"")?;
        let columns = self.comma_list(Self::column_def)?;
        self.expect(&TokenKind::RightParen, "\",\" or \")\"")?;
        Ok(Statement::CreateTable { name, columns })
    }

    /// `name type [NOT NULL | NULL]`. Column constraints other than nullability are the
    /// Constraints section's, still to come.
    fn column_def(&mut self) -> Result<ColumnDef, SqlError> {
        let name = self.name("a column name")?;
        let data_type = self.data_type()?;
        let is_not_null = if self.eat_keyword(Keyword::Not) {
            self.expect_keyword(Keyword::Null)?;
            true
        } else {
            self.eat_keyword(Keyword::Null);
            false
        };
        match self.peek_keyword() {
            Some(Keyword::Primary) => Err(self.unsupported("PRIMARY KEY")),
            Some(Keyword::Unique) => Err(self.unsupported("UNIQUE columns")),
            _ => Ok(ColumnDef {
                name,
                data_type,
                is_not_null,
            }),
        }
    }

    /// The rest of `CREATE [UNIQUE] INDEX name ON table (column)`, after `INDEX`.
    fn create_index(&mut self, is_unique: bool) -> Result<Statement, SqlError> {
        let name = self.name("an index name")?;
        self.expect_keyword(Keyword::On)?;
        let table = self.name("a table name")?;
        self.expect(&TokenKind::LeftParen, "\"(\"")?;
        let column = self.name("a column name")?;
        if self.peek_kind() == Some(&TokenKind::Comma) {
            return Err(self.unsupported("indexes on more than one column"));
        }
        self.expect(&TokenKind::RightParen, "\")\"")?;
        Ok(Statement::CreateIndex {
            name,
            table,
            column,
            is_unique,
        })
    }

    /// The rest of a `SELECT`, after the keyword.
    fn select(&mut self) -> Result<Select, SqlError> {
        if self.peek_keyword() == Some(Keyword::Distinct) {
            return Err(self.unsupported("SELECT DISTINCT"));
        }
        self.eat_keyword(Keyword::All);
        let projection = self.comma_list(Self::select_item)?;
        let from = match self.eat_keyword(Keyword::From) {
            true => Some(self.table_expression()?),
            false => None,
        };
        let selection = self.optional_clause(Keyword::Where)?;
        let group_by = match self.eat_keyword(Keyword::Group) {
            true => {
                self.expect_keyword(Keyword::By)?;
                self.comma_list(Self::expression)?
            }
            false => Vec::new(),
        };
        let having = self.optional_clause(Keyword::Having)?;
        let order_by = match self.eat_keyword(Keyword::Order) {
            true => {
                self.expect_keyword(Keyword::By)?;
                self.comma_list(Self::order_item)?
            }
            false => Vec::new(),
        };
        let (limit, offset) = self.row_counts()?;
        Ok(Select {
            projection,
            from,
            selection,
            group_by,
            having,
            order_by,
            limit,
            offset,
        })
    }

    /// The expression after `keyword`, if the clause is there.
    fn optional_clause(&mut self, keyword: Keyword) -> Result<Option<Expr>, SqlError> {
        match self.eat_keyword(keyword) {
            true => Ok(Some(self.expression()?)),
            false => Ok(None),
        }
    }

    fn select_item(&mut self) -> Result<SelectItem, SqlError> {
        if self.eat(&TokenKind::Star) {
            return Ok(SelectItem::Wildcard);
        }
        let is_qualified_wildcard = self.is_name()
            && self.kind_at(1) == Some(&TokenKind::Dot)
            && self.kind_at(2) == Some(&TokenKind::Star);
        if is_qualified_wildcard {
            let table = self.name("a table name")?;
            self.next += 2;
            return Ok(SelectItem::QualifiedWildcard(table));
        }
        let expr = self.expression()?;
        let alias = self.alias(true)?;
        Ok(SelectItem::Expr { expr, alias })
    }

    /// `AS name`, or a bare name. After `AS`, a column label may be any word, keywords
    /// included (`SELECT 1 AS from`), as in PostgreSQL; a bare alias is never a reserved word.
    fn alias(&mut self, may_be_keyword: bool) -> Result<Option<String>, SqlError> {
        if self.eat_keyword(Keyword::As) {
            if let (true, Some(TokenKind::Word { text, .. })) = (may_be_keyword, self.peek_kind()) {
                let label = text.clone();
                self.next += 1;
                return Ok(Some(label));
            }
            return Ok(Some(self.name("an alias")?));
        }
        match self.is_name() {
            true => Ok(Some(self.name("an alias")?)),
            false => Ok(None),
        }
    }

    /// Joined tables separated by commas; a comma is a `CROSS JOIN`.
    fn table_expression(&mut self) -> Result<TableRef, SqlError> {
        self.within_depth(|parser| {
            let mut from = parser.joined_table()?;
            while parser.eat(&TokenKind::Comma) {
                let right = parser.joined_table()?;
                from = TableRef::Join {
                    left: Box::new(from),
                    right: Box::new(right),
                    kind: JoinKind::Cross,
                    on: None,
                };
                parser.deeper()?;
            }
            Ok(from)
        })
    }

    /// Tables joined left to right: `a JOIN b ON x JOIN c ON y` is `(a JOIN b) JOIN c`.
    fn joined_table(&mut self) -> Result<TableRef, SqlError> {
        self.within_depth(|parser| {
            let mut left = parser.table_primary()?;
            while let Some(kind) = parser.join_kind()? {
                let right = parser.table_primary()?;
                let on = match kind {
                    JoinKind::Cross => None,
                    _ => {
                        parser.expect_keyword(Keyword::On)?;
                        Some(parser.expression()?)
                    }
                };
                left = TableRef::Join {
                    left: Box::new(left),
                    right: Box::new(right),
                    kind,
                    on,
                };
                parser.deeper()?;
            }
            Ok(left)
        })
    }

    /// `[INNER] JOIN`, `LEFT | RIGHT | FULL [OUTER] JOIN`, `CROSS JOIN`, or `None`.
    fn join_kind(&mut self) -> Result<Option<JoinKind>, SqlError> {
        let kind = match self.peek_keyword() {
            Some(Keyword::Join) => {
                self.next += 1;
                return Ok(Some(JoinKind::Inner));
            }
            Some(Keyword::Inner) => JoinKind::Inner,
            Some(Keyword::Cross) => JoinKind::Cross,
            Some(Keyword::Left) => JoinKind::Left,
            Some(Keyword::Right) => JoinKind::Right,
            Some(Keyword::Full) => JoinKind::Full,
            _ => return Ok(None),
        };
        self.next += 1;
        if matches!(kind, JoinKind::Left | JoinKind::Right | JoinKind::Full) {
            self.eat_keyword(Keyword::Outer);
        }
        self.expect_keyword(Keyword::Join)?;
        Ok(Some(kind))
    }

    /// A table with its alias, or joined tables in parentheses.
    fn table_primary(&mut self) -> Result<TableRef, SqlError> {
        self.nested(Self::table_primary_at_depth)
    }

    fn table_primary_at_depth(&mut self) -> Result<TableRef, SqlError> {
        if self.eat(&TokenKind::LeftParen) {
            let table = self.joined_table()?;
            self.expect(&TokenKind::RightParen, "\")\"")?;
            return Ok(table);
        }
        let name = self.name("a table name")?;
        let alias = self.alias(false)?;
        Ok(TableRef::Table { name, alias })
    }

    fn order_item(&mut self) -> Result<OrderItem, SqlError> {
        let expr = self.expression()?;
        let is_descending = match self.eat_keyword(Keyword::Desc) {
            true => true,
            false => {
                self.eat_keyword(Keyword::Asc);
                false
            }
        };
        let nulls_first = match self.eat_keyword(Keyword::Nulls) {
            true if self.eat_keyword(Keyword::First) => Some(true),
            true if self.eat_keyword(Keyword::Last) => Some(false),
            true => return Err(self.error_here("FIRST or LAST")),
            false => None,
        };
        Ok(OrderItem {
            expr,
            is_descending,
            nulls_first,
        })
    }

    /// `LIMIT`, `OFFSET` and `FETCH`, in any order, each once; `LIMIT` and `FETCH` both set
    /// the count, so only one of them may appear.
    fn row_counts(&mut self) -> Result<(Option<Expr>, Option<Expr>), SqlError> {
        let (mut limit, mut offset) = (None, None);
        let (mut has_count, mut has_offset) = (false, false);
        loop {
            match self.peek_keyword() {
                Some(Keyword::Limit | Keyword::Fetch) if has_count => {
                    return Err(self.error_here("only one LIMIT or FETCH"));
                }
                Some(Keyword::Offset) if has_offset => {
                    return Err(self.error_here("only one OFFSET"));
                }
                Some(Keyword::Limit) => {
                    self.next += 1;
                    has_count = true;
                    if !self.eat_keyword(Keyword::All) {
                        limit = Some(self.expression()?);
                    }
                }
                Some(Keyword::Fetch) => {
                    self.next += 1;
                    has_count = true;
                    limit = Some(self.fetch_count()?);
                }
                Some(Keyword::Offset) => {
                    self.next += 1;
                    has_offset = true;
                    offset = Some(self.expression()?);
                    let _ = self.eat_keyword(Keyword::Row) || self.eat_keyword(Keyword::Rows);
                }
                _ => return Ok((limit, offset)),
            }
        }
    }

    /// The rest of `FETCH {FIRST | NEXT} [count] {ROW | ROWS} ONLY`; the count is 1 when
    /// left out.
    fn fetch_count(&mut self) -> Result<Expr, SqlError> {
        if !(self.eat_keyword(Keyword::First) || self.eat_keyword(Keyword::Next)) {
            return Err(self.error_here("FIRST or NEXT"));
        }
        let count = match self.peek_keyword() {
            Some(Keyword::Row | Keyword::Rows) => Expr::Literal(Literal::Number("1".to_string())),
            _ => self.expression()?,
        };
        if !(self.eat_keyword(Keyword::Row) || self.eat_keyword(Keyword::Rows)) {
            return Err(self.error_here("ROW or ROWS"));
        }
        self.expect_keyword(Keyword::Only)?;
        Ok(count)
    }

    fn expression(&mut self) -> Result<Expr, SqlError> {
        self.expression_from(Precedence::Or)
    }

    /// Precedence climbing: an operand, then every operator that binds at least as tightly
    /// as `min`, its right side read one level tighter (left associativity).
    fn expression_from(&mut self, min: Precedence) -> Result<Expr, SqlError> {
        self.nested(|parser| parser.climb(min))
    }

    fn climb(&mut self, min: Precedence) -> Result<Expr, SqlError> {
        self.within_depth(|parser| {
            let mut left = parser.prefix()?;
            while let Some(precedence) = parser.infix_precedence() {
                if precedence < min {
                    break;
                }
                left = parser.infix(left, precedence)?;
                parser.deeper()?;
            }
            Ok(left)
        })
    }

    fn prefix(&mut self) -> Result<Expr, SqlError> {
        let (op, operand_precedence) = if self.eat_keyword(Keyword::Not) {
            (UnaryOp::Not, Precedence::Not)
        } else if self.eat(&TokenKind::Minus) {
            (UnaryOp::Minus, Precedence::Sign)
        } else if self.eat(&TokenKind::Plus) {
            (UnaryOp::Plus, Precedence::Sign)
        } else {
            return self.operand();
        };
        let expr = self.expression_from(operand_precedence)?;
        Ok(Expr::Unary {
            op,
            expr: Box::new(expr),
        })
    }

    /// The precedence of the next token as an operator after an operand, if it is one. A
    /// `NOT` there can only start `NOT BETWEEN` or `NOT IN`.
    fn infix_precedence(&self) -> Option<Precedence> {
        match self.peek_keyword() {
            Some(Keyword::Is) => Some(Precedence::Is),
            Some(Keyword::Not | Keyword::Between | Keyword::In) => Some(Precedence::Range),
            _ => self.binary_operator().map(|(_, precedence)| precedence),
        }
    }

    /// The next token as a binary operator, with its precedence.
    fn binary_operator(&self) -> Option<(BinaryOp, Precedence)> {
        if let Some(keyword) = self.peek_keyword() {
            return match keyword {
                Keyword::Or => Some((BinaryOp::Or, Precedence::Or)),
                Keyword::And => Some((BinaryOp::And, Precedence::And)),
                _ => None,
            };
        }
        let op = match self.peek_kind()? {
            TokenKind::Eq => BinaryOp::Eq,
            TokenKind::NotEq => BinaryOp::NotEq,
            TokenKind::Lt => BinaryOp::Lt,
            TokenKind::LtEq => BinaryOp::LtEq,
            TokenKind::Gt => BinaryOp::Gt,
            TokenKind::GtEq => BinaryOp::GtEq,
            TokenKind::Concat => BinaryOp::Concat,
            TokenKind::Plus => BinaryOp::Plus,
            TokenKind::Minus => BinaryOp::Minus,
            TokenKind::Star => BinaryOp::Multiply,
            TokenKind::Slash => BinaryOp::Divide,
            TokenKind::Percent => BinaryOp::Modulo,
            _ => return None,
        };
        let precedence = match op {
            BinaryOp::Concat => Precedence::Concat,
            BinaryOp::Plus | BinaryOp::Minus => Precedence::Additive,
            BinaryOp::Multiply | BinaryOp::Divide | BinaryOp::Modulo => Precedence::Multiplicative,
            _ => Precedence::Comparison,
        };
        Some((op, precedence))
    }

    fn infix(&mut self, left: Expr, precedence: Precedence) -> Result<Expr, SqlError> {
        match precedence {
            Precedence::Is => self.is_test(left),
            Precedence::Range => self.range(left),
            Precedence::Comparison => {
                let expr = self.binary(left)?;
                // Comparisons do not associate, as in the standard: `a < b < c` is an error.
                if self.infix_precedence() == Some(Precedence::Comparison) {
                    return Err(self.error_here("an operator that is not a comparison"));
                }
                Ok(expr)
            }
            _ => self.binary(left),
        }
    }

    fn binary(&mut self, left: Expr) -> Result<Expr, SqlError> {
        let Some((op, precedence)) = self.binary_operator() else {
            return Err(self.error_here("an operator"));
        };
        self.next += 1;
        let right = self.expression_from(precedence.tighter())?;
        Ok(Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        })
    }

    /// The rest of `left IS [NOT] …`.
    fn is_test(&mut self, left: Expr) -> Result<Expr, SqlError> {
        self.expect_keyword(Keyword::Is)?;
        let negated = self.eat_keyword(Keyword::Not);
        let expr = Box::new(left);
        let test = match self.peek_keyword() {
            Some(Keyword::Null) => {
                self.next += 1;
                return Ok(Expr::IsNull { expr, negated });
            }
            Some(Keyword::Distinct) => {
                self.next += 1;
                self.expect_keyword(Keyword::From)?;
                let right = self.expression_from(Precedence::Comparison)?;
                return Ok(Expr::IsDistinctFrom {
                    left: expr,
                    right: Box::new(right),
                    negated,
                });
            }
            Some(Keyword::True) => BooleanTest::True,
            Some(Keyword::False) => BooleanTest::False,
            Some(Keyword::Unknown) => BooleanTest::Unknown,
            _ => return Err(self.error_here("NULL, TRUE, FALSE, UNKNOWN or DISTINCT FROM")),
        };
        self.next += 1;
        Ok(Expr::Is {
            expr,
            test,
            negated,
        })
    }

    /// The rest of `left [NOT] BETWEEN low AND high` or `left [NOT] IN (…)`. The bounds are
    /// read above `AND`, so `x BETWEEN 1 AND 2 AND y` ends the BETWEEN at `2`.
    fn range(&mut self, left: Expr) -> Result<Expr, SqlError> {
        let negated = self.eat_keyword(Keyword::Not);
        let expr = Box::new(left);
        if self.eat_keyword(Keyword::Between) {
            let low = self.expression_from(Precedence::Concat)?;
            self.expect_keyword(Keyword::And)?;
            let high = self.expression_from(Precedence::Concat)?;
            return Ok(Expr::Between {
                expr,
                low: Box::new(low),
                high: Box::new(high),
                negated,
            });
        }
        if self.eat_keyword(Keyword::In) {
            self.expect(&TokenKind::LeftParen, "\"(\"")?;
            let list = self.comma_list(Self::expression)?;
            self.expect(&TokenKind::RightParen, "\")\"")?;
            return Ok(Expr::InList {
                expr,
                list,
                negated,
            });
        }
        Err(self.error_here("BETWEEN or IN"))
    }

    /// A literal, a column, a function call, a `CAST` or an expression in parentheses.
    fn operand(&mut self) -> Result<Expr, SqlError> {
        let Some(token) = self.peek() else {
            return Err(self.error_here("an expression"));
        };
        let literal = match &token.kind {
            TokenKind::Number(text) => Some(Literal::Number(text.clone())),
            TokenKind::String(text) => Some(Literal::String(text.clone())),
            TokenKind::Word {
                keyword: Some(Keyword::True),
                ..
            } => Some(Literal::Boolean(true)),
            TokenKind::Word {
                keyword: Some(Keyword::False),
                ..
            } => Some(Literal::Boolean(false)),
            TokenKind::Word {
                keyword: Some(Keyword::Null),
                ..
            } => Some(Literal::Null),
            _ => None,
        };
        if let Some(literal) = literal {
            self.next += 1;
            return Ok(Expr::Literal(literal));
        }
        if let Some(literal) = self.typed_literal() {
            return Ok(Expr::Literal(literal));
        }
        if self.eat_keyword(Keyword::Cast) {
            return self.cast();
        }
        if self.eat(&TokenKind::LeftParen) {
            let expr = self.expression()?;
            self.expect(&TokenKind::RightParen, "\")\"")?;
            return Ok(expr);
        }
        if self.is_name() {
            return self.column_or_function();
        }
        Err(self.error_here("an expression"))
    }

    /// `DATE '…'` or `TIMESTAMP '…'`; without the string, the word is a name.
    fn typed_literal(&mut self) -> Option<Literal> {
        let keyword = self.peek_keyword()?;
        let Some(TokenKind::String(text)) = self.tokens.get(self.next + 1).map(|token| &token.kind)
        else {
            return None;
        };
        let literal = match keyword {
            Keyword::Date => Literal::Date(text.clone()),
            Keyword::Timestamp => Literal::Timestamp(text.clone()),
            _ => return None,
        };
        self.next += 2;
        Some(literal)
    }

    fn column_or_function(&mut self) -> Result<Expr, SqlError> {
        let name = self.name("a name")?;
        if self.eat(&TokenKind::LeftParen) {
            let args = if self.eat(&TokenKind::Star) {
                FunctionArgs::Star
            } else if self.peek_kind() == Some(&TokenKind::RightParen) {
                FunctionArgs::List(Vec::new())
            } else {
                FunctionArgs::List(self.comma_list(Self::expression)?)
            };
            self.expect(&TokenKind::RightParen, "\")\"")?;
            return Ok(Expr::Function { name, args });
        }
        if self.eat(&TokenKind::Dot) {
            let column = self.name("a column name")?;
            return Ok(Expr::Column {
                table: Some(name),
                name: column,
            });
        }
        Ok(Expr::Column { table: None, name })
    }

    /// The rest of `CAST(expr AS type)`, after the keyword.
    fn cast(&mut self) -> Result<Expr, SqlError> {
        self.expect(&TokenKind::LeftParen, "\"(\"")?;
        let expr = self.expression()?;
        self.expect_keyword(Keyword::As)?;
        let to = self.data_type()?;
        self.expect(&TokenKind::RightParen, "\")\"")?;
        Ok(Expr::Cast {
            expr: Box::new(expr),
            to,
        })
    }

    fn data_type(&mut self) -> Result<DataType, SqlError> {
        let Some(token) = self.peek() else {
            return Err(self.error_here("a type"));
        };
        let position = self.position_of(token);
        let (keyword, name) = match &token.kind {
            TokenKind::Word { text, keyword } => (*keyword, text.clone()),
            TokenKind::QuotedIdentifier(name) => (None, name.clone()),
            _ => return Err(self.error_here("a type")),
        };
        self.next += 1;
        Ok(match keyword {
            Some(Keyword::Smallint) => DataType::SmallInt,
            Some(Keyword::Int | Keyword::Integer) => DataType::Integer,
            Some(Keyword::Bigint) => DataType::BigInt,
            Some(Keyword::Real) => DataType::Real,
            Some(Keyword::Double) => {
                self.expect_keyword(Keyword::Precision)?;
                DataType::Double
            }
            Some(Keyword::Boolean) => DataType::Boolean,
            Some(Keyword::Date) => DataType::Date,
            Some(Keyword::Timestamp) => DataType::Timestamp,
            Some(Keyword::Varchar) => self.varchar_length()?,
            Some(Keyword::Character) if self.eat_keyword(Keyword::Varying) => {
                self.varchar_length()?
            }
            _ => return Err(SqlError::UndefinedType { name, position }),
        })
    }

    /// The optional `(n)` of a VARCHAR; without it, the widest VARCHAR.
    fn varchar_length(&mut self) -> Result<DataType, SqlError> {
        if !self.eat(&TokenKind::LeftParen) {
            return Ok(DataType::VARCHAR_MAX);
        }
        let Some(Token {
            kind: TokenKind::Number(length),
            span,
        }) = self.peek().cloned()
        else {
            return Err(self.error_here("a length"));
        };
        self.next += 1;
        let max_chars = length.parse::<u16>().ok().and_then(NonZeroU16::new).ok_or(
            SqlError::InvalidVarcharLength {
                length,
                position: character_position(self.sql, span.start),
            },
        )?;
        self.expect(&TokenKind::RightParen, "\")\"")?;
        Ok(DataType::Varchar(max_chars))
    }

    /// `item [, item …]`.
    fn comma_list<T>(
        &mut self,
        mut item: impl FnMut(&mut Self) -> Result<T, SqlError>,
    ) -> Result<Vec<T>, SqlError> {
        let mut items = vec![item(self)?];
        while self.eat(&TokenKind::Comma) {
            items.push(item(self)?);
        }
        Ok(items)
    }

    /// A word that is not a reserved keyword, or a quoted identifier.
    fn is_name(&self) -> bool {
        match self.peek_kind() {
            Some(TokenKind::Word { keyword, .. }) => !keyword.is_some_and(is_reserved),
            Some(TokenKind::QuotedIdentifier(_)) => true,
            _ => false,
        }
    }

    fn name(&mut self, expected: &str) -> Result<String, SqlError> {
        let name = match self.peek_kind() {
            Some(TokenKind::Word { text, keyword }) if !keyword.is_some_and(is_reserved) => {
                text.clone()
            }
            Some(TokenKind::QuotedIdentifier(name)) => name.clone(),
            _ => return Err(self.error_here(expected)),
        };
        self.next += 1;
        Ok(name)
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.next)
    }

    fn peek_kind(&self) -> Option<&TokenKind> {
        self.kind_at(0)
    }

    /// The kind of the token `ahead` places after the next one.
    fn kind_at(&self, ahead: usize) -> Option<&TokenKind> {
        self.tokens
            .get(self.next.checked_add(ahead)?)
            .map(|token| &token.kind)
    }

    fn peek_keyword(&self) -> Option<Keyword> {
        match self.peek_kind() {
            Some(TokenKind::Word { keyword, .. }) => *keyword,
            _ => None,
        }
    }

    /// Moves past the next token if it is `kind`.
    fn eat(&mut self, kind: &TokenKind) -> bool {
        let is_next = self.peek_kind() == Some(kind);
        if is_next {
            self.next += 1;
        }
        is_next
    }

    fn eat_keyword(&mut self, keyword: Keyword) -> bool {
        let is_next = self.peek_keyword() == Some(keyword);
        if is_next {
            self.next += 1;
        }
        is_next
    }

    fn expect(&mut self, kind: &TokenKind, expected: &str) -> Result<(), SqlError> {
        if self.eat(kind) {
            Ok(())
        } else {
            Err(self.error_here(expected))
        }
    }

    fn expect_keyword(&mut self, keyword: Keyword) -> Result<(), SqlError> {
        if self.eat_keyword(keyword) {
            Ok(())
        } else {
            Err(self.error_here(&keyword.to_string()))
        }
    }

    /// The error for the next token, or for the end of the input.
    fn error_here(&self, expected: &str) -> SqlError {
        match self.peek() {
            Some(token) => SqlError::UnexpectedToken {
                found: self
                    .sql
                    .get(token.span.start..token.span.end)
                    .unwrap_or_default()
                    .to_string(),
                expected: expected.to_string(),
                position: self.position_of(token),
            },
            None => SqlError::UnexpectedEnd {
                expected: expected.to_string(),
            },
        }
    }

    /// Valid SQL ferroDB does not run yet, at the next token.
    fn unsupported(&self, feature: &'static str) -> SqlError {
        SqlError::Unsupported {
            feature,
            position: self.position_here(),
        }
    }

    /// Runs `parse` one level deeper, or fails with `TooDeep` past `MAX_NESTING`.
    fn nested<T>(
        &mut self,
        parse: impl FnOnce(&mut Self) -> Result<T, SqlError>,
    ) -> Result<T, SqlError> {
        self.within_depth(|parser| {
            parser.deeper()?;
            parse(parser)
        })
    }

    /// Runs `parse`, then puts the depth back: the levels it adds end with it.
    fn within_depth<T>(
        &mut self,
        parse: impl FnOnce(&mut Self) -> Result<T, SqlError>,
    ) -> Result<T, SqlError> {
        let depth = self.depth;
        let result = parse(self);
        self.depth = depth;
        result
    }

    /// One level more, or `TooDeep` past `MAX_NESTING`. Each operator or join folded into a
    /// chain adds one too: `a + b + c` is `(a + b) + c`, a tree as deep as the chain is long.
    fn deeper(&mut self) -> Result<(), SqlError> {
        if self.depth >= MAX_NESTING {
            return Err(SqlError::TooDeep {
                position: self.position_here(),
            });
        }
        self.depth += 1;
        Ok(())
    }

    /// The position of the next token, or of the end of the text.
    fn position_here(&self) -> usize {
        self.peek().map_or_else(
            || character_position(self.sql, self.sql.len()),
            |token| self.position_of(token),
        )
    }

    fn position_of(&self, token: &Token) -> usize {
        character_position(self.sql, token.span.start)
    }
}

/// How tightly an operator binds, loosest first, as in PostgreSQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Precedence {
    Or,
    And,
    Not,
    /// `IS [NOT] NULL / TRUE / FALSE / UNKNOWN / DISTINCT FROM`.
    Is,
    Comparison,
    /// `[NOT] BETWEEN`, `[NOT] IN`.
    Range,
    Concat,
    Additive,
    Multiplicative,
    /// A prefix `-` or `+`.
    Sign,
}

impl Precedence {
    /// The next level: the right side of a left-associative operator.
    fn tighter(self) -> Precedence {
        match self {
            Precedence::Or => Precedence::And,
            Precedence::And => Precedence::Not,
            Precedence::Not => Precedence::Is,
            Precedence::Is => Precedence::Comparison,
            Precedence::Comparison => Precedence::Range,
            Precedence::Range => Precedence::Concat,
            Precedence::Concat => Precedence::Additive,
            Precedence::Additive => Precedence::Multiplicative,
            Precedence::Multiplicative | Precedence::Sign => Precedence::Sign,
        }
    }
}

/// The keywords that cannot name a table, a column or an alias unless quoted; the others
/// (type names, `first`, `key`, `rows`…) can, as in PostgreSQL.
pub(super) fn is_reserved(keyword: Keyword) -> bool {
    use Keyword::*;
    matches!(
        keyword,
        All | And
            | As
            | Asc
            | Between
            | By
            | Cast
            | Create
            | Cross
            | Default
            | Delete
            | Desc
            | Distinct
            | Drop
            | False
            | Fetch
            | From
            | Full
            | Group
            | Having
            | In
            | Inner
            | Insert
            | Into
            | Is
            | Join
            | Left
            | Limit
            | Not
            | Null
            | Offset
            | On
            | Or
            | Order
            | Outer
            | Right
            | Select
            | Set
            | Table
            | True
            | Unique
            | Update
            | Values
            | Where
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::{
        Assignment, BinaryOp, BooleanTest, ColumnDef, FunctionArgs, JoinKind, Literal, OrderItem,
        SelectItem, TableRef, UnaryOp,
    };
    use std::num::NonZeroU16;

    fn select(sql: &str) -> Select {
        match parse(sql).unwrap().as_slice() {
            [Statement::Select(select)] => (**select).clone(),
            other => panic!("{other:?}"),
        }
    }

    /// The expression of `SELECT <sql>`.
    fn expr(sql: &str) -> Expr {
        match select(&format!("SELECT {sql}")).projection.as_slice() {
            [SelectItem::Expr { expr, alias: None }] => expr.clone(),
            other => panic!("{other:?}"),
        }
    }

    fn parse_err(sql: &str) -> SqlError {
        parse(sql).unwrap_err()
    }

    fn column(name: &str) -> Expr {
        Expr::Column {
            table: None,
            name: name.to_string(),
        }
    }

    fn number(text: &str) -> Expr {
        Expr::Literal(Literal::Number(text.to_string()))
    }

    fn unexpected(found: &str, expected: &str, position: usize) -> SqlError {
        SqlError::UnexpectedToken {
            found: found.to_string(),
            expected: expected.to_string(),
            position,
        }
    }

    #[test]
    fn literals_should_become_literal_expressions() {
        let cases = [
            ("42", number("42")),
            ("6.02e+23", number("6.02e+23")),
            (
                "'it''s'",
                Expr::Literal(Literal::String("it's".to_string())),
            ),
            ("TRUE", Expr::Literal(Literal::Boolean(true))),
            ("false", Expr::Literal(Literal::Boolean(false))),
            ("NULL", Expr::Literal(Literal::Null)),
            (
                "DATE '2026-10-23'",
                Expr::Literal(Literal::Date("2026-10-23".to_string())),
            ),
            (
                "timestamp '2026-10-23 10:00:00'",
                Expr::Literal(Literal::Timestamp("2026-10-23 10:00:00".to_string())),
            ),
        ];

        for (sql, expected) in cases {
            assert_eq!(expr(sql), expected, "{sql}");
        }
    }

    #[test]
    fn a_type_keyword_without_a_string_should_be_a_name() {
        assert_eq!(expr("date"), column("date"));
        assert_eq!(expr("rows"), column("rows"));
    }

    #[test]
    fn columns_may_be_qualified_and_quoted() {
        assert_eq!(expr("Name"), column("name"));
        assert_eq!(
            expr("t.a"),
            Expr::Column {
                table: Some("t".to_string()),
                name: "a".to_string()
            }
        );
        assert_eq!(
            expr("\"T\".\"A b\""),
            Expr::Column {
                table: Some("T".to_string()),
                name: "A b".to_string()
            }
        );
    }

    #[test]
    fn parentheses_should_give_the_inner_expression() {
        assert_eq!(expr("(a)"), column("a"));
        assert_eq!(expr("((1))"), number("1"));
    }

    #[test]
    fn function_calls_should_take_a_star_nothing_or_arguments() {
        let call = |name: &str, args| Expr::Function {
            name: name.to_string(),
            args,
        };

        assert_eq!(expr("COUNT(*)"), call("count", FunctionArgs::Star));
        assert_eq!(expr("f()"), call("f", FunctionArgs::List(vec![])));
        assert_eq!(
            expr("sum(x)"),
            call("sum", FunctionArgs::List(vec![column("x")]))
        );
        assert_eq!(
            expr("f(a, 2)"),
            call("f", FunctionArgs::List(vec![column("a"), number("2")]))
        );
    }

    #[test]
    fn cast_should_take_every_supported_type() {
        let varchar = |n| DataType::Varchar(NonZeroU16::new(n).unwrap());
        let cases = [
            ("SMALLINT", DataType::SmallInt),
            ("int", DataType::Integer),
            ("INTEGER", DataType::Integer),
            ("BIGINT", DataType::BigInt),
            ("REAL", DataType::Real),
            ("DOUBLE PRECISION", DataType::Double),
            ("BOOLEAN", DataType::Boolean),
            ("DATE", DataType::Date),
            ("TIMESTAMP", DataType::Timestamp),
            ("VARCHAR(5)", varchar(5)),
            ("VARCHAR(65535)", varchar(65_535)),
            ("CHARACTER VARYING(1)", varchar(1)),
            ("VARCHAR", DataType::VARCHAR_MAX),
        ];

        for (name, to) in cases {
            let expected = Expr::Cast {
                expr: Box::new(column("a")),
                to,
            };
            assert_eq!(expr(&format!("CAST(a AS {name})")), expected, "{name}");
        }
    }

    #[test]
    fn a_varchar_length_outside_its_range_should_fail() {
        for length in ["0", "65536", "1.5", "99999999999999999999"] {
            let err = parse_err(&format!("SELECT CAST(a AS VARCHAR({length}))"));

            assert_eq!(
                err,
                SqlError::InvalidVarcharLength {
                    length: length.to_string(),
                    position: 26
                }
            );
            assert_eq!(err.sqlstate(), "22023");
        }
    }

    #[test]
    fn a_type_ferro_does_not_have_should_be_undefined() {
        for name in ["text", "char", "\"INTEGER\""] {
            let err = parse_err(&format!("SELECT CAST(a AS {name})"));

            let expected_name = name.trim_matches('"');
            assert_eq!(
                err,
                SqlError::UndefinedType {
                    name: expected_name.to_string(),
                    position: 18
                }
            );
            assert_eq!(err.sqlstate(), "42704");
        }
    }

    #[test]
    fn select_items_should_be_separated_by_commas() {
        let items = select("SELECT a, 1").projection;

        assert_eq!(
            items,
            [
                SelectItem::Expr {
                    expr: column("a"),
                    alias: None
                },
                SelectItem::Expr {
                    expr: number("1"),
                    alias: None
                },
            ]
        );
    }

    #[test]
    fn a_missing_operand_should_be_a_syntax_error() {
        assert_eq!(
            parse_err("SELECT"),
            SqlError::UnexpectedEnd {
                expected: "an expression".to_string()
            }
        );
        assert_eq!(parse_err("SELECT ,"), unexpected(",", "an expression", 8));
        assert_eq!(
            parse_err("SELECT f(a,)"),
            unexpected(")", "an expression", 12)
        );
    }

    #[test]
    fn an_unclosed_parenthesis_should_fail_at_the_end() {
        assert_eq!(
            parse_err("SELECT (a"),
            SqlError::UnexpectedEnd {
                expected: "\")\"".to_string()
            }
        );
        assert_eq!(
            parse_err("SELECT CAST(a AS INT"),
            SqlError::UnexpectedEnd {
                expected: "\")\"".to_string()
            }
        );
    }

    #[test]
    fn a_reserved_word_should_not_be_a_column() {
        let err = parse_err("SELECT select");

        assert_eq!(err, unexpected("select", "an expression", 8));
        assert_eq!(err.sqlstate(), "42601");
    }

    #[test]
    fn a_statement_should_start_with_its_keyword() {
        assert_eq!(parse_err("WHERE a"), unexpected("WHERE", "a statement", 1));
    }

    fn statement(sql: &str) -> Statement {
        match parse(sql).unwrap().as_slice() {
            [statement] => statement.clone(),
            other => panic!("{other:?}"),
        }
    }

    fn column_def(name: &str, data_type: DataType, is_not_null: bool) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            data_type,
            is_not_null,
        }
    }

    #[test]
    fn create_table_should_take_columns_types_and_null_markers() {
        let sql = "CREATE TABLE Users (id INTEGER NOT NULL, name VARCHAR(20) NULL, born DATE, \
                   \"Score\" DOUBLE PRECISION)";

        let parsed = statement(sql);

        assert_eq!(
            parsed,
            Statement::CreateTable {
                name: "users".to_string(),
                columns: vec![
                    column_def("id", DataType::Integer, true),
                    column_def(
                        "name",
                        DataType::Varchar(NonZeroU16::new(20).unwrap()),
                        false
                    ),
                    column_def("born", DataType::Date, false),
                    column_def("Score", DataType::Double, false),
                ],
            }
        );
    }

    #[test]
    fn create_table_should_take_non_reserved_keywords_as_names() {
        let parsed = statement("CREATE TABLE rows (date DATE, key INT)");

        assert_eq!(
            parsed,
            Statement::CreateTable {
                name: "rows".to_string(),
                columns: vec![
                    column_def("date", DataType::Date, false),
                    column_def("key", DataType::Integer, false),
                ],
            }
        );
    }

    #[test]
    fn primary_key_and_unique_columns_should_be_unsupported() {
        assert_eq!(
            parse_err("CREATE TABLE t (id INT PRIMARY KEY)"),
            SqlError::Unsupported {
                feature: "PRIMARY KEY",
                position: 24
            }
        );
        assert_eq!(
            parse_err("CREATE TABLE t (id INT NOT NULL UNIQUE)"),
            SqlError::Unsupported {
                feature: "UNIQUE columns",
                position: 33
            }
        );
    }

    #[test]
    fn a_malformed_create_table_should_fail() {
        assert_eq!(
            parse_err("CREATE TABLE t ()"),
            unexpected(")", "a column name", 17)
        );
        assert_eq!(
            parse_err("CREATE TABLE t (id)"),
            unexpected(")", "a type", 19)
        );
        assert_eq!(
            parse_err("CREATE TABLE t (id INT"),
            SqlError::UnexpectedEnd {
                expected: "\",\" or \")\"".to_string()
            }
        );
        assert_eq!(
            parse_err("CREATE TABLE select (a INT)"),
            unexpected("select", "a table name", 14)
        );
        assert_eq!(
            parse_err("CREATE TABLE t (a INT NOT)"),
            unexpected(")", "NULL", 26)
        );
    }

    #[test]
    fn create_index_should_take_one_column_and_say_whether_it_is_unique() {
        let index = |is_unique| Statement::CreateIndex {
            name: "users_id".to_string(),
            table: "users".to_string(),
            column: "id".to_string(),
            is_unique,
        };

        assert_eq!(
            statement("CREATE UNIQUE INDEX users_id ON users (id)"),
            index(true)
        );
        assert_eq!(
            statement("create index users_id on users (id)"),
            index(false)
        );
    }

    fn text(value: &str) -> Expr {
        Expr::Literal(Literal::String(value.to_string()))
    }

    #[test]
    fn insert_should_take_rows_with_or_without_a_column_list() {
        let without = statement("INSERT INTO t VALUES (1, 'a'), (2, NULL)");
        let with = statement("INSERT INTO t (id, \"Name\") VALUES (1, 'x')");

        assert_eq!(
            without,
            Statement::Insert {
                table: "t".to_string(),
                columns: vec![],
                rows: vec![
                    vec![number("1"), text("a")],
                    vec![number("2"), Expr::Literal(Literal::Null)],
                ],
            }
        );
        assert_eq!(
            with,
            Statement::Insert {
                table: "t".to_string(),
                columns: vec!["id".to_string(), "Name".to_string()],
                rows: vec![vec![number("1"), text("x")]],
            }
        );
    }

    #[test]
    fn a_malformed_insert_should_fail() {
        assert_eq!(
            parse_err("INSERT INTO t () VALUES (1)"),
            unexpected(")", "a column name", 16)
        );
        assert_eq!(
            parse_err("INSERT INTO t VALUES ()"),
            unexpected(")", "an expression", 23)
        );
        assert_eq!(parse_err("INSERT t VALUES (1)"), unexpected("t", "INTO", 8));
        assert_eq!(
            parse_err("INSERT INTO t VALUES (1"),
            SqlError::UnexpectedEnd {
                expected: "\",\" or \")\"".to_string()
            }
        );
        assert_eq!(
            parse_err("INSERT INTO t SELECT 1"),
            SqlError::Unsupported {
                feature: "INSERT … SELECT",
                position: 15
            }
        );
    }

    #[test]
    fn explain_should_wrap_a_select() {
        let explained = statement("EXPLAIN SELECT a FROM t");
        let as_names = statement("SELECT explain FROM explain");

        assert!(matches!(&explained, Statement::Explain(_)), "{explained:?}");
        assert_eq!(explained.to_string(), "EXPLAIN SELECT a FROM t");
        assert_eq!(as_names.to_string(), "SELECT explain FROM explain");
    }

    #[test]
    fn explain_of_anything_but_a_select_should_fail() {
        let unsupported = SqlError::Unsupported {
            feature: "EXPLAIN of statements other than SELECT",
            position: 9,
        };

        assert_eq!(parse_err("EXPLAIN INSERT INTO t VALUES (1)"), unsupported);
        assert_eq!(parse_err("EXPLAIN DELETE FROM t"), unsupported);
        assert_eq!(parse_err("EXPLAIN EXPLAIN SELECT 1"), unsupported);
        assert_eq!(parse_err("EXPLAIN t"), unexpected("t", "SELECT", 9));
        assert_eq!(
            parse_err("EXPLAIN"),
            SqlError::UnexpectedEnd {
                expected: "SELECT".to_string()
            }
        );
    }

    #[test]
    fn update_should_take_assignments_and_an_optional_where() {
        let with_where = statement("UPDATE t SET v = v + 1, s = 'x' WHERE id > 2");
        let without = statement("UPDATE t SET v = NULL");

        assert_eq!(
            with_where,
            Statement::Update {
                table: "t".to_string(),
                assignments: vec![
                    Assignment {
                        column: "v".to_string(),
                        value: expr("v + 1"),
                    },
                    Assignment {
                        column: "s".to_string(),
                        value: text("x"),
                    },
                ],
                selection: Some(expr("id > 2")),
            }
        );
        assert_eq!(
            without,
            Statement::Update {
                table: "t".to_string(),
                assignments: vec![Assignment {
                    column: "v".to_string(),
                    value: Expr::Literal(Literal::Null),
                }],
                selection: None,
            }
        );
    }

    #[test]
    fn a_malformed_update_should_fail() {
        assert_eq!(parse_err("UPDATE t v = 1"), unexpected("v", "SET", 10));
        assert_eq!(parse_err("UPDATE t SET v 1"), unexpected("1", "\"=\"", 16));
        assert_eq!(
            parse_err("UPDATE t SET"),
            SqlError::UnexpectedEnd {
                expected: "a column name".to_string()
            }
        );
    }

    #[test]
    fn delete_should_take_an_optional_where() {
        assert_eq!(
            statement("DELETE FROM t WHERE id = 1"),
            Statement::Delete {
                table: "t".to_string(),
                selection: Some(expr("id = 1")),
            }
        );
        assert_eq!(
            statement("DELETE FROM t"),
            Statement::Delete {
                table: "t".to_string(),
                selection: None,
            }
        );
        assert_eq!(parse_err("DELETE t"), unexpected("t", "FROM", 8));
    }

    #[test]
    fn a_malformed_create_index_should_fail() {
        assert_eq!(
            parse_err("CREATE UNIQUE INDEX i ON t (a, b)"),
            SqlError::Unsupported {
                feature: "indexes on more than one column",
                position: 30
            }
        );
        assert_eq!(
            parse_err("CREATE UNIQUE TABLE t (a INT)"),
            unexpected("TABLE", "INDEX", 15)
        );
        assert_eq!(
            parse_err("CREATE VIEW v"),
            unexpected("VIEW", "TABLE, INDEX or UNIQUE INDEX", 8)
        );
        assert_eq!(parse_err("CREATE INDEX i t (a)"), unexpected("t", "ON", 16));
    }

    /// The expression as the parser read it, every compound part in parentheses.
    fn shape(sql: &str) -> String {
        expr(sql).to_string()
    }

    #[test]
    fn arithmetic_should_bind_tighter_than_concatenation_and_comparison() {
        let cases = [
            ("a + b * c", "(a + (b * c))"),
            ("a * b + c", "((a * b) + c)"),
            ("a - b - c", "((a - b) - c)"),
            ("a / b % c", "((a / b) % c)"),
            ("a || b + c", "(a || (b + c))"),
            ("a || b || c", "((a || b) || c)"),
            ("a = b || c", "(a = (b || c))"),
            ("a <> b + 1", "(a <> (b + 1))"),
        ];

        for (sql, expected) in cases {
            assert_eq!(shape(sql), expected, "{sql}");
        }
    }

    #[test]
    fn logic_should_bind_looser_than_comparison_and_and_tighter_than_or() {
        let cases = [
            ("a OR b AND c", "(a OR (b AND c))"),
            ("a AND b OR c", "((a AND b) OR c)"),
            ("NOT a = b", "(NOT (a = b))"),
            ("NOT a AND b", "((NOT a) AND b)"),
            ("NOT NOT a", "(NOT (NOT a))"),
            ("a AND NOT b = c", "(a AND (NOT (b = c)))"),
            ("a OR b OR c", "((a OR b) OR c)"),
        ];

        for (sql, expected) in cases {
            assert_eq!(shape(sql), expected, "{sql}");
        }
    }

    #[test]
    fn is_tests_should_bind_looser_than_comparison() {
        let cases = [
            ("a = b IS NULL", "((a = b) IS NULL)"),
            ("a IS NOT NULL AND b", "((a IS NOT NULL) AND b)"),
            ("a IS TRUE", "(a IS TRUE)"),
            ("a IS NOT FALSE", "(a IS NOT FALSE)"),
            ("a IS UNKNOWN", "(a IS UNKNOWN)"),
            ("a IS DISTINCT FROM b + 1", "(a IS DISTINCT FROM (b + 1))"),
            (
                "a IS NOT DISTINCT FROM NULL",
                "(a IS NOT DISTINCT FROM NULL)",
            ),
        ];

        for (sql, expected) in cases {
            assert_eq!(shape(sql), expected, "{sql}");
        }
    }

    #[test]
    fn between_and_in_should_bind_tighter_than_comparison() {
        let cases = [
            ("x BETWEEN 1 AND 2 AND y", "((x BETWEEN 1 AND 2) AND y)"),
            ("x NOT BETWEEN a + 1 AND b", "(x NOT BETWEEN (a + 1) AND b)"),
            ("a = b BETWEEN 1 AND 2", "(a = (b BETWEEN 1 AND 2))"),
            ("x IN (1, 2)", "(x IN (1, 2))"),
            ("x NOT IN (a || b)", "(x NOT IN ((a || b)))"),
        ];

        for (sql, expected) in cases {
            assert_eq!(shape(sql), expected, "{sql}");
        }
    }

    #[test]
    fn signs_should_bind_tightest() {
        let cases = [
            ("- a * b", "((- a) * b)"),
            ("a * - b", "(a * (- b))"),
            ("- - 1", "(- (- 1))"),
            ("+a", "(+ a)"),
            ("-1", "(- 1)"),
        ];

        for (sql, expected) in cases {
            assert_eq!(shape(sql), expected, "{sql}");
        }
    }

    /// `SELECT` followed by `levels` openers around `1`.
    fn nested(open: &str, close: &str, levels: usize) -> String {
        format!("SELECT {}1{}", open.repeat(levels), close.repeat(levels))
    }

    fn nested_tables(levels: usize) -> String {
        format!(
            "SELECT 1 FROM {}t{}",
            "(".repeat(levels),
            ")".repeat(levels)
        )
    }

    #[test]
    fn nesting_up_to_the_limit_should_parse_and_beyond_it_should_fail() {
        for (open, close) in [("(", ")"), ("- ", ""), ("NOT ", "")] {
            assert!(
                parse(&nested(open, close, MAX_NESTING - 1)).is_ok(),
                "{open}"
            );

            let err = parse_err(&nested(open, close, MAX_NESTING));

            assert!(matches!(err, SqlError::TooDeep { .. }), "{open}: {err:?}");
            assert_eq!(err.sqlstate(), "54001");
        }
        assert!(parse(&nested_tables(MAX_NESTING - 1)).is_ok());
        assert!(matches!(
            parse_err(&nested_tables(MAX_NESTING)),
            SqlError::TooDeep { .. }
        ));
        // The outermost expression is the first level: the limit is hit at the `1`.
        assert_eq!(
            parse_err(&nested("(", ")", MAX_NESTING)),
            SqlError::TooDeep {
                position: "SELECT ".len() + MAX_NESTING + 1
            }
        );
    }

    /// `SELECT` followed by `operators` binary operators: `1 + 1 + … + 1`.
    fn chain(operator: &str, operators: usize) -> String {
        format!("SELECT {}1", format!("1 {operator} ").repeat(operators))
    }

    /// `SELECT 1 FROM t` followed by `joins` more tables, each after `separator`.
    fn joins(separator: &str, joins: usize) -> String {
        format!("SELECT 1 FROM t{}", format!("{separator}t").repeat(joins))
    }

    #[test]
    fn chains_up_to_the_limit_should_parse_and_beyond_it_should_fail() {
        for operator in ["+", "*", "||", "AND", "OR"] {
            assert!(
                parse(&chain(operator, MAX_NESTING - 1)).is_ok(),
                "{operator}"
            );

            let err = parse_err(&chain(operator, MAX_NESTING));

            assert!(
                matches!(err, SqlError::TooDeep { .. }),
                "{operator}: {err:?}"
            );
            assert_eq!(err.sqlstate(), "54001");
        }
        // An expression is a level before its first operand; FROM is not, so a chain of joins
        // fits one more.
        for separator in [", ", " CROSS JOIN "] {
            assert!(parse(&joins(separator, MAX_NESTING)).is_ok(), "{separator}");

            let err = parse_err(&joins(separator, MAX_NESTING + 1));

            assert!(
                matches!(err, SqlError::TooDeep { .. }),
                "{separator}: {err:?}"
            );
        }
    }

    #[test]
    fn very_deep_nesting_should_fail_without_exhausting_a_small_stack() {
        let statements = [
            nested("(", ")", 10_000),
            nested("- ", "", 10_000),
            nested("NOT ", "", 10_000),
            nested_tables(10_000),
            chain("+", 100_000),
            chain("OR", 100_000),
            joins(", ", 100_000),
            joins(" CROSS JOIN ", 100_000),
        ];

        for sql in statements {
            let result = std::thread::Builder::new()
                .stack_size(2 * 1024 * 1024)
                .spawn(move || parse(&sql).map(|statements| statements.len()))
                .unwrap()
                .join()
                .unwrap();

            assert!(
                matches!(result, Err(SqlError::TooDeep { .. })),
                "{result:?}"
            );
        }
    }

    #[test]
    fn fetch_without_its_keywords_should_fail() {
        assert_eq!(
            parse_err("SELECT a FETCH 5 ROWS ONLY"),
            unexpected("5", "FIRST or NEXT", 16)
        );
        assert_eq!(
            parse_err("SELECT a FETCH FIRST 5 ONLY"),
            unexpected("ONLY", "ROW or ROWS", 24)
        );
        assert_eq!(
            parse_err("SELECT a FETCH FIRST 5 ROWS"),
            SqlError::UnexpectedEnd {
                expected: "ONLY".to_string()
            }
        );
    }

    #[test]
    fn incomplete_type_names_should_fail() {
        assert_eq!(
            parse_err("SELECT CAST(a AS DOUBLE)"),
            unexpected(")", "PRECISION", 24)
        );
        assert_eq!(
            parse_err("SELECT CAST(a AS CHARACTER(5))"),
            SqlError::UndefinedType {
                name: "character".to_string(),
                position: 18
            }
        );
        assert_eq!(
            parse_err("SELECT CAST(a AS VARCHAR())"),
            unexpected(")", "a length", 26)
        );
        assert_eq!(
            parse_err("SELECT CAST(a AS VARCHAR(-1))"),
            unexpected("-", "a length", 26)
        );
    }

    #[test]
    fn an_empty_in_list_should_fail() {
        assert_eq!(
            parse_err("SELECT x IN ()"),
            unexpected(")", "an expression", 14)
        );
    }

    #[test]
    fn comparisons_should_not_chain() {
        let expected = "an operator that is not a comparison";

        assert_eq!(parse_err("SELECT a < b < c"), unexpected("<", expected, 14));
        assert_eq!(parse_err("SELECT a = b = c"), unexpected("=", expected, 14));
        assert_eq!(shape("a = (b = c)"), "(a = (b = c))");
    }

    #[test]
    fn an_incomplete_operator_should_be_a_syntax_error() {
        let end = |expected: &str| SqlError::UnexpectedEnd {
            expected: expected.to_string(),
        };

        assert_eq!(parse_err("SELECT a +"), end("an expression"));
        assert_eq!(parse_err("SELECT a AND"), end("an expression"));
        assert_eq!(parse_err("SELECT x BETWEEN 1"), end("AND"));
        assert_eq!(parse_err("SELECT x IN 1"), unexpected("1", "\"(\"", 13));
        assert_eq!(
            parse_err("SELECT a IS 5"),
            unexpected("5", "NULL, TRUE, FALSE, UNKNOWN or DISTINCT FROM", 13)
        );
        assert_eq!(
            parse_err("SELECT a NOT b"),
            unexpected("b", "BETWEEN or IN", 14)
        );
    }

    fn table(name: &str, alias: Option<&str>) -> TableRef {
        TableRef::Table {
            name: name.to_string(),
            alias: alias.map(str::to_string),
        }
    }

    fn join(left: TableRef, kind: JoinKind, right: TableRef, on: Option<Expr>) -> TableRef {
        TableRef::Join {
            left: Box::new(left),
            right: Box::new(right),
            kind,
            on,
        }
    }

    fn item(expr: Expr, alias: Option<&str>) -> SelectItem {
        SelectItem::Expr {
            expr,
            alias: alias.map(str::to_string),
        }
    }

    #[test]
    fn select_items_should_take_wildcards_and_aliases() {
        let items = select("SELECT *, t.*, a AS b, a b, 1 AS \"B c\", 2 AS from").projection;

        assert_eq!(
            items,
            [
                SelectItem::Wildcard,
                SelectItem::QualifiedWildcard("t".to_string()),
                item(column("a"), Some("b")),
                item(column("a"), Some("b")),
                item(number("1"), Some("B c")),
                item(number("2"), Some("from")),
            ]
        );
    }

    #[test]
    fn from_should_take_tables_with_or_without_aliases_and_commas() {
        assert_eq!(select("SELECT 1 FROM t").from, Some(table("t", None)));
        assert_eq!(
            select("SELECT 1 FROM t AS x").from,
            Some(table("t", Some("x")))
        );
        assert_eq!(
            select("SELECT 1 FROM t x").from,
            Some(table("t", Some("x")))
        );
        assert_eq!(
            select("SELECT 1 FROM a, b c").from,
            Some(join(
                table("a", None),
                JoinKind::Cross,
                table("b", Some("c")),
                None
            ))
        );
    }

    #[test]
    fn every_join_should_be_read_left_to_right() {
        let on = |sql: &str| Some(expr(sql));
        let cases = [
            ("a JOIN b ON x", JoinKind::Inner),
            ("a INNER JOIN b ON x", JoinKind::Inner),
            ("a LEFT JOIN b ON x", JoinKind::Left),
            ("a LEFT OUTER JOIN b ON x", JoinKind::Left),
            ("a RIGHT OUTER JOIN b ON x", JoinKind::Right),
            ("a FULL JOIN b ON x", JoinKind::Full),
        ];

        for (sql, kind) in cases {
            let from = select(&format!("SELECT 1 FROM {sql}")).from;

            assert_eq!(
                from,
                Some(join(table("a", None), kind, table("b", None), on("x"))),
                "{sql}"
            );
        }
        let chained = select("SELECT 1 FROM a CROSS JOIN b LEFT JOIN c ON y").from;
        let cross = join(table("a", None), JoinKind::Cross, table("b", None), None);
        assert_eq!(
            chained,
            Some(join(cross, JoinKind::Left, table("c", None), on("y")))
        );
        let grouped = select("SELECT 1 FROM a JOIN (b JOIN c ON y) ON x").from;
        let inner = join(table("b", None), JoinKind::Inner, table("c", None), on("y"));
        assert_eq!(
            grouped,
            Some(join(table("a", None), JoinKind::Inner, inner, on("x")))
        );
    }

    #[test]
    fn a_join_without_on_should_fail_and_a_cross_join_should_take_none() {
        assert_eq!(
            parse_err("SELECT 1 FROM a JOIN b"),
            SqlError::UnexpectedEnd {
                expected: "ON".to_string()
            }
        );
        assert_eq!(
            parse_err("SELECT 1 FROM a CROSS JOIN b ON x"),
            unexpected("ON", "\";\"", 30)
        );
    }

    #[test]
    fn where_group_by_and_having_should_take_expressions() {
        let query = select("SELECT a FROM t WHERE a > 1 GROUP BY a, b HAVING count(*) > 2");

        assert_eq!(query.selection, Some(expr("a > 1")));
        assert_eq!(query.group_by, [column("a"), column("b")]);
        assert_eq!(query.having, Some(expr("count(*) > 2")));
    }

    #[test]
    fn order_by_should_take_directions_and_null_positions() {
        let order =
            select("SELECT a FROM t ORDER BY a, b DESC, c ASC NULLS FIRST, d DESC NULLS LAST")
                .order_by;

        let item = |name: &str, is_descending, nulls_first| OrderItem {
            expr: column(name),
            is_descending,
            nulls_first,
        };
        assert_eq!(
            order,
            [
                item("a", false, None),
                item("b", true, None),
                item("c", false, Some(true)),
                item("d", true, Some(false)),
            ]
        );
    }

    #[test]
    fn limit_offset_and_fetch_should_set_the_counts() {
        let cases = [
            ("LIMIT 10", Some("10"), None),
            ("LIMIT ALL", None, None),
            ("LIMIT 10 OFFSET 5", Some("10"), Some("5")),
            ("OFFSET 5 LIMIT 10", Some("10"), Some("5")),
            ("OFFSET 5 ROWS", None, Some("5")),
            ("OFFSET 1 ROW FETCH FIRST 3 ROWS ONLY", Some("3"), Some("1")),
            ("FETCH NEXT 2 ROW ONLY", Some("2"), None),
            ("FETCH FIRST ROW ONLY", Some("1"), None),
        ];

        for (clauses, limit, offset) in cases {
            let query = select(&format!("SELECT a FROM t {clauses}"));

            assert_eq!(query.limit, limit.map(number), "{clauses}");
            assert_eq!(query.offset, offset.map(number), "{clauses}");
        }
    }

    #[test]
    fn a_count_given_twice_should_fail() {
        assert_eq!(
            parse_err("SELECT a FROM t LIMIT 1 FETCH FIRST 2 ROWS ONLY"),
            unexpected("FETCH", "only one LIMIT or FETCH", 25)
        );
        assert_eq!(
            parse_err("SELECT a FROM t LIMIT 1 LIMIT 2"),
            unexpected("LIMIT", "only one LIMIT or FETCH", 25)
        );
        assert_eq!(
            parse_err("SELECT a FROM t OFFSET 1 OFFSET 2"),
            unexpected("OFFSET", "only one OFFSET", 26)
        );
    }

    #[test]
    fn statements_should_be_separated_by_semicolons() {
        assert_eq!(parse("SELECT 1; SELECT 2;").unwrap().len(), 2);
        assert!(parse(";;").unwrap().is_empty());
        assert!(parse("").unwrap().is_empty());
        assert_eq!(
            parse_err("SELECT 1 SELECT 2"),
            unexpected("SELECT", "\";\"", 10)
        );
    }

    #[test]
    fn select_distinct_should_be_unsupported_and_select_all_accepted() {
        let err = parse_err("SELECT DISTINCT a FROM t");

        assert_eq!(
            err,
            SqlError::Unsupported {
                feature: "SELECT DISTINCT",
                position: 8
            }
        );
        assert_eq!(err.sqlstate(), "0A000");
        assert_eq!(select("SELECT ALL a").projection, [item(column("a"), None)]);
    }

    #[test]
    fn non_reserved_keywords_should_name_columns_and_tables() {
        let query = select("SELECT date, key, first AS last FROM rows");

        assert_eq!(
            query.projection,
            [
                item(column("date"), None),
                item(column("key"), None),
                item(column("first"), Some("last")),
            ]
        );
        assert_eq!(query.from, Some(table("rows", None)));
    }

    #[test]
    fn clauses_out_of_order_or_incomplete_should_fail() {
        assert_eq!(
            parse_err("SELECT a WHERE x FROM t"),
            unexpected("FROM", "\";\"", 18)
        );
        assert_eq!(
            parse_err("SELECT a FROM t WHERE"),
            SqlError::UnexpectedEnd {
                expected: "an expression".to_string()
            }
        );
        assert_eq!(
            parse_err("SELECT a FROM"),
            SqlError::UnexpectedEnd {
                expected: "a table name".to_string()
            }
        );
        assert_eq!(
            parse_err("SELECT a FROM t ORDER a"),
            unexpected("a", "BY", 23)
        );
        assert_eq!(
            parse_err("SELECT a FROM t ORDER BY a NULLS"),
            SqlError::UnexpectedEnd {
                expected: "FIRST or LAST".to_string()
            }
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

        fn pick<T: Clone>(&mut self, items: &[T]) -> T {
            items[self.below(items.len())].clone()
        }

        /// Names that are bare, need quotes, or are non-reserved keywords.
        fn name(&mut self) -> String {
            self.pick(&[
                "a",
                "b2",
                "_x",
                "Name",
                "select",
                "two words",
                "date",
                "rows",
                "ç",
                "x\"y",
            ])
            .to_string()
        }

        fn literal(&mut self) -> Literal {
            match self.below(6) {
                0 => Literal::Number(
                    self.pick(&["0", "42", "1.5", ".5", "1e3", "2.5E-3"])
                        .to_string(),
                ),
                1 => Literal::String(self.pick(&["", "it's", "a\nb", "--", "ç"]).to_string()),
                2 => Literal::Boolean(self.chance(2)),
                3 => Literal::Null,
                4 => Literal::Date("2026-10-23".to_string()),
                _ => Literal::Timestamp("2026-10-23 10:00:00".to_string()),
            }
        }

        fn expr(&mut self, depth: usize) -> Expr {
            let leaf = depth == 0 || self.chance(3);
            if leaf {
                return match self.below(3) {
                    0 => Expr::Literal(self.literal()),
                    1 => Expr::Column {
                        table: None,
                        name: self.name(),
                    },
                    _ => Expr::Column {
                        table: Some(self.name()),
                        name: self.name(),
                    },
                };
            }
            let child = Box::new(self.expr(depth - 1));
            match self.below(10) {
                0 => Expr::Unary {
                    op: self.pick(&[UnaryOp::Not, UnaryOp::Minus, UnaryOp::Plus]),
                    expr: child,
                },
                1 | 2 => Expr::Binary {
                    op: self.pick(&BINARY_OPS),
                    left: child,
                    right: Box::new(self.expr(depth - 1)),
                },
                3 => Expr::IsNull {
                    expr: child,
                    negated: self.chance(2),
                },
                4 => Expr::Is {
                    expr: child,
                    test: self.pick(&[BooleanTest::True, BooleanTest::False, BooleanTest::Unknown]),
                    negated: self.chance(2),
                },
                5 => Expr::IsDistinctFrom {
                    left: child,
                    right: Box::new(self.expr(depth - 1)),
                    negated: self.chance(2),
                },
                6 => Expr::Between {
                    expr: child,
                    low: Box::new(self.expr(depth - 1)),
                    high: Box::new(self.expr(depth - 1)),
                    negated: self.chance(2),
                },
                7 => Expr::InList {
                    expr: child,
                    list: (0..1 + self.below(3))
                        .map(|_| self.expr(depth - 1))
                        .collect(),
                    negated: self.chance(2),
                },
                8 => Expr::Cast {
                    expr: child,
                    to: self.pick(&[
                        DataType::SmallInt,
                        DataType::Integer,
                        DataType::Double,
                        DataType::Boolean,
                        DataType::Timestamp,
                        DataType::Varchar(NonZeroU16::new(7).unwrap()),
                        DataType::VARCHAR_MAX,
                    ]),
                },
                _ => Expr::Function {
                    name: self.name(),
                    args: match self.chance(3) {
                        true => FunctionArgs::Star,
                        false => FunctionArgs::List(
                            std::iter::once(*child)
                                .chain((0..self.below(2)).map(|_| self.expr(depth - 1)))
                                .collect(),
                        ),
                    },
                },
            }
        }

        fn table_ref(&mut self, depth: usize) -> TableRef {
            if depth == 0 || self.chance(2) {
                return TableRef::Table {
                    name: self.name(),
                    alias: self.chance(2).then(|| self.name()),
                };
            }
            let kind = self.pick(&[
                JoinKind::Inner,
                JoinKind::Left,
                JoinKind::Right,
                JoinKind::Full,
                JoinKind::Cross,
            ]);
            TableRef::Join {
                left: Box::new(self.table_ref(depth - 1)),
                right: Box::new(self.table_ref(depth - 1)),
                kind,
                on: (kind != JoinKind::Cross).then(|| self.expr(2)),
            }
        }

        fn select(&mut self) -> Select {
            let projection = (0..1 + self.below(3))
                .map(|_| match self.below(4) {
                    0 => SelectItem::Wildcard,
                    1 => SelectItem::QualifiedWildcard(self.name()),
                    _ => SelectItem::Expr {
                        expr: self.expr(3),
                        alias: self.chance(2).then(|| self.name()),
                    },
                })
                .collect();
            Select {
                projection,
                from: (!self.chance(4)).then(|| self.table_ref(2)),
                selection: self.chance(2).then(|| self.expr(3)),
                group_by: (0..self.below(3)).map(|_| self.expr(2)).collect(),
                having: self.chance(3).then(|| self.expr(2)),
                order_by: (0..self.below(3))
                    .map(|_| OrderItem {
                        expr: self.expr(2),
                        is_descending: self.chance(2),
                        nulls_first: self.pick(&[None, Some(true), Some(false)]),
                    })
                    .collect(),
                limit: self.chance(3).then(|| self.expr(1)),
                offset: self.chance(3).then(|| self.expr(1)),
            }
        }

        fn data_type(&mut self) -> DataType {
            self.pick(&[
                DataType::SmallInt,
                DataType::BigInt,
                DataType::Real,
                DataType::Boolean,
                DataType::Date,
                DataType::Varchar(NonZeroU16::new(12).unwrap()),
            ])
        }

        fn statement(&mut self) -> Statement {
            let table = self.name();
            match self.below(7) {
                0 => Statement::Select(Box::new(self.select())),
                1 => Statement::CreateTable {
                    name: table,
                    columns: (0..1 + self.below(3))
                        .map(|_| ColumnDef {
                            name: self.name(),
                            data_type: self.data_type(),
                            is_not_null: self.chance(2),
                        })
                        .collect(),
                },
                2 => Statement::CreateIndex {
                    name: self.name(),
                    table,
                    column: self.name(),
                    is_unique: self.chance(2),
                },
                3 => {
                    let width = 1 + self.below(3);
                    Statement::Insert {
                        table,
                        columns: match self.chance(2) {
                            true => (0..width).map(|_| self.name()).collect(),
                            false => Vec::new(),
                        },
                        rows: (0..1 + self.below(3))
                            .map(|_| (0..width).map(|_| self.expr(2)).collect())
                            .collect(),
                    }
                }
                4 => Statement::Update {
                    table,
                    assignments: (0..1 + self.below(3))
                        .map(|_| Assignment {
                            column: self.name(),
                            value: self.expr(2),
                        })
                        .collect(),
                    selection: self.chance(2).then(|| self.expr(2)),
                },
                5 => Statement::Explain(Box::new(self.select())),
                _ => Statement::Delete {
                    table,
                    selection: self.chance(2).then(|| self.expr(2)),
                },
            }
        }
    }

    const BINARY_OPS: [BinaryOp; 14] = [
        BinaryOp::Or,
        BinaryOp::And,
        BinaryOp::Eq,
        BinaryOp::NotEq,
        BinaryOp::Lt,
        BinaryOp::LtEq,
        BinaryOp::Gt,
        BinaryOp::GtEq,
        BinaryOp::Concat,
        BinaryOp::Plus,
        BinaryOp::Minus,
        BinaryOp::Multiply,
        BinaryOp::Divide,
        BinaryOp::Modulo,
    ];

    /// The left-associative operators of the precedence test, with their source text and level.
    const CHAINABLE: [(BinaryOp, &str, u8); 8] = [
        (BinaryOp::Or, "OR", 1),
        (BinaryOp::And, "AND", 2),
        (BinaryOp::Concat, "||", 3),
        (BinaryOp::Plus, "+", 4),
        (BinaryOp::Minus, "-", 4),
        (BinaryOp::Multiply, "*", 5),
        (BinaryOp::Divide, "/", 5),
        (BinaryOp::Modulo, "%", 5),
    ];

    /// Shunting-yard over operands and left-associative operators: the tree the precedence
    /// table should give, built without the parser.
    fn shunting_yard(operands: &[Expr], operators: &[(BinaryOp, u8)]) -> Expr {
        fn reduce(output: &mut Vec<Expr>, op: BinaryOp) {
            let right = output.pop().unwrap();
            let left = output.pop().unwrap();
            output.push(Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            });
        }
        let mut output = vec![operands[0].clone()];
        let mut pending: Vec<(BinaryOp, u8)> = Vec::new();
        for (operand, &(op, level)) in operands[1..].iter().zip(operators) {
            while pending.last().is_some_and(|&(_, top)| top >= level) {
                let (top_op, _) = pending.pop().unwrap();
                reduce(&mut output, top_op);
            }
            pending.push((op, level));
            output.push(operand.clone());
        }
        while let Some((op, _)) = pending.pop() {
            reduce(&mut output, op);
        }
        output.pop().unwrap()
    }

    #[test]
    fn random_operator_chains_should_parse_like_a_shunting_yard() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

        for _ in 0..2_000 {
            let count = 1 + rng.below(8);
            let operands: Vec<Expr> = (0..count)
                .map(|index| column(&format!("c{index}")))
                .collect();
            let operators: Vec<(BinaryOp, &str, u8)> =
                (1..count).map(|_| rng.pick(&CHAINABLE)).collect();
            let mut sql = "c0".to_string();
            for (index, (_, text, _)) in operators.iter().enumerate() {
                sql.push_str(&format!(" {text} c{}", index + 1));
            }

            let parsed = expr(&sql);

            let levels: Vec<(BinaryOp, u8)> = operators
                .iter()
                .map(|&(op, _, level)| (op, level))
                .collect();
            assert_eq!(parsed, shunting_yard(&operands, &levels), "{sql}");
        }
    }

    #[test]
    fn random_statements_should_print_and_parse_back_to_themselves() {
        let mut rng = Rng(0xD6E8_FEB8_6659_FD93);

        for _ in 0..2_000 {
            let statement = rng.statement();
            let sql = statement.to_string();

            let parsed = parse(&sql).unwrap_or_else(|err| panic!("{err}\n{sql}"));

            assert_eq!(parsed, [statement], "{sql}");
        }
    }

    #[test]
    fn random_token_soup_should_never_panic() {
        const WORDS: [&str; 40] = [
            "SELECT", "FROM", "WHERE", "a", "1", "'s'", "(", ")", ",", "+", "-", "*", "AND", "OR",
            "NOT", "IS", "NULL", "JOIN", "ON", "LEFT", "BETWEEN", "IN", "CAST", "AS", "INT",
            "GROUP", "BY", "ORDER", "LIMIT", "OFFSET", "FETCH", "FIRST", "ROWS", "ONLY", ";", ".",
            "DISTINCT", "=", "<", "DATE",
        ];
        let mut rng = Rng(0xA076_1D64_78BD_642F);

        for _ in 0..20_000 {
            let sql: Vec<&str> = (0..rng.below(25)).map(|_| rng.pick(&WORDS)).collect();
            let sql = sql.join(" ");

            if let Err(err) = parse(&sql) {
                assert!(
                    ["42601", "42704", "22023", "0A000"].contains(&err.sqlstate()),
                    "{err}"
                );
            }
        }
    }
}
