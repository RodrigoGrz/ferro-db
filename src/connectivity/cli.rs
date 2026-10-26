//! The command-line client: a read → run → print loop over a database embedded in the process.
//!
//! Lines gather until they make a complete command: the lexer reads them whole and their last
//! token is `;`, so a `;` inside a string, a quoted name or a comment never ends one. A line
//! that starts with `\` is a meta-command of the client, unless it is inside such a string or
//! comment.

use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::catalog::{Catalog, CatalogError};
use crate::config::PAGE_SIZE;
use crate::execution::ExecutionContext;
use crate::planner::{PlannerError, StatementOutcome, run_statement};
use crate::sql::{SqlError, TokenKind, parse, tokenize};
use crate::storage::buffer_pool::{BufferPoolError, BufferPoolManager};
use crate::types::Value;

use super::output;

/// The file `ferro-db` opens without an argument.
pub const DEFAULT_DATABASE: &str = "data.db";

const POOL_SIZE: usize = 64;
const BANNER: &str = "ferro-db: type \\? for help, \\q to quit";
const PROMPT: &str = "ferro=> ";
const CONTINUATION: &str = "ferro-> ";
const HELP: &str = "SQL commands end with ;\n\
                    \\d          list the tables\n\
                    \\d TABLE    describe a table: its columns and indexes\n\
                    \\i FILE     run the commands of a file\n\
                    \\?          show this help\n\
                    \\q          quit";
/// How many `\i` may run inside one another, so that a file that includes itself stops.
const MAX_INCLUDE_DEPTH: usize = 16;

/// How to call `ferro-db`.
pub const USAGE: &str = "usage: ferro-db [FILE]\n\n\
                         Opens the database in FILE (data.db by default), creating it if it does \
                         not exist,\nand reads SQL from standard input: with prompts from a \
                         terminal, as a script otherwise.";

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Open the database in this file.
    Open(PathBuf),
    Help,
}

/// What the arguments after the program's name ask for, or what is wrong with them. Anything
/// that looks like an option is never taken for a file, so `ferro-db --help` does not create a
/// database named `--help`.
pub fn parse_arguments(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<Invocation, String> {
    let mut file = None;
    for argument in arguments {
        if let Some(text) = argument.to_str() {
            if text == "-h" || text == "--help" {
                return Ok(Invocation::Help);
            }
            if text.len() > 1 && text.starts_with('-') {
                return Err(format!("unknown option {text}"));
            }
        }
        if file.is_some() {
            return Err("too many arguments".to_string());
        }
        file = Some(PathBuf::from(argument));
    }
    Ok(Invocation::Open(
        file.unwrap_or_else(|| PathBuf::from(DEFAULT_DATABASE)),
    ))
}

/// An open database: its buffer pool and its catalog.
pub struct Session {
    bpm: BufferPoolManager,
    catalog: Catalog,
}

impl Session {
    pub fn open(path: &Path) -> Result<Session, CliError> {
        let mut bpm = BufferPoolManager::new(POOL_SIZE, PAGE_SIZE, path)?;
        let catalog = Catalog::open(&mut bpm)?;
        Ok(Session { bpm, catalog })
    }
}

/// Reads commands from `input` until it ends or `\q`, writing what they give to `out` and
/// their errors to `err`; `interactive` adds the banner and the prompts.
pub fn run(
    session: &mut Session,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
    interactive: bool,
) -> io::Result<()> {
    let mut cli = Cli {
        session,
        out,
        err,
        interactive,
    };
    if interactive {
        writeln!(cli.out, "{BANNER}")?;
    }
    cli.read(input, 0, "standard input")?;
    cli.flush()?;
    if interactive {
        // The input ended after a prompt: the shell's own goes on the next line.
        writeln!(cli.out)?;
    }
    Ok(())
}

struct Cli<'a> {
    session: &'a mut Session,
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
    interactive: bool,
}

/// Whether to go on reading after a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

impl Cli<'_> {
    /// Reads `input` (`source` names it in errors) to its end or a `\q`, running each command
    /// as soon as it is complete; what is left at the end runs too, as a script's last command
    /// may lack its `;`. `depth` counts the `\i` it is in.
    ///
    /// Only typed input, at depth 0 from a terminal, gets prompts, and its pages are written
    /// after each command: what the screen showed as done must survive a killed process. A
    /// script is written once, at its end (`run`), since writing after each of thousands of
    /// commands would cost far more than running them.
    fn read(&mut self, input: &mut dyn BufRead, depth: usize, source: &str) -> io::Result<Flow> {
        let is_typed = self.interactive && depth == 0;
        let mut buffer = CommandBuffer::default();
        loop {
            if is_typed {
                let prompt = if buffer.is_blank() {
                    PROMPT
                } else {
                    CONTINUATION
                };
                write!(self.out, "{prompt}")?;
                self.out.flush()?;
            }
            let mut line = String::new();
            match input.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                // A file that cannot be read stops, not the client; what it ran stays done.
                Err(err) if depth > 0 => {
                    writeln!(self.err, "\\i: {source}: {err}")?;
                    return Ok(Flow::Continue);
                }
                Err(err) => return Err(err),
            }
            let line = line.trim_end_matches(['\n', '\r']);
            // A meta-command may come in the middle of a command, but not inside a string or a
            // comment, where the line is text.
            if line.trim_start().starts_with('\\') && (buffer.is_blank() || !buffer.is_open()) {
                let flow = self.meta_command(line.trim(), depth)?;
                if is_typed {
                    self.flush()?;
                }
                if flow == Flow::Quit {
                    return Ok(Flow::Quit);
                }
                continue;
            }
            buffer.push_line(line);
            if buffer.is_complete() {
                self.execute(&buffer.take())?;
                if is_typed {
                    self.flush()?;
                }
            }
        }
        if !buffer.is_blank() {
            self.execute(&buffer.take())?;
        }
        Ok(Flow::Continue)
    }

    /// Writes every dirty page, printing a failure as an error.
    fn flush(&mut self) -> io::Result<()> {
        match self.session.bpm.flush_all() {
            Ok(()) => Ok(()),
            Err(err) => writeln!(
                self.err,
                "{}",
                output::error(&err.to_string(), err.sqlstate())
            ),
        }
    }

    fn meta_command(&mut self, line: &str, depth: usize) -> io::Result<Flow> {
        let (command, argument) = match line.split_once(char::is_whitespace) {
            Some((command, argument)) => (command, argument.trim()),
            None => (line, ""),
        };
        match (command, argument) {
            ("\\q", _) => return Ok(Flow::Quit),
            ("\\?", _) => writeln!(self.out, "{HELP}")?,
            ("\\d", "") => self.list_tables()?,
            ("\\d", table) => self.describe(&table_name(table))?,
            ("\\i", "") => writeln!(self.err, "\\i: missing file name")?,
            ("\\i", path) => return self.include(path, depth),
            _ => writeln!(self.err, "invalid command {command}. Try \\? for help.")?,
        }
        Ok(Flow::Continue)
    }

    fn list_tables(&mut self) -> io::Result<()> {
        let mut tables: Vec<(String, usize)> = self
            .session
            .catalog
            .tables()
            .iter()
            .map(|table| (table.name.clone(), table.schema.columns().len()))
            .collect();
        tables.sort();
        let mut rows = Vec::with_capacity(tables.len());
        for (name, columns) in tables {
            let indexes = match self.session.catalog.table_indexes(&name) {
                Ok(indexes) => indexes.len(),
                Err(err) => return self.catalog_error(&err),
            };
            rows.push(vec![Value::Text(name), count(columns), count(indexes)]);
        }
        let columns = ["table", "columns", "indexes"].map(str::to_string);
        writeln!(self.out, "List of tables")?;
        writeln!(self.out, "{}\n", output::table(&columns, &rows))
    }

    fn describe(&mut self, name: &str) -> io::Result<()> {
        let catalog = &self.session.catalog;
        let (table, indexes) = match catalog
            .table(name)
            .and_then(|table| catalog.table_indexes(name).map(|indexes| (table, indexes)))
        {
            Ok(found) => found,
            Err(err) => return self.catalog_error(&err),
        };
        let schema_columns = table.schema.columns();
        let rows: Vec<Vec<Value>> = schema_columns
            .iter()
            .map(|column| {
                let nullable = if column.nullable { "" } else { "not null" };
                vec![
                    Value::Text(column.name.clone()),
                    Value::Text(column.data_type.to_string()),
                    Value::Text(nullable.to_string()),
                ]
            })
            .collect();
        let mut text = format!(
            "Table \"{}\"\n{}",
            table.name,
            output::table(&["column", "type", "nullable"].map(str::to_string), &rows)
        );
        if !indexes.is_empty() {
            text.push_str("\nIndexes:");
            for index in indexes {
                let column = schema_columns
                    .get(usize::from(index.column))
                    .map_or("?", |column| column.name.as_str());
                text.push_str(&format!("\n    \"{}\" UNIQUE ({column})", index.name));
            }
        }
        writeln!(self.out, "{text}\n")
    }

    /// Runs the commands of the file at `path` without prompts; a `\q` in it quits.
    fn include(&mut self, path: &str, depth: usize) -> io::Result<Flow> {
        if depth >= MAX_INCLUDE_DEPTH {
            writeln!(
                self.err,
                "\\i: nested too deeply ({MAX_INCLUDE_DEPTH} files)"
            )?;
            return Ok(Flow::Continue);
        }
        match File::open(path) {
            Ok(file) => self.read(&mut BufReader::new(file), depth + 1, path),
            Err(err) => {
                writeln!(self.err, "\\i: {path}: {err}")?;
                Ok(Flow::Continue)
            }
        }
    }

    fn catalog_error(&mut self, err: &CatalogError) -> io::Result<()> {
        writeln!(
            self.err,
            "{}",
            output::error(&err.to_string(), err.sqlstate())
        )
    }

    /// Runs the statements of `text` in order, printing each outcome as it comes; the first
    /// error stops the rest, as in `run_sql`.
    fn execute(&mut self, text: &str) -> io::Result<()> {
        match parse(text) {
            Err(err) => self.sql_error(&PlannerError::Sql(err))?,
            Ok(statements) => {
                for statement in statements {
                    let outcome = run_statement(
                        statement,
                        &mut ExecutionContext {
                            catalog: &mut self.session.catalog,
                            bpm: &mut self.session.bpm,
                        },
                    );
                    match outcome {
                        Ok(outcome) => self.print(&outcome)?,
                        Err(err) => {
                            self.sql_error(&err)?;
                            break;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn print(&mut self, outcome: &StatementOutcome) -> io::Result<()> {
        writeln!(self.out, "{}", output::outcome(outcome))?;
        // A result of several lines ends with a blank one, to stand apart from the next.
        if let StatementOutcome::Rows { .. } | StatementOutcome::Explain(_) = outcome {
            writeln!(self.out)?;
        }
        Ok(())
    }

    fn sql_error(&mut self, err: &PlannerError) -> io::Result<()> {
        writeln!(
            self.err,
            "{}",
            output::error(&err.to_string(), err.sqlstate())
        )
    }
}

/// A database that could not be opened.
#[derive(Debug)]
pub enum CliError {
    BufferPool(BufferPoolError),
    Catalog(CatalogError),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::BufferPool(err) => write!(f, "{err}"),
            CliError::Catalog(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CliError::BufferPool(err) => Some(err),
            CliError::Catalog(err) => Some(err),
        }
    }
}

impl From<BufferPoolError> for CliError {
    fn from(err: BufferPoolError) -> Self {
        CliError::BufferPool(err)
    }
}

impl From<CatalogError> for CliError {
    fn from(err: CatalogError) -> Self {
        CliError::Catalog(err)
    }
}

/// The table a `\d` argument names: folded to lowercase as SQL folds a name, unless it is
/// quoted.
fn table_name(argument: &str) -> String {
    match argument
        .strip_prefix('"')
        .and_then(|name| name.strip_suffix('"'))
    {
        Some(quoted) => quoted.to_string(),
        None => argument.to_ascii_lowercase(),
    }
}

fn count(number: usize) -> Value {
    Value::BigInt(i64::try_from(number).unwrap_or(i64::MAX))
}

/// The lines of a command until it is complete.
#[derive(Debug, Default)]
struct CommandBuffer {
    text: String,
    is_complete: bool,
    /// The last lexing ended inside a string, a quoted name or a comment.
    was_open: bool,
    /// How many times the text was lexed, to check that a long command is lexed once.
    #[cfg(test)]
    lexes: usize,
}

impl CommandBuffer {
    /// Adds a line; blank lines before a command are dropped, so that an error's position
    /// counts from the command's first line.
    ///
    /// The text is lexed again only when the line could end the command: when it has a `;`, or
    /// when the text was left inside a string, a quoted name or a comment. Without either, the
    /// last token cannot have become a `;` (and a string the line opens hides no `;` before it),
    /// so a command of many lines is lexed once, not once per line.
    fn push_line(&mut self, line: &str) {
        if self.text.is_empty() && line.trim().is_empty() {
            return;
        }
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(line);
        if line.contains(';') || self.was_open {
            self.lex();
        }
    }

    /// Text the lexer cannot read for another reason than a string, a quoted name or a comment
    /// left open counts as complete, for the parser to report.
    fn lex(&mut self) {
        #[cfg(test)]
        {
            self.lexes += 1;
        }
        (self.is_complete, self.was_open) = match tokenize(&self.text) {
            Ok(tokens) => (
                tokens
                    .last()
                    .is_some_and(|token| token.kind == TokenKind::Semicolon),
                false,
            ),
            Err(err) if is_left_open(&err) => (false, true),
            Err(_) => (true, false),
        };
    }

    fn is_complete(&self) -> bool {
        self.is_complete
    }

    /// Whether the text ends inside a string, a quoted name or a comment, where a line that
    /// starts with `\` is text and not a meta-command. It lexes the text, so it is asked only
    /// for such lines.
    fn is_open(&self) -> bool {
        tokenize(&self.text).is_err_and(|err| is_left_open(&err))
    }

    fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    fn take(&mut self) -> String {
        self.is_complete = false;
        self.was_open = false;
        std::mem::take(&mut self.text)
    }
}

fn is_left_open(err: &SqlError) -> bool {
    matches!(
        err,
        SqlError::UnterminatedString { .. }
            | SqlError::UnterminatedQuotedIdentifier { .. }
            | SqlError::UnterminatedComment { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{Cursor, Read};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Feeds `lines` one by one and returns each command taken when it became complete, and
    /// what is left.
    fn commands(lines: &[&str]) -> (Vec<String>, String) {
        let mut buffer = CommandBuffer::default();
        let mut commands = Vec::new();
        for line in lines {
            buffer.push_line(line);
            if buffer.is_complete() {
                commands.push(buffer.take());
            }
        }
        (commands, buffer.take())
    }

    #[test]
    fn a_semicolon_at_the_end_should_complete_a_command() {
        let one_line = commands(&["SELECT 1;"]);
        let several = commands(&["SELECT a", "FROM t", "WHERE a > 1;", "SELECT"]);
        let two_in_a_line = commands(&["SELECT 1; SELECT 2;"]);
        let trailing = commands(&["SELECT 1; SELECT", "2;"]);

        assert_eq!(one_line, (vec!["SELECT 1;".to_string()], String::new()));
        assert_eq!(
            several,
            (
                vec!["SELECT a\nFROM t\nWHERE a > 1;".to_string()],
                "SELECT".to_string()
            )
        );
        assert_eq!(two_in_a_line.0, ["SELECT 1; SELECT 2;"]);
        assert_eq!(trailing.0, ["SELECT 1; SELECT\n2;"]);
    }

    #[test]
    fn a_semicolon_inside_a_string_a_name_or_a_comment_should_not() {
        let cases: [&[&str]; 5] = [
            &["SELECT ';'"],
            &["SELECT 'a;", "b'"],
            &["SELECT \"a;b\""],
            &["SELECT 1 -- ;"],
            &["SELECT 1 /* ;", "; */"],
        ];

        for lines in cases {
            let (taken, left) = commands(lines);

            assert!(taken.is_empty(), "{lines:?}: {taken:?}");
            assert_eq!(left, lines.join("\n"));
        }
        assert_eq!(commands(&["SELECT 'a;", "b';"]).0, ["SELECT 'a;\nb';"]);
        assert_eq!(
            commands(&["SELECT 1 /* ;", "*/;"]).0,
            ["SELECT 1 /* ;\n*/;"]
        );
    }

    #[test]
    fn text_the_lexer_cannot_read_should_go_to_the_parser_when_its_line_ends_it() {
        let (taken, left) = commands(&["SELECT 12abc", "FROM t;"]);
        let (none_yet, waiting) = commands(&["SELECT 12abc"]);

        assert_eq!(taken, ["SELECT 12abc\nFROM t;"]);
        assert_eq!(left, "");
        assert!(none_yet.is_empty());
        assert_eq!(waiting, "SELECT 12abc");
    }

    #[test]
    fn a_long_command_should_be_lexed_only_when_a_line_can_end_it() {
        let mut buffer = CommandBuffer::default();
        buffer.push_line("INSERT INTO t VALUES");

        for id in 0..5_000 {
            buffer.push_line(&format!("  ({id}, 'x'),"));
        }
        let lexes_before_the_end = buffer.lexes;
        buffer.push_line("  (5000, 'last');");

        assert_eq!(lexes_before_the_end, 0);
        assert_eq!(buffer.lexes, 1);
        assert!(buffer.is_complete());
    }

    #[test]
    fn a_string_left_open_should_be_lexed_until_it_closes() {
        let mut buffer = CommandBuffer::default();

        buffer.push_line("SELECT 'a;");
        buffer.push_line("no semicolon here");
        buffer.push_line("b';");

        assert_eq!(buffer.lexes, 3);
        assert!(buffer.is_complete());
    }

    #[test]
    fn blank_lines_before_a_command_should_not_enter_the_buffer() {
        let mut buffer = CommandBuffer::default();

        buffer.push_line("");
        buffer.push_line("   ");
        buffer.push_line("SELECT 1;");

        assert_eq!(buffer.take(), "SELECT 1;");
    }

    #[test]
    fn a_blank_buffer_should_be_neither_complete_nor_a_command() {
        let mut buffer = CommandBuffer::default();

        buffer.push_line("   ");

        assert!(buffer.is_blank());
        assert!(!buffer.is_complete());
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[(self.next() % items.len() as u64) as usize]
        }
    }

    #[test]
    fn random_scripts_should_split_into_the_same_commands() {
        let mut rng = Rng(0x5DEE_CE66_D1CE_4E5B);
        let pieces = [
            "SELECT",
            "1",
            "x",
            "'a;b'",
            "'x\ny;'",
            "\"c;d\"",
            "-- c;\n",
            "/* ;\n; */",
            "''';'",
            "t.a",
        ];
        let separators = [" ", "\n", "\n\n", "  \n ", " \n"];

        for _ in 0..500 {
            // Each command ends with its `;`; `ends` holds where each one ends in the script.
            let mut script = String::new();
            let mut ends = Vec::new();
            for _ in 0..1 + rng.next() % 4 {
                let words: Vec<&str> = (0..1 + rng.next() % 5).map(|_| rng.pick(&pieces)).collect();
                script += &words.join(" ");
                script += ";";
                ends.push(script.len());
                script += rng.pick(&separators);
            }

            let lines: Vec<&str> = script.split('\n').collect();
            let (taken, left) = commands(&lines);

            // Every command is taken whole: each piece taken ends where a command ends, with
            // nothing but blanks and comments after it (blank lines before a command are
            // dropped), and the pieces hold all the words of the script.
            let mut offset = 0;
            for piece in &taken {
                let start = offset + script[offset..].find(piece.as_str()).unwrap();
                let end = start + piece.len();
                assert!(
                    ends.iter().any(|&command_end| command_end > start
                        && command_end <= end
                        && tokenize(&script[command_end..end])
                            .is_ok_and(|tokens| tokens.is_empty())),
                    "{script:?}: {piece:?}"
                );
                offset = end;
            }
            assert!(left.trim().is_empty(), "{script:?}: left {left:?}");
            let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
            assert_eq!(words(&taken.join("\n")), words(&script), "{script:?}");
        }
    }

    struct TempFile(PathBuf);

    impl TempFile {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!("ferro-cli-{}-{id}.db", std::process::id())))
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// What `input` prints, on the output and on the errors, in a session on a new database.
    fn transcript(input: &str, interactive: bool) -> (String, String) {
        let file = TempFile::new();
        let mut session = Session::open(&file.0).unwrap();
        transcript_in(&mut session, input, interactive)
    }

    fn transcript_in(session: &mut Session, input: &str, interactive: bool) -> (String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        run(
            session,
            &mut Cursor::new(input.as_bytes()),
            &mut out,
            &mut err,
            interactive,
        )
        .unwrap();
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn a_script_should_print_each_outcome_in_turn() {
        let script = "CREATE TABLE t (id INTEGER NOT NULL, name VARCHAR(10));\n\
                      INSERT INTO t VALUES (1, 'ana'),\n  (2, NULL);\n\
                      SELECT * FROM t ORDER BY id;\n\
                      UPDATE t SET name = 'rui' WHERE id = 2; DELETE FROM t WHERE id = 1;\n\
                      EXPLAIN SELECT name FROM t;\n";

        let (out, err) = transcript(script, false);

        assert_eq!(
            out,
            "CREATE TABLE\n\
             INSERT 0 2\n\
             \x20id | name\n\
             ----+------\n\
             \x20 1 | ana\n\
             \x20 2 |\n\
             (2 rows)\n\
             \n\
             UPDATE 1\n\
             DELETE 1\n\
             Projection: t.name AS name\n\
             \x20 SeqScan: t\n\
             \n"
        );
        assert_eq!(err, "");
    }

    #[test]
    fn an_error_should_stop_its_line_but_not_the_reading() {
        let script = "CREATE TABLE t (id INTEGER);\n\
                      INSERT INTO t VALUES (1); SELECT zz FROM t; INSERT INTO t VALUES (2);\n\
                      SELECT FROM;\n\
                      SELECT count(*) FROM t;\n";

        let (out, err) = transcript(script, false);

        assert_eq!(
            out,
            "CREATE TABLE\nINSERT 0 1\n count\n-------\n     1\n(1 row)\n\n"
        );
        assert_eq!(
            err,
            "ERROR: column \"zz\" does not exist (SQLSTATE 42703)\n\
             ERROR: syntax error at character 8: expected an expression, found \"FROM\" \
             (SQLSTATE 42601)\n"
        );
    }

    #[test]
    fn a_terminal_should_get_the_banner_and_the_prompts() {
        let (out, _) = transcript("SELECT\n1;\n", true);

        assert_eq!(
            out,
            format!(
                "{BANNER}\n{PROMPT}{CONTINUATION} ?column?\n----------\n        1\n(1 row)\n\n{PROMPT}\n"
            )
        );
    }

    #[test]
    fn the_end_of_the_input_should_run_what_is_left_and_skip_blank_lines() {
        let (out, err) = transcript("\n  \n\nSELECT 1 AS one\n", false);

        assert_eq!(out, " one\n-----\n   1\n(1 row)\n\n");
        assert_eq!(err, "");
    }

    const TWO_TABLES: &str = "CREATE TABLE users (id INTEGER NOT NULL, name VARCHAR(20));\n\
                              CREATE UNIQUE INDEX users_id ON users (id);\n\
                              CREATE TABLE accounts (n BIGINT);\n";

    #[test]
    fn describe_should_list_the_tables_or_show_one() {
        let script = format!("{TWO_TABLES}\\d\n\\d USERS\n");

        let (out, err) = transcript(&script, false);

        assert_eq!(
            out,
            "CREATE TABLE\nCREATE INDEX\nCREATE TABLE\n\
             List of tables\n\
             \x20 table   | columns | indexes\n\
             ----------+---------+---------\n\
             \x20accounts |       1 |       0\n\
             \x20users    |       2 |       1\n\
             (2 rows)\n\
             \n\
             Table \"users\"\n\
             \x20column |    type     | nullable\n\
             --------+-------------+----------\n\
             \x20id     | INTEGER     | not null\n\
             \x20name   | VARCHAR(20) |\n\
             (2 rows)\n\
             Indexes:\n\
             \x20   \"users_id\" UNIQUE (id)\n\
             \n"
        );
        assert_eq!(err, "");
    }

    #[test]
    fn describe_should_fail_for_a_table_that_does_not_exist() {
        let (out, err) = transcript(&format!("{TWO_TABLES}\\d \"Users\"\n\\d nowhere\n"), false);

        assert!(!out.contains("Table"), "{out}");
        assert_eq!(
            err,
            "ERROR: table \"Users\" does not exist (SQLSTATE 42P01)\n\
             ERROR: table \"nowhere\" does not exist (SQLSTATE 42P01)\n"
        );
    }

    #[test]
    fn quit_should_stop_the_reading() {
        let (out, _) = transcript("SELECT 1 AS one;\n  \\q\nSELECT 2 AS two;\n", false);

        assert!(out.contains("one"), "{out}");
        assert!(!out.contains("two"), "{out}");
    }

    #[test]
    fn help_and_unknown_commands_should_answer_without_sql() {
        let (out, err) = transcript("\\?\n\\x\n\\i\n", false);

        assert_eq!(out, format!("{HELP}\n"));
        assert_eq!(
            err,
            "invalid command \\x. Try \\? for help.\n\\i: missing file name\n"
        );
    }

    #[test]
    fn include_should_run_a_file_as_if_it_were_typed() {
        let script = TempFile::new();
        std::fs::write(
            &script.0,
            "CREATE TABLE t (a INTEGER);\nSELECT count(*)\n  FROM t;\n",
        )
        .unwrap();
        let missing = TempFile::new();

        let (out, err) = transcript(
            &format!(
                "\\i {}\n\\i {}\nSELECT 1 AS after;\n",
                script.0.display(),
                missing.0.display()
            ),
            false,
        );

        assert_eq!(
            out,
            "CREATE TABLE\n count\n-------\n     0\n(1 row)\n\n after\n-------\n     1\n(1 row)\n\n"
        );
        assert!(
            err.starts_with(&format!("\\i: {}: ", missing.0.display())),
            "{err}"
        );
    }

    #[test]
    fn include_should_stop_a_file_that_includes_itself() {
        let script = TempFile::new();
        std::fs::write(
            &script.0,
            format!("SELECT 1 AS one;\n\\i {}\n", script.0.display()),
        )
        .unwrap();

        let (out, err) = transcript(&format!("\\i {}\n", script.0.display()), false);

        assert_eq!(out.matches("(1 row)").count(), MAX_INCLUDE_DEPTH);
        assert_eq!(
            err,
            format!("\\i: nested too deeply ({MAX_INCLUDE_DEPTH} files)\n")
        );
    }

    #[test]
    fn quit_in_an_included_file_should_stop_everything() {
        let script = TempFile::new();
        std::fs::write(&script.0, "SELECT 1 AS one;\n\\q\nSELECT 2 AS two;\n").unwrap();

        let (out, _) = transcript(
            &format!("\\i {}\nSELECT 3 AS three;\n", script.0.display()),
            true,
        );

        assert!(out.contains("one"), "{out}");
        assert!(!out.contains("two") && !out.contains("three"), "{out}");
        // A file is read without prompts, even from a terminal.
        assert_eq!(out.matches(PROMPT).count(), 1, "{out}");
    }

    #[test]
    fn opening_a_file_that_is_not_a_database_should_fail() {
        let file = TempFile::new();
        std::fs::write(&file.0, vec![0xAB; PAGE_SIZE]).unwrap();

        let err = Session::open(&file.0).err().unwrap();

        assert!(!err.to_string().is_empty());
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn arguments_should_name_the_file_or_ask_for_help() {
        let parse = |arguments: &[&str]| parse_arguments(arguments.iter().map(OsString::from));

        assert_eq!(
            parse(&[]),
            Ok(Invocation::Open(PathBuf::from(DEFAULT_DATABASE)))
        );
        assert_eq!(
            parse(&["shop.db"]),
            Ok(Invocation::Open(PathBuf::from("shop.db")))
        );
        assert_eq!(parse(&["-"]), Ok(Invocation::Open(PathBuf::from("-"))));
        assert_eq!(parse(&["--help"]), Ok(Invocation::Help));
        assert_eq!(parse(&["shop.db", "-h"]), Ok(Invocation::Help));
        assert_eq!(parse(&["-x"]), Err("unknown option -x".to_string()));
        assert_eq!(
            parse(&["a.db", "b.db"]),
            Err("too many arguments".to_string())
        );
    }

    #[test]
    fn an_include_that_is_not_utf8_should_fail_and_go_on() {
        let script = TempFile::new();
        std::fs::write(&script.0, b"SELECT 'caf\xe9';\n").unwrap();

        let (out, err) = transcript(
            &format!("\\i {}\nSELECT 1 AS after;\n", script.0.display()),
            false,
        );

        assert!(out.contains("after"), "{out}");
        assert!(
            err.starts_with(&format!("\\i: {}: ", script.0.display())),
            "{err}"
        );
    }

    #[test]
    fn describe_should_list_no_tables_in_an_empty_database() {
        let (out, err) = transcript("\\d\n", false);

        assert_eq!(
            out,
            "List of tables\n table | columns | indexes\n-------+---------+---------\n(0 rows)\n\n"
        );
        assert_eq!(err, "");
    }

    #[test]
    fn an_empty_include_should_print_nothing() {
        let script = TempFile::new();
        std::fs::write(&script.0, "").unwrap();

        let (out, err) = transcript(&format!("\\i {}\n", script.0.display()), false);

        assert_eq!((out.as_str(), err.as_str()), ("", ""));
    }

    #[test]
    fn a_meta_command_should_work_in_the_middle_of_a_command_but_not_inside_a_string() {
        let (between, _) = transcript("SELECT 1 AS one\n\\d\n;\n", false);
        let (quit, _) = transcript("SELECT 1 AS one\n\\q\n;\n", false);
        let (in_string, _) = transcript("SELECT 'a\n\\q\nb' AS text;\n", false);

        assert!(between.starts_with("List of tables\n"), "{between}");
        assert!(
            between.ends_with(" one\n-----\n   1\n(1 row)\n\n"),
            "{between}"
        );
        assert_eq!(quit, "");
        assert!(in_string.contains("(1 row)"), "{in_string}");
    }

    #[test]
    fn an_error_position_should_count_from_the_first_line_of_its_command() {
        let (_, err) = transcript("\n   \nSELECT FROM;\n", false);

        assert_eq!(
            err,
            "ERROR: syntax error at character 8: expected an expression, found \"FROM\" \
             (SQLSTATE 42601)\n"
        );
    }

    /// Input that, when the client asks for its byte at `check_at`, opens a second session on
    /// the same file and records what `\d` shows there: what the first session wrote so far.
    struct Watching {
        bytes: Vec<u8>,
        position: usize,
        check_at: usize,
        path: PathBuf,
        seen: Option<String>,
    }

    impl Read for Watching {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.position == self.check_at && self.seen.is_none() {
                let mut second = Session::open(&self.path).unwrap();
                self.seen = Some(transcript_in(&mut second, "\\d\n", false).0);
            }
            let Some(byte) = self.bytes.get(self.position) else {
                return Ok(0);
            };
            match buf.first_mut() {
                Some(slot) => *slot = *byte,
                None => return Ok(0),
            }
            self.position += 1;
            Ok(1)
        }
    }

    #[test]
    fn a_typed_command_should_be_in_the_file_before_the_next_prompt() {
        let file = TempFile::new();
        let mut session = Session::open(&file.0).unwrap();
        let first_line = "CREATE TABLE t (a INTEGER);\n";
        let input = Watching {
            bytes: format!("{first_line}SELECT 1;\n").into_bytes(),
            position: 0,
            check_at: first_line.len(),
            path: file.0.clone(),
            seen: None,
        };
        // One byte at a time, so the check runs right after the first command.
        let mut reader = BufReader::with_capacity(1, input);
        let (mut out, mut err) = (Vec::new(), Vec::new());

        run(&mut session, &mut reader, &mut out, &mut err, true).unwrap();

        let seen = reader.into_inner().seen.unwrap();
        assert!(seen.contains(" t     |       1 |       0"), "{seen}");
    }
}
