//! The abstract syntax tree: statements as written, with names rather than column positions.

use std::fmt;

use crate::types::DataType;

use super::Keyword;
use super::parser::is_reserved;

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Select(Box<Select>),
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
    },
    /// Only a `UNIQUE` index runs; a plain `CREATE INDEX` parses and the planner refuses it.
    CreateIndex {
        name: String,
        table: String,
        column: String,
        is_unique: bool,
    },
    /// `columns` is empty when the statement names none: every column, in table order.
    Insert {
        table: String,
        columns: Vec<String>,
        rows: Vec<Vec<Expr>>,
    },
    Update {
        table: String,
        assignments: Vec<Assignment>,
        selection: Option<Expr>,
    },
    Delete {
        table: String,
        selection: Option<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub is_not_null: bool,
}

/// `column = value` in an `UPDATE`.
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: String,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub projection: Vec<SelectItem>,
    pub from: Option<TableRef>,
    /// `WHERE`.
    pub selection: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderItem>,
    /// `LIMIT n` or `FETCH FIRST n ROWS ONLY`; `LIMIT ALL` is `None`.
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Expr {
        expr: Expr,
        alias: Option<String>,
    },
    /// `*`
    Wildcard,
    /// `t.*`
    QualifiedWildcard(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableRef {
    Table {
        name: String,
        alias: Option<String>,
    },
    /// `on` is `None` only for `Cross`.
    Join {
        left: Box<TableRef>,
        right: Box<TableRef>,
        kind: JoinKind,
        on: Option<Expr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub expr: Expr,
    pub is_descending: bool,
    /// `None` without `NULLS FIRST` / `NULLS LAST`.
    pub nulls_first: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Column {
        table: Option<String>,
        name: String,
    },
    Literal(Literal),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// `IS [NOT] TRUE / FALSE / UNKNOWN`.
    Is {
        expr: Box<Expr>,
        test: BooleanTest,
        negated: bool,
    },
    IsDistinctFrom {
        left: Box<Expr>,
        right: Box<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Cast {
        expr: Box<Expr>,
        to: DataType,
    },
    Function {
        name: String,
        args: FunctionArgs,
    },
}

/// Literals keep their text where the planner gives them a type: a number may become an
/// INTEGER, a BIGINT or a DOUBLE PRECISION, and `DATE '…'` is checked by `Value::parse`.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Number(String),
    String(String),
    Boolean(bool),
    Null,
    Date(String),
    Timestamp(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Minus,
    Plus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    And,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Concat,
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BooleanTest {
    True,
    False,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FunctionArgs {
    /// `count(*)`
    Star,
    List(Vec<Expr>),
}

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Statement::Select(select) => write!(f, "{select}"),
            Statement::CreateTable { name, columns } => {
                write!(f, "CREATE TABLE {} (", Name(name))?;
                write_list(f, columns)?;
                write!(f, ")")
            }
            Statement::CreateIndex {
                name,
                table,
                column,
                is_unique,
            } => {
                let unique = if *is_unique { "UNIQUE " } else { "" };
                write!(
                    f,
                    "CREATE {unique}INDEX {} ON {} ({})",
                    Name(name),
                    Name(table),
                    Name(column)
                )
            }
            Statement::Insert {
                table,
                columns,
                rows,
            } => {
                write!(f, "INSERT INTO {}", Name(table))?;
                if !columns.is_empty() {
                    let names: Vec<Name<'_>> = columns.iter().map(|column| Name(column)).collect();
                    write!(f, " (")?;
                    write_list(f, &names)?;
                    write!(f, ")")?;
                }
                write!(f, " VALUES ")?;
                for (index, row) in rows.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "(")?;
                    write_list(f, row)?;
                    write!(f, ")")?;
                }
                Ok(())
            }
            Statement::Update {
                table,
                assignments,
                selection,
            } => {
                write!(f, "UPDATE {} SET ", Name(table))?;
                write_list(f, assignments)?;
                write_where(f, selection.as_ref())
            }
            Statement::Delete { table, selection } => {
                write!(f, "DELETE FROM {}", Name(table))?;
                write_where(f, selection.as_ref())
            }
        }
    }
}

impl fmt::Display for ColumnDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", Name(&self.name), self.data_type)?;
        if self.is_not_null {
            write!(f, " NOT NULL")?;
        }
        Ok(())
    }
}

impl fmt::Display for Assignment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = {}", Name(&self.column), self.value)
    }
}

fn write_where(f: &mut fmt::Formatter<'_>, selection: Option<&Expr>) -> fmt::Result {
    match selection {
        Some(selection) => write!(f, " WHERE {selection}"),
        None => Ok(()),
    }
}

impl fmt::Display for Select {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SELECT ")?;
        write_list(f, &self.projection)?;
        if let Some(from) = &self.from {
            write!(f, " FROM {from}")?;
        }
        if let Some(selection) = &self.selection {
            write!(f, " WHERE {selection}")?;
        }
        if !self.group_by.is_empty() {
            write!(f, " GROUP BY ")?;
            write_list(f, &self.group_by)?;
        }
        if let Some(having) = &self.having {
            write!(f, " HAVING {having}")?;
        }
        if !self.order_by.is_empty() {
            write!(f, " ORDER BY ")?;
            write_list(f, &self.order_by)?;
        }
        if let Some(limit) = &self.limit {
            write!(f, " LIMIT {limit}")?;
        }
        if let Some(offset) = &self.offset {
            write!(f, " OFFSET {offset}")?;
        }
        Ok(())
    }
}

impl fmt::Display for SelectItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelectItem::Expr { expr, alias } => {
                write!(f, "{expr}")?;
                if let Some(alias) = alias {
                    write!(f, " AS {}", Name(alias))?;
                }
                Ok(())
            }
            SelectItem::Wildcard => write!(f, "*"),
            SelectItem::QualifiedWildcard(table) => write!(f, "{}.*", Name(table)),
        }
    }
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableRef::Table { name, alias } => {
                write!(f, "{}", Name(name))?;
                if let Some(alias) = alias {
                    write!(f, " AS {}", Name(alias))?;
                }
                Ok(())
            }
            TableRef::Join {
                left,
                right,
                kind,
                on,
            } => {
                let join = match kind {
                    JoinKind::Inner => "JOIN",
                    JoinKind::Left => "LEFT JOIN",
                    JoinKind::Right => "RIGHT JOIN",
                    JoinKind::Full => "FULL JOIN",
                    JoinKind::Cross => "CROSS JOIN",
                };
                write!(f, "({left} {join} {right}")?;
                if let Some(on) = on {
                    write!(f, " ON {on}")?;
                }
                write!(f, ")")
            }
        }
    }
}

impl fmt::Display for OrderItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.expr)?;
        if self.is_descending {
            write!(f, " DESC")?;
        }
        match self.nulls_first {
            Some(true) => write!(f, " NULLS FIRST"),
            Some(false) => write!(f, " NULLS LAST"),
            None => Ok(()),
        }
    }
}

/// Every compound expression in parentheses, so the text parses back to the same tree
/// whatever the precedence of its operators.
impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Column { table, name } => {
                if let Some(table) = table {
                    write!(f, "{}.", Name(table))?;
                }
                write!(f, "{}", Name(name))
            }
            Expr::Literal(literal) => write!(f, "{literal}"),
            Expr::Unary { op, expr } => {
                let op = match op {
                    UnaryOp::Not => "NOT",
                    UnaryOp::Minus => "-",
                    UnaryOp::Plus => "+",
                };
                write!(f, "({op} {expr})")
            }
            Expr::Binary { op, left, right } => write!(f, "({left} {} {right})", op.symbol()),
            Expr::IsNull { expr, negated } => write!(f, "({expr} IS {}NULL)", not(*negated)),
            Expr::Is {
                expr,
                test,
                negated,
            } => {
                let test = match test {
                    BooleanTest::True => "TRUE",
                    BooleanTest::False => "FALSE",
                    BooleanTest::Unknown => "UNKNOWN",
                };
                write!(f, "({expr} IS {}{test})", not(*negated))
            }
            Expr::IsDistinctFrom {
                left,
                right,
                negated,
            } => write!(f, "({left} IS {}DISTINCT FROM {right})", not(*negated)),
            Expr::Between {
                expr,
                low,
                high,
                negated,
            } => write!(f, "({expr} {}BETWEEN {low} AND {high})", not(*negated)),
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                write!(f, "({expr} {}IN (", not(*negated))?;
                write_list(f, list)?;
                write!(f, "))")
            }
            Expr::Cast { expr, to } => write!(f, "CAST({expr} AS {to})"),
            Expr::Function { name, args } => {
                write!(f, "{}(", Name(name))?;
                match args {
                    FunctionArgs::Star => write!(f, "*")?,
                    FunctionArgs::List(args) => write_list(f, args)?,
                }
                write!(f, ")")
            }
        }
    }
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Number(text) => write!(f, "{text}"),
            Literal::String(text) => write!(f, "{}", Quoted('\'', text)),
            Literal::Boolean(true) => write!(f, "TRUE"),
            Literal::Boolean(false) => write!(f, "FALSE"),
            Literal::Null => write!(f, "NULL"),
            Literal::Date(text) => write!(f, "DATE {}", Quoted('\'', text)),
            Literal::Timestamp(text) => write!(f, "TIMESTAMP {}", Quoted('\'', text)),
        }
    }
}

impl BinaryOp {
    fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Or => "OR",
            BinaryOp::And => "AND",
            BinaryOp::Eq => "=",
            BinaryOp::NotEq => "<>",
            BinaryOp::Lt => "<",
            BinaryOp::LtEq => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::GtEq => ">=",
            BinaryOp::Concat => "||",
            BinaryOp::Plus => "+",
            BinaryOp::Minus => "-",
            BinaryOp::Multiply => "*",
            BinaryOp::Divide => "/",
            BinaryOp::Modulo => "%",
        }
    }
}

fn not(negated: bool) -> &'static str {
    if negated { "NOT " } else { "" }
}

fn write_list<T: fmt::Display>(f: &mut fmt::Formatter<'_>, items: &[T]) -> fmt::Result {
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            write!(f, ", ")?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
}

/// A name, bare when the lexer would read it back unchanged (a word with no ASCII capital
/// that is not a reserved keyword), quoted otherwise.
struct Name<'a>(&'a str);

impl fmt::Display for Name<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.0;
        let mut chars = name.chars();
        let is_word = chars
            .next()
            .is_some_and(|first| first.is_alphabetic() || first == '_')
            && chars.all(|next| next.is_alphanumeric() || next == '_');
        let has_capital = name.chars().any(|letter| letter.is_ascii_uppercase());
        let is_reserved = Keyword::from_word(name).is_some_and(is_reserved);
        if is_word && !has_capital && !is_reserved {
            write!(f, "{name}")
        } else {
            write!(f, "{}", Quoted('"', name))
        }
    }
}

/// Text between `quote`s, each quote inside doubled.
struct Quoted<'a>(char, &'a str);

impl fmt::Display for Quoted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Quoted(quote, text) = *self;
        let doubled: String = [quote, quote].iter().collect();
        write!(f, "{quote}{}{quote}", text.replace(quote, &doubled))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU16;

    fn column(name: &str) -> Expr {
        Expr::Column {
            table: None,
            name: name.to_string(),
        }
    }

    fn number(text: &str) -> Expr {
        Expr::Literal(Literal::Number(text.to_string()))
    }

    fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
        Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    #[test]
    fn literals_should_print_as_sql() {
        let cases = [
            (number("6.02e+23"), "6.02e+23"),
            (
                Expr::Literal(Literal::String("it's".to_string())),
                "'it''s'",
            ),
            (Expr::Literal(Literal::Boolean(true)), "TRUE"),
            (Expr::Literal(Literal::Null), "NULL"),
            (
                Expr::Literal(Literal::Date("2026-10-23".to_string())),
                "DATE '2026-10-23'",
            ),
            (
                Expr::Literal(Literal::Timestamp("2026-10-23 10:00:00".to_string())),
                "TIMESTAMP '2026-10-23 10:00:00'",
            ),
        ];

        for (expr, sql) in cases {
            assert_eq!(expr.to_string(), sql);
        }
    }

    #[test]
    fn names_should_be_quoted_only_when_needed() {
        let cases = [
            ("users", "users"),
            ("_n2", "_n2"),
            ("ação", "ação"),
            ("Users", "\"Users\""),
            ("select", "\"select\""),
            ("date", "date"),
            ("two words", "\"two words\""),
            ("a\"b", "\"a\"\"b\""),
            ("2nd", "\"2nd\""),
        ];

        for (name, sql) in cases {
            assert_eq!(column(name).to_string(), sql, "{name}");
        }
        let qualified = Expr::Column {
            table: Some("T".to_string()),
            name: "a".to_string(),
        };
        assert_eq!(qualified.to_string(), "\"T\".a");
    }

    #[test]
    fn compound_expressions_should_print_in_parentheses() {
        let sum = binary(BinaryOp::Plus, column("a"), number("1"));
        let negated = Expr::Unary {
            op: UnaryOp::Minus,
            expr: Box::new(Expr::Unary {
                op: UnaryOp::Minus,
                expr: Box::new(number("1")),
            }),
        };
        let cases = [
            (
                binary(BinaryOp::Or, sum.clone(), column("b")),
                "((a + 1) OR b)",
            ),
            (negated, "(- (- 1))"),
            (
                Expr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(column("b")),
                },
                "(NOT b)",
            ),
            (
                Expr::IsNull {
                    expr: Box::new(column("a")),
                    negated: true,
                },
                "(a IS NOT NULL)",
            ),
            (
                Expr::Is {
                    expr: Box::new(column("a")),
                    test: BooleanTest::Unknown,
                    negated: false,
                },
                "(a IS UNKNOWN)",
            ),
            (
                Expr::IsDistinctFrom {
                    left: Box::new(column("a")),
                    right: Box::new(column("b")),
                    negated: true,
                },
                "(a IS NOT DISTINCT FROM b)",
            ),
            (
                Expr::Between {
                    expr: Box::new(column("a")),
                    low: Box::new(number("1")),
                    high: Box::new(sum),
                    negated: true,
                },
                "(a NOT BETWEEN 1 AND (a + 1))",
            ),
            (
                Expr::InList {
                    expr: Box::new(column("a")),
                    list: vec![number("1"), number("2")],
                    negated: false,
                },
                "(a IN (1, 2))",
            ),
            (
                Expr::Cast {
                    expr: Box::new(column("a")),
                    to: DataType::Varchar(NonZeroU16::new(5).unwrap()),
                },
                "CAST(a AS VARCHAR(5))",
            ),
            (
                Expr::Function {
                    name: "count".to_string(),
                    args: FunctionArgs::Star,
                },
                "count(*)",
            ),
            (
                Expr::Function {
                    name: "f".to_string(),
                    args: FunctionArgs::List(vec![column("a"), number("2")]),
                },
                "f(a, 2)",
            ),
        ];

        for (expr, sql) in cases {
            assert_eq!(expr.to_string(), sql);
        }
    }

    #[test]
    fn a_select_should_print_every_clause() {
        let select = Select {
            projection: vec![
                SelectItem::QualifiedWildcard("c".to_string()),
                SelectItem::Expr {
                    expr: Expr::Function {
                        name: "count".to_string(),
                        args: FunctionArgs::Star,
                    },
                    alias: Some("Total".to_string()),
                },
            ],
            from: Some(TableRef::Join {
                left: Box::new(TableRef::Table {
                    name: "customers".to_string(),
                    alias: Some("c".to_string()),
                }),
                right: Box::new(TableRef::Table {
                    name: "orders".to_string(),
                    alias: None,
                }),
                kind: JoinKind::Left,
                on: Some(binary(BinaryOp::Eq, column("id"), column("customer_id"))),
            }),
            selection: Some(binary(BinaryOp::Gt, column("amount"), number("10"))),
            group_by: vec![column("id")],
            having: Some(binary(BinaryOp::Gt, column("n"), number("2"))),
            order_by: vec![
                OrderItem {
                    expr: column("n"),
                    is_descending: true,
                    nulls_first: Some(false),
                },
                OrderItem {
                    expr: column("id"),
                    is_descending: false,
                    nulls_first: None,
                },
            ],
            limit: Some(number("10")),
            offset: Some(number("20")),
        };

        let sql = Statement::Select(Box::new(select)).to_string();

        assert_eq!(
            sql,
            "SELECT c.*, count(*) AS \"Total\" \
             FROM (customers AS c LEFT JOIN orders ON (id = customer_id)) \
             WHERE (amount > 10) GROUP BY id HAVING (n > 2) \
             ORDER BY n DESC NULLS LAST, id LIMIT 10 OFFSET 20"
        );
    }

    #[test]
    fn a_minimal_select_and_a_cross_join_should_print() {
        let constant = Select {
            projection: vec![SelectItem::Wildcard],
            from: Some(TableRef::Join {
                left: Box::new(TableRef::Table {
                    name: "a".to_string(),
                    alias: None,
                }),
                right: Box::new(TableRef::Table {
                    name: "b".to_string(),
                    alias: None,
                }),
                kind: JoinKind::Cross,
                on: None,
            }),
            selection: None,
            group_by: vec![],
            having: None,
            order_by: vec![],
            limit: None,
            offset: Some(number("5")),
        };

        let sql = Statement::Select(Box::new(constant)).to_string();

        assert_eq!(sql, "SELECT * FROM (a CROSS JOIN b) OFFSET 5");
    }

    #[test]
    fn ddl_statements_should_print_as_sql() {
        let create_table = Statement::CreateTable {
            name: "Users".to_string(),
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Integer,
                    is_not_null: true,
                },
                ColumnDef {
                    name: "date".to_string(),
                    data_type: DataType::Varchar(NonZeroU16::new(5).unwrap()),
                    is_not_null: false,
                },
            ],
        };
        let index = |is_unique| Statement::CreateIndex {
            name: "users_id".to_string(),
            table: "Users".to_string(),
            column: "id".to_string(),
            is_unique,
        };

        assert_eq!(
            create_table.to_string(),
            "CREATE TABLE \"Users\" (id INTEGER NOT NULL, date VARCHAR(5))"
        );
        assert_eq!(
            index(true).to_string(),
            "CREATE UNIQUE INDEX users_id ON \"Users\" (id)"
        );
        assert_eq!(
            index(false).to_string(),
            "CREATE INDEX users_id ON \"Users\" (id)"
        );
    }

    #[test]
    fn dml_statements_should_print_as_sql() {
        let insert = |columns: Vec<&str>| Statement::Insert {
            table: "t".to_string(),
            columns: columns.into_iter().map(str::to_string).collect(),
            rows: vec![
                vec![number("1"), column("a")],
                vec![number("2"), number("3")],
            ],
        };
        let update = Statement::Update {
            table: "t".to_string(),
            assignments: vec![
                Assignment {
                    column: "v".to_string(),
                    value: binary(BinaryOp::Plus, column("v"), number("1")),
                },
                Assignment {
                    column: "select".to_string(),
                    value: number("0"),
                },
            ],
            selection: Some(binary(BinaryOp::Gt, column("id"), number("2"))),
        };
        let delete = |selection| Statement::Delete {
            table: "t".to_string(),
            selection,
        };

        assert_eq!(
            insert(vec![]).to_string(),
            "INSERT INTO t VALUES (1, a), (2, 3)"
        );
        assert_eq!(
            insert(vec!["id", "Name"]).to_string(),
            "INSERT INTO t (id, \"Name\") VALUES (1, a), (2, 3)"
        );
        assert_eq!(
            update.to_string(),
            "UPDATE t SET v = (v + 1), \"select\" = 0 WHERE (id > 2)"
        );
        assert_eq!(delete(None).to_string(), "DELETE FROM t");
        assert_eq!(
            delete(Some(column("gone"))).to_string(),
            "DELETE FROM t WHERE gone"
        );
    }
}
