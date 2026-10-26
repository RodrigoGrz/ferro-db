use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ferro_db::connectivity::cli::{Session, run};

struct TempFile(PathBuf);

impl TempFile {
    fn new(extension: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "ferro-cli-it-{}-{id}.{extension}",
            std::process::id()
        )))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// What `input` prints on the output and on the errors.
fn transcript(session: &mut Session, input: &str) -> (String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    run(
        session,
        &mut Cursor::new(input.as_bytes()),
        &mut out,
        &mut err,
        false,
    )
    .unwrap();
    (
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn what_a_session_printed_as_done_should_be_in_the_file_before_it_closes() {
    let database = TempFile::new("db");
    let mut first = Session::open(&database.0).unwrap();
    transcript(
        &mut first,
        "CREATE TABLE notes (id INTEGER NOT NULL, body VARCHAR(40));\n\
         CREATE UNIQUE INDEX notes_id ON notes (id);\n\
         INSERT INTO notes VALUES (1, 'first'), (2, 'second');\n\
         UPDATE notes SET body = 'changed' WHERE id = 2;\n",
    );

    // The first session is still open: only what it flushed after each command is on disk.
    let mut second = Session::open(&database.0).unwrap();
    let (out, err) = transcript(&mut second, "\\d\nSELECT body FROM notes WHERE id = 2;\n");

    assert_eq!(err, "");
    assert_eq!(
        out,
        "List of tables\n\
         \x20table | columns | indexes\n\
         -------+---------+---------\n\
         \x20notes |       2 |       1\n\
         (1 row)\n\
         \n\
         \x20 body\n\
         ---------\n\
         \x20changed\n\
         (1 row)\n\
         \n"
    );
    drop(first);
}

#[test]
fn a_large_script_should_fill_more_pages_than_the_pool_and_survive_reopening() {
    let database = TempFile::new("db");
    let script = TempFile::new("sql");
    let body = "x".repeat(250);
    let mut sql = String::from("CREATE TABLE rows (id INTEGER NOT NULL, body VARCHAR(250));\n");
    for start in (0..2_000).step_by(100) {
        let values: Vec<String> = (start..start + 100)
            .map(|id| format!("  ({id}, '{body}')"))
            .collect();
        sql += &format!("INSERT INTO rows VALUES\n{};\n", values.join(",\n"));
    }
    fs::write(&script.0, sql).unwrap();
    {
        let mut session = Session::open(&database.0).unwrap();
        let (out, err) = transcript(&mut session, &format!("\\i {}\n", script.0.display()));

        assert_eq!(err, "");
        assert_eq!(out.matches("INSERT 0 100").count(), 20);
    }

    let mut session = Session::open(&database.0).unwrap();
    let (out, _) = transcript(
        &mut session,
        "SELECT count(*), min(id), max(id) FROM rows WHERE body = '' || body;\n",
    );

    assert_eq!(
        out,
        " count | min | max\n-------+-----+------\n  2000 |   0 | 1999\n(1 row)\n\n"
    );
}
