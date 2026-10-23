# SQL

This document describes ferroDB's SQL layer (`src/sql/`): how the text of a statement becomes something the rest of the database can use. The **lexer** turns text into tokens and the **parser** turns tokens into an **AST**; only `SELECT` statements and expressions are parsed so far, and nothing runs them yet: the planner, which will resolve names and build executors, comes next. The execution layer is in [execution.md](execution.md).

The layer depends only on `types` (the `DataType` of a `CAST`). The planner will depend on it and on the catalog and the executors.

- [Lexer](#lexer)
  - [Tokens](#tokens)
  - [Lexical rules](#lexical-rules)
  - [Errors](#errors)
- [Parser](#parser)
  - [SELECT](#select)
  - [Expressions](#expressions)
  - [Names and reserved words](#names-and-reserved-words)
  - [Parse errors](#parse-errors)
- [AST](#ast)
- [Design decisions](#design-decisions)
- [Tests](#tests)
- [Limitations](#limitations)

---

## Lexer

`tokenize` (`lexer.rs`) turns a statement into tokens (lexical analysis; `scan.l` in PostgreSQL, generated there by flex). The lexer here is written by hand: a loop over the characters with a `match` on the first one of each token, the shape of `sqlparser-rs`.

```rust
pub fn tokenize(sql: &str) -> Result<Vec<Token>, SqlError>;

let tokens = tokenize("SELECT name FROM \"Users\" WHERE id = 7 -- one row")?;
// Word("select", SELECT), Word("name"), Word("from", FROM), QuotedIdentifier("Users"),
// Word("where", WHERE), Word("id"), Eq, Number("7")
```

### Tokens

```rust
pub struct Token { pub kind: TokenKind, pub span: Span }
pub struct Span { pub start: usize, pub end: usize }   // byte offsets: &sql[start..end]

pub enum TokenKind {
    Word { text: String, keyword: Option<Keyword> },
    QuotedIdentifier(String),
    String(String),
    Number(String),
    Plus, Minus, Star, Slash, Percent, Concat,     // + - * / % ||
    Eq, NotEq, Lt, LtEq, Gt, GtEq,                 // = <> (and !=) < <= > >=
    Comma, Semicolon, Dot, LeftParen, RightParen,
}
```

- **A word** is folded to lowercase and carries its `Keyword` when it is one of the 69 the parser will use (`SELECT`, `FROM`, `JOIN`, `INTEGER`, `NULLS`…). The keyword does not replace the text: whether a keyword may also name a table or a column (`first`, `key`, `index`) is the parser's call, in context, as PostgreSQL's grammar decides for its non-reserved keywords. The keywords live in one sorted table, searched by binary search and printed in uppercase by `Display`.
- **A quoted identifier** keeps its text and case, and is never a keyword: `"select"` is a name.
- **A number** keeps its source text (`42`, `1.`, `.5`, `3.25`, `6.02e+23`); its type is decided later, by context. A sign is always an operator: `-1` is `Minus`, `Number("1")`.
- **Spans** are byte offsets into the statement, increasing and on character boundaries; lexing the text of one span gives back that token.

### Lexical rules

| Input | Token |
|---|---|
| whitespace (`char::is_whitespace`) | none |
| `--` up to the end of the line | none |
| `/* … */`, nesting (`/* a /* b */ c */` is one comment) | none; unterminated → `UnterminatedComment` |
| a letter (`char::is_alphabetic`) or `_`, then letters, digits and `_` | `Word`, with `A`–`Z` changed to `a`–`z` and other letters as they are (`AÇÃO` → `aÇÃo`) |
| `"…"`, `""` standing for `"` | `QuotedIdentifier`; empty → `EmptyQuotedIdentifier`; unterminated → `UnterminatedQuotedIdentifier` |
| `'…'`, `''` standing for `'`, newlines included | `String`; unterminated → `UnterminatedString` |
| `digits [. [digits]] [e [+\|-] digits]`, or `. digits […]` | `Number`; an exponent without digits (`1e`) or a letter or `_` right after the number (`12abc`) → `InvalidNumber` |
| `+ - * / % \|\| = <> != < <= > >= , ; . ( )` | operator or punctuation; `.` followed by a digit starts a number; `\|` or `!` alone → `UnexpectedCharacter` |
| any other character | `UnexpectedCharacter` |

`--` inside a string or a quoted identifier is not a comment, and a quote inside a comment opens nothing.

### Errors

`SqlError` (`error.rs`); every variant is SQLSTATE **`42601`** (*syntax error*) and carries the **character** where the problem starts, counting from 1, the position PostgreSQL reports:

| Variant | Message |
|---|---|
| `UnterminatedString { position }` | `unterminated quoted string at character 8` |
| `UnterminatedQuotedIdentifier { position }` | `unterminated quoted identifier at character 8` |
| `EmptyQuotedIdentifier { position }` | `zero-length delimited identifier at character 8` |
| `UnterminatedComment { position }` | `unterminated /* comment at character 8` |
| `UnexpectedCharacter { character, position }` | `unexpected character "!" at character 8` |
| `InvalidNumber { literal, position }` | `invalid numeric literal "12abc" at character 8` |

An unterminated string, quoted identifier or comment is reported at its opening character. The parser will add its own variants to the same enum.

## Parser

`parse` (`parser.rs`) turns the tokens of a text into statements by **recursive descent**: one function per rule of the grammar (`select`, `joined_table`, `order_item`…), each consuming tokens left to right and returning a node of the AST (PostgreSQL generates its parser with bison from `gram.y`; here it is written by hand).

```rust
pub fn parse(sql: &str) -> Result<Vec<Statement>, SqlError>;

let statements = parse("SELECT name FROM users WHERE id = 7; SELECT 1;")?;   // two statements
```

Statements are separated by `;`; empty statements (`;;`) are skipped, and anything after a complete statement other than `;` is an error.

### SELECT

```
SELECT [ALL] item [, …]
  [FROM table_ref [, table_ref …]]
  [WHERE expr] [GROUP BY expr [, …]] [HAVING expr]
  [ORDER BY expr [ASC | DESC] [NULLS FIRST | NULLS LAST] [, …]]
  [LIMIT expr | LIMIT ALL] [OFFSET expr [ROW | ROWS]]
  [FETCH {FIRST | NEXT} [expr] {ROW | ROWS} ONLY]
item      := * | name.* | expr [[AS] alias]
table_ref := name [[AS] alias]
           | table_ref [INNER | LEFT [OUTER] | RIGHT [OUTER] | FULL [OUTER]] JOIN table_ref ON expr
           | table_ref CROSS JOIN table_ref
           | ( table_ref )
```

- **Joins** are read left to right: `a JOIN b ON x JOIN c ON y` is `(a JOIN b) JOIN c`; parentheses group them otherwise. A comma in `FROM` is a `CROSS JOIN`. Every join but `CROSS` needs `ON`.
- **`LIMIT`, `OFFSET` and `FETCH`** may come in any order, each once; `LIMIT` and `FETCH` both set the count, so only one of them may appear. `FETCH FIRST ROW ONLY` without a number is 1; `LIMIT ALL` is no limit. `LIMIT` is PostgreSQL's; `OFFSET … FETCH FIRST` is the standard's form.
- **`SELECT DISTINCT`** is refused with `Unsupported` (`0A000`); `SELECT ALL` is the default.

### Expressions

Precedence climbing (Pratt): `expression_from(min)` reads an operand, then every operator that binds at least as tightly as `min`, reading its right side one level tighter, which makes operators left-associative. The table is PostgreSQL's order, loosest first:

| Level | Operators | Note |
|---|---|---|
| 1 | `OR` | left |
| 2 | `AND` | left |
| 3 | `NOT` | prefix |
| 4 | `IS [NOT] NULL / TRUE / FALSE / UNKNOWN`, `IS [NOT] DISTINCT FROM` | suffix |
| 5 | `= <> != < <= > >=` | **do not associate**: `a < b < c` is an error |
| 6 | `[NOT] BETWEEN … AND …`, `[NOT] IN (…)` | the bounds of `BETWEEN` are read at level 7, so `x BETWEEN 1 AND 2 AND y` is `(x BETWEEN 1 AND 2) AND y` |
| 7 | `\|\|` | left |
| 8 | `+ -` | left |
| 9 | `* / %` | left |
| 10 | prefix `-` and `+` | |

Operands: a number, a string, `TRUE`, `FALSE`, `NULL`, `DATE '…'` and `TIMESTAMP '…'` (without the string, `date` is a name), a column (`a`, `t.a`, `"T"."A"`), `( expr )`, `CAST(expr AS type)` and a function call (`count(*)`, `sum(x)`, `f()`). A sign is never folded into a number: `-1` is `Minus` applied to `1`, and the planner does the arithmetic (it needs to, for `-9223372036854775808`).

**Types of a `CAST`:** `SMALLINT`, `INTEGER` or `INT`, `BIGINT`, `REAL`, `DOUBLE PRECISION`, `BOOLEAN`, `VARCHAR(n)` or `CHARACTER VARYING(n)`, `VARCHAR` without a length (the widest, `VARCHAR(65535)`), `DATE`, `TIMESTAMP`, straight into `DataType`. A length outside 1..=65535, or not an integer, is `InvalidVarcharLength` (`22023`); any other type name (`text`, `char`) is `UndefinedType` (`42704`).

### Names and reserved words

The lexer marks keywords; the parser decides which ones may name a table, a column or an alias (`is_reserved`):

- **reserved**, a name only when quoted: `all and as asc between by cast create cross default delete desc distinct drop false fetch from full group having in inner insert into is join left limit not null offset on or order outer right select set table true unique update values where`;
- **non-reserved**, valid names: `bigint boolean char character date double first index int integer key last next nulls only precision primary real row rows smallint timestamp unknown varchar varying`, so columns named `date`, `key`, `first` or `rows` work, as in PostgreSQL.

After `AS`, a column alias may be any word, a keyword included (`SELECT 1 AS from`), as in PostgreSQL; a bare alias (`SELECT a b`) and a table alias must be non-reserved.

### Parse errors

| Variant | SQLSTATE | Message |
|---|---|---|
| `UnexpectedToken { found, expected, position }` | `42601` | `syntax error at character 8: expected an expression, found "FROM"` |
| `UnexpectedEnd { expected }` | `42601` | `syntax error at end of input: expected ")"` |
| `UndefinedType { name, position }` | `42704` | `type "text" does not exist at character 16` |
| `InvalidVarcharLength { length, position }` | `22023` | `length for type varchar must be between 1 and 65535, got 0 at character 20` |
| `Unsupported { feature, position }` | `0A000` | `SELECT DISTINCT is not supported at character 8` |
| `TooDeep { position }` | `54001` | `statement is nested too deeply at character 210` |

`found` is the token's source text as written; the position is its first character.

**Nesting is limited** to `MAX_NESTING` (200) levels of expressions and table references: parentheses, signs, `NOT`, function arguments and parenthesized joins each open one. The parser recurses once per level, and so do `Display`, dropping the tree and the planner to come, so without a limit a long enough `((((…))))` would exhaust the stack and abort the process; past it, `TooDeep` (`54001`, *statement too complex*), as PostgreSQL does when it reaches `max_stack_depth`.

## AST

`ast.rs` holds the statement as written, with **names** rather than column positions (the executors' `Expr` uses positions; the planner will translate):

```rust
pub enum Statement { Select(Select) }
pub struct Select {
    pub projection: Vec<SelectItem>, pub from: Option<TableRef>, pub selection: Option<Expr>,
    pub group_by: Vec<Expr>, pub having: Option<Expr>, pub order_by: Vec<OrderItem>,
    pub limit: Option<Expr>, pub offset: Option<Expr>,
}
pub enum SelectItem { Expr { expr: Expr, alias: Option<String> }, Wildcard, QualifiedWildcard(String) }
pub enum TableRef { Table { name, alias }, Join { left, right, kind: JoinKind, on: Option<Expr> } }
pub enum Expr { Column { table, name }, Literal(Literal), Unary { op, expr }, Binary { op, left, right },
                IsNull { .. }, Is { .. }, IsDistinctFrom { .. }, Between { .. }, InList { .. },
                Cast { expr, to: DataType }, Function { name, args: FunctionArgs } }
pub enum Literal { Number(String), String(String), Boolean(bool), Null, Date(String), Timestamp(String) }
```

Literals keep their text where the planner gives them a type: a number may become an `INTEGER`, a `BIGINT` or a `DOUBLE PRECISION`, and `DATE '…'` is checked by `Value::parse`.

**`Display`** prints canonical SQL: keywords in uppercase, a name bare when the lexer would read it back unchanged (a word with no ASCII capital that is not reserved) and quoted otherwise, strings with `''`, and **every compound expression in parentheses** (`(a + (b * c))`). The parser reads that text back to the same tree, whatever the precedence; it will also serve the planner's messages and a future `EXPLAIN`.

## Design decisions

- **Unquoted identifiers fold to lowercase** (decided with the author), as PostgreSQL, instead of the standard's uppercase: see [ADR 0012](decisions/0012-lowercase-folding-of-unquoted-identifiers.md).
- **A lexer written by hand**, instead of a generator (flex, `logos`) or regular expressions: no new dependency, and the loop is short and readable.
- **Keywords marked, text kept** (`Word { text, keyword }`), instead of a keyword token: the parser decides what is reserved where, so a column named `first` or `key` stays possible, and adding a keyword never breaks a name.
- **Numbers kept as text**, instead of converted by the lexer: there is no `NUMERIC` yet, and the type depends on context.
- **Spans in bytes, error positions in characters:** bytes slice the `&str` for free; people count characters.
- **`!=` accepted** as `<>` (decided with the author), like PostgreSQL, MySQL and SQLite.
- **Whitespace by `char::is_whitespace`**, instead of PostgreSQL's ASCII set: the standard admits Unicode spaces.
- **One error enum for the layer** (`SqlError`), which the parser will extend.
- **Parser scope: expressions and `SELECT`** (decided with the author); `INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE`, `CREATE INDEX` and `DROP TABLE` come next, on the same parser.
- **Recursive descent by hand, precedence climbing for expressions**, instead of a generator (bison, `lalrpop`) or one rule per level: no dependency, and the precedence lives in one table.
- **An AST with names, nothing resolved**, instead of building the executors' expressions directly: syntax stays testable without a database, and the planner validates names and types in a step of its own.
- **`DataType` in a `CAST`**, instead of a type name of the AST's own: the parser already checks the `VARCHAR` length, and `sql` depending on `types` points downward.
- **Numbers as text, signs not folded:** the type and the sign (`-9223372036854775808`) are the planner's to decide.
- **Reserved words decided by the parser**, with a short version of PostgreSQL's list: columns named `date`, `key` or `rows` stay possible.
- **Comparisons that do not associate**, as the standard and PostgreSQL, instead of left associativity: `a < b < c` is almost always a mistake.
- **A canonical `Display` with every parenthesis**, instead of minimal parentheses: printing and parsing back is right by construction.
- **`LIMIT` and `FETCH FIRST` both accepted:** the first is the custom, the second the standard.
- **One character-position function** (`character_position`) for the lexer and the parser.
- **The cursor moves by whole characters** (`char::len_utf8`) and reads through `str::get`, so it never slices off a character boundary and never panics.

## Tests

- `error.rs`: every error with its message and SQLSTATE.
- `lexer.rs`: every operator and punctuation mark, `!=` as `<>`, operators without spaces (`<=>=<>=`), empty and blank input, spans, `|` and `!` alone, error positions counting characters after multi-byte spaces; words folded to lowercase and marked as keywords, words that are not keywords and words that only resemble one (`selects`, `_select`, `nullss`), letters, digits and `_`, non-ASCII letters kept, every keyword round-tripping through its name, the keyword table sorted; quoted identifiers keeping their case, never keywords, with `""`, next to a dot, empty and unterminated; strings with `''`, empty, across lines, with comment markers and accents, unterminated; numbers in every form (also `1.e5` and `1.E-2`), signs as operators, a dot starting a number only before a digit (`t.c1`, `t.5`, `1..2`), invalid literals (`12abc`, `3x`, `1.5_`, `1e`, `1e+`, `2.5E-x`, `1.a`); control characters (`\0`, BEL) as unexpected; line comments (also without a final newline, `1--2`), block comments (empty, nested, with `*`, `/` and `--` inside), unterminated ones, comment markers inside quotes and quotes inside comments.
- `random_tokens_written_out_should_lex_back_with_their_spans`: 2,000 statements of up to 11 random tokens (symbols, keywords in random case, words, quoted identifiers and strings with doubled quotes, numbers) separated by random whitespace and comments (nested ones included); `tokenize` must give back the same tokens with spans on their source text.
- `random_text_should_never_panic_and_spans_should_hold_their_tokens`: 20,000 random texts over an alphabet of quotes, comment and operator characters, digits, letters and 2- to 4-byte characters; the lexer never panics, an error is always a `42601` with a position, and on success the spans are increasing, on character boundaries, and each one lexes back to its token.
- Mutations checked by hand, each caught: block comments that do not nest, and a doubled quote not standing for one.
- Integration in [`tests/sql_lexer.rs`](../tests/sql_lexer.rs): a `SELECT` with a join, `GROUP BY`, `HAVING`, `ORDER BY … NULLS LAST`, `LIMIT` and `OFFSET`; a `CREATE TABLE` with types and constraints; an `INSERT` with a string holding `''`, accents and a newline; a script of three statements with line and nested block comments; errors deep in a statement reported at the right character.
- `ast.rs`: literals (strings with `''`, typed literals), names quoted only when needed (capitals, reserved words, spaces, quotes, a leading digit; non-reserved keywords and non-ASCII letters bare), every compound expression in parentheses, a `SELECT` with every clause, a minimal one with a `CROSS JOIN`.
- `parser.rs`: every kind of literal, `date` and `rows` as names, qualified and quoted columns, parentheses, function calls with `*`, nothing or arguments, `CAST` to every type, `VARCHAR` lengths out of range (`22023`) and unknown types (`42704`), missing operands, unclosed parentheses, a reserved word as a column, a statement not starting with `SELECT`; incomplete type names (`DOUBLE` without `PRECISION`, `CHARACTER(5)`, `VARCHAR()`, `VARCHAR(-1)`), an empty `IN ()`, `FETCH` missing `FIRST`, `ROWS` or `ONLY`; nesting exactly at the limit and one level past it for parentheses, signs, `NOT` and parenthesized joins (`54001`, at the right character), and 10,000 levels of each on a 2 MB stack failing with `54001` instead of overflowing; each precedence level and associativity, `NOT`, `IS` tests, `BETWEEN` followed by `AND`, `IN`, signs, chained comparisons (`42601`), incomplete operators; wildcards and aliases (`AS` with a keyword), tables with aliases and commas, every join kind with and without `OUTER`, `CROSS JOIN`, parenthesized joins, a join without `ON`, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY` directions and null positions, every form of `LIMIT`, `OFFSET` and `FETCH` and a count given twice, statements and empty statements, `SELECT DISTINCT` (`0A000`) and `SELECT ALL`, non-reserved keywords as names, clauses out of order or incomplete.
- `random_operator_chains_should_parse_like_a_shunting_yard`: 2,000 chains of up to 8 operands joined by `OR`, `AND`, `||`, `+`, `-`, `*`, `/` and `%` without parentheses; the tree must equal the one a shunting-yard built in the test with the same levels gives.
- `random_selects_should_print_and_parse_back_to_themselves`: 1,000 random `SELECT`s (every expression kind to depth 3, joins to depth 2, every clause, names that need quotes or are non-reserved keywords); printing and parsing back gives the same AST.
- `random_token_soup_should_never_panic`: 20,000 random sequences of keywords, names, literals and symbols; the parser never panics and every error has one of the layer's SQLSTATEs.
- Mutations checked by hand, each caught: `+` and `-` at the level of `*` and `/`, and reserved words printed without quotes (by the random tests); the nesting limit off by one (by the boundary test).
- Integration in [`tests/sql_parser.rs`](../tests/sql_parser.rs): a `SELECT` with a join, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY … NULLS LAST`, `LIMIT` and `OFFSET` checked clause by clause; the `OFFSET … FETCH NEXT` form; a script with comments; a complex query printed and parsed back; errors deep in a query with their positions and SQLSTATEs.

## Limitations

- **Only `SELECT` is parsed**, and nothing runs it: there is no planner yet, and `INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE`, `CREATE INDEX` and `DROP TABLE` come next.
- **Not parsed yet:** `SELECT DISTINCT` (refused with `0A000`), `UNION`/`INTERSECT`/`EXCEPT`, subqueries, `WITH`, `CASE`, `LIKE`, `JOIN … USING`, `NATURAL JOIN`, window functions, `COUNT(DISTINCT x)`, the boolean literal `UNKNOWN`.
- **At most 200 levels of nesting** (`54001` beyond), a fixed count where PostgreSQL measures the stack (`max_stack_depth`).
- **Not lexed:** strings with a prefix (`E'…'`, `N'…'`, `U&'…'`, `X'…'`, `B'…'`), the standard's concatenation of string literals separated by a newline, parameters (`$1`, `?`), PostgreSQL operators (`::`, `~`, `->`) and dollar quoting (`$$…$$`).
- **No identifier length check here:** names longer than 63 bytes are refused by the catalog (`42622`).
- **The whole statement in memory:** no incremental lexing of a stream.
