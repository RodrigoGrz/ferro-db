# Connectivity

This document describes how a person talks to ferroDB: the command-line client (`src/connectivity/`, the `ferro-db` binary). The database is embedded in the client's process, as in `sqlite3`: there is no server or network protocol yet. Each command goes to the planner ([planner.md](planner.md)) through the same `run_statement` that `run_sql` uses.

Dependencies point downward: `connectivity` → `planner`, `sql` (the lexer and the parser), `catalog`, `execution`, `storage`, `types`.

| File | Responsibility |
|---|---|
| `cli.rs` | `Session` (the open database), the read → run → print loop, the command buffer, the meta-commands |
| `output.rs` | the text of results: aligned tables, command tags, errors |
| `src/main.rs` | the binary: the arguments, whether the input is a terminal, the exit code |

- [Usage](#usage)
- [The loop](#the-loop)
- [When a command is complete](#when-a-command-is-complete)
- [Meta-commands](#meta-commands)
- [Output](#output)
- [Design decisions](#design-decisions)
- [Tests](#tests)
- [Limitations](#limitations)

---

## Usage

```text
$ cargo run -- shop.db
ferro-db: type \? for help, \q to quit
ferro=> CREATE TABLE customers (id INTEGER NOT NULL, name VARCHAR(20), city VARCHAR(20));
CREATE TABLE
ferro=> INSERT INTO customers VALUES (1, 'Ana', 'Lisboa'),
ferro->   (2, 'Rui', 'Porto'), (3, 'Inês', NULL);
INSERT 0 3
ferro=> SELECT name, city FROM customers WHERE id > 1;
 name | city
------+-------
 Rui  | Porto
 Inês |
(2 rows)

ferro=> SELECT nome FROM customers;
ERROR: column "nome" does not exist (SQLSTATE 42703)
ferro=> \q
```

- `ferro-db [FILE]` opens the database in `FILE`, `data.db` in the current directory by default, and creates it when the file does not exist. A file that is not a ferroDB database is refused with the catalog's or the buffer pool's error, and the exit code is 1.
- `ferro-db -h` or `--help` prints the usage. An unknown option or a second file prints the usage with the problem and exits with 2: an argument that looks like an option is never taken for a file, so a mistyped option does not create a database named after it (`./-x` names such a file).
- **From a terminal** the client prints a banner and the prompts (`ferro=> `, `ferro-> ` inside a command). **Otherwise** it reads a script, without them: `ferro-db shop.db < setup.sql`. The exit code is 0 even when commands fail, as their errors are printed and reading goes on.

## The loop

`run(session, input, out, err, interactive)` reads lines from `input` until it ends or `\q`:

1. A line that starts with `\` is a **meta-command**, also in the middle of a command (the command stays gathered, or is dropped by `\q`), but not inside a string or a comment left open, where it is text.
2. Any other line joins the **command buffer**; blank lines before a command are dropped, so an error's position counts from the command's first line. When the buffer is **complete**, its text is parsed, and its statements run one by one through `run_statement`, each outcome printed as soon as it comes. The first error is printed (to `err`) and stops the rest of that text, as in `run_sql`; a syntax error runs none of it.
3. **`flush_all`** writes every dirty page of the buffer pool: after each command typed at the prompt (and each meta-command there, which may run a whole `\i`), so that what the screen showed as done is in the file even if the process is then killed (Ctrl-C does not run destructors); and once at the end of the input. A script is written only at its end: the disk manager syncs every page it writes, and writing after each of 5,000 commands took 25 s where running them took 0.6 s. It is not atomicity: without a WAL, a crash in the middle of the writes is still the documented limit.
4. When the input ends, a buffer that is not blank runs as it is, so a script's last command may lack its `;`. Blank lines before a command are skipped.

An error never stops the reading, from a terminal or a script.

## When a command is complete

The buffer asks the **lexer** of the SQL layer, the one the parser uses, instead of counting quotes itself:

- the text lexes and its last token is `;` → complete;
- the lexer reports a string, a quoted name or a comment left open → not complete, keep reading (a string may span lines);
- any other lexer error → complete, so that the parser reports it with its position (`42601`).

The text is lexed again only when a new line could end the command: when the line has a `;`, or when the text was left inside a string, a quoted name or a comment. Without either, the last token cannot have become a `;` (and a string the line opens hides no `;` before it), so a command of many lines is lexed once: lexing it after every line made a 5,000-line `INSERT` take 37 s instead of 0.5 s. Text the lexer cannot read therefore goes to the parser when the line with its `;` comes, or at the end of the input.

So a `;` inside `'a;b'`, `"c;d"`, `-- ;` or `/* ; */` never ends a command, and a comment after the `;` on the same line goes with it. A line may hold several commands (`SELECT 1; SELECT 2;`): they run in order.

## Meta-commands

| Command | Effect |
|---|---|
| `\d` | `List of tables`: each table (by name) with how many columns and indexes it has |
| `\d TABLE` | `Table "t"`: each column with its type and `not null`, then `Indexes:` with `"name" UNIQUE (column)`; the name is folded to lowercase unless quoted (`\d "Users"`); an unknown table is the catalog's `42P01` |
| `\i FILE` | runs the commands of the file as if they were typed, with a buffer of its own and no prompts; a `\q` in it quits the client; a file that does not open is an error, and so is a 17th `\i` inside `\i`, so that a file that includes itself stops instead of exhausting the stack; a file that cannot be read midway (not UTF-8) stops with an error, the commands before it done, and the client goes on |
| `\?` | the list of meta-commands |
| `\q` | quits |
| anything else | `invalid command \x. Try \? for help.` |

## Output

**Rows of a query**, aligned as a table:

```text
 name | orders | spent
------+--------+-------
 Ana  |      2 |   120
 Rui  |      1 |    20
(2 rows)
```

- each cell has a space on either side and the columns are separated by `|`; the header is centered (the odd space on the right) and the rule under it is made of `-` and `+`;
- numbers (`SMALLINT` to `DOUBLE PRECISION`) line up to the right, everything else to the left; `NULL` is empty; each value is written by `Value`'s `Display`;
- widths count characters, not bytes, so accented text lines up;
- the footer is `(1 row)` or `(n rows)`, also for none, and a blank line follows.

**Other outcomes** are command tags: `CREATE TABLE`, `CREATE INDEX`, `INSERT 0 n` (the tag's legacy OID field, always 0, which clients expect), `UPDATE n`, `DELETE n`; `EXPLAIN` prints its plan, followed by a blank line.

**Errors** go to `err`: `ERROR: <message> (SQLSTATE xxxxx)`. Errors of the client itself (`\i` of a missing file, an unknown meta-command) have no SQLSTATE.

## Design decisions

- **An embedded database**, not a client of a server: the client opens the file and calls the planner; a protocol and a server are the next Connectivity items.
- **Standard input only** (decided with the author), without a line-editing crate: no new dependency, at the cost of history and arrow keys. Scripts come for free by redirecting the input, and `\i` runs a file from the prompt.
- **The lexer decides when a command ends**, one rule shared with the parser: a `;` in a string or a comment can never cut a command.
- **Each statement printed as it runs**, through `run_statement`, instead of `run_sql`, which returns the outcomes only at the end and drops those before an error: `INSERT …; SELECT zz …;` shows the `INSERT 0 1` before the error, so what was done is never hidden.
- **`flush_all` after each typed command, and once at the end of a script**, instead of trusting the buffer pool's `Drop`: a Ctrl-C kills the process without it. After every command of a script it made scripts 40 times slower, so a script killed midway may lose what it had not yet written, and can be run again.
- **Lexing only when a line can end the command**, instead of after every line: the same answer, and a long command is gathered in linear time.
- **Meta-commands also in the middle of a command**, as a client is expected to accept `\q` anywhere; not inside an open string or comment, where the line is text.
- **Arguments parsed by a function** (`parse_arguments`), tested apart from the binary.
- **Errors do not stop the reading**: a script shows all its errors at once.
- **`run` takes `BufRead` and `Write`**: the whole client is tested with in-memory input and output; only `main.rs` looks at the terminal (`std::io::IsTerminal`).
- **A pool of 64 frames** (512 KB), a constant for now.

## Tests

- `output.rs`: numbers to the right and text to the left, `NULL` empty, a header centered over wider values, accented text, a table without rows, `(1 row)`, every command tag and `EXPLAIN`, an error with its SQLSTATE.
- `cli.rs`, the buffer: a command on one line and on several, two in a line, text after the last `;`; a `;` inside a string (also across lines), a quoted name, a line comment and a block comment; text the lexer cannot read going to the parser when its `;` comes; a blank buffer; blank lines before a command dropped; a 5,000-line command lexed once, when its last line comes, and a string left open lexed on every line until it closes.
- `random_scripts_should_split_into_the_same_commands`: 500 scripts of random commands made of words, strings and comments holding `;` and line breaks, split into lines; every command must be taken whole, ending where a command ends with only blanks and comments after it, and nothing may be lost.
- `cli.rs`, the loop: a script printing each outcome in turn (`CREATE`, a multi-line `INSERT`, `SELECT`, two commands on a line, `EXPLAIN`); an error that stops its line but not the reading, and a syntax error; the banner and the prompts from a terminal; the end of the input running a command without `;` and skipping blank lines.
- `cli.rs`, meta-commands: `\d` and `\d TABLE` with an index and a folded name, an unknown or quoted name (`42P01`); `\q` in the middle; `\?`, an unknown command, `\i` without a file; `\i` of a file with a multi-line command, of a missing file, of a file that includes itself (the limit), and a `\q` inside a file, read without prompts; `\i` of an empty file and of a file that is not UTF-8 (an error, then the next command runs); `\d` of an empty database; a meta-command in the middle of a command and a line starting with `\` inside a string; an error's position counting from its command's first line, after blank lines; the arguments (a file, the default, `-`, `--help` and `-h`, an unknown option, two files); a typed command's pages on disk before the next prompt, checked by a second session opened from inside the input while the first one runs; a file that is not a database refused by `Session::open`.
- `buffer_pool.rs`: `flush_all` writing every dirty page while the pool keeps them and their pins, and leaving a clean page alone.
- Mutations checked by hand, each caught: an unterminated string ending a command, a `flush_all` that writes nothing (by the integration test), `(1 rows)`; and each fix of the review undone: lexing after every line, a read error of an `\i` ending the session, no flush after a typed command, meta-commands only with an empty buffer, blank lines kept, options taken as files.
- Integration in [`tests/connectivity_cli.rs`](../tests/connectivity_cli.rs): a second `Session` opened on the file **while the first is still open** sees its tables, rows and updates (so `flush_all` wrote them, not a destructor); a script of 2,000 rows of 250 characters run by `\i`, more pages than the 64-frame pool, survives reopening.

## Limitations

- **No line editing or history**, no tab completion.
- **No transactions:** each statement is applied as it runs; a script that fails midway keeps the statements before the failure.
- **Widths count characters**, so combining marks and wide characters (CJK, emoji) do not line up in a terminal.
- **Results are collected before printing**: a large `SELECT` is held in memory whole.
- **Ctrl-C kills the process** without a handler; `flush_all` after each typed command keeps what was printed as done, but a command interrupted midway may leave its pages half written, as any crash without a WAL, and a script killed midway may lose the commands it ran, written only at its end.
- **One client per file:** nothing stops two processes from opening the same database, and their buffer pools would overwrite each other's pages.
