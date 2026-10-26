//! `ferro-db [FILE]`: opens the database in `FILE` (`data.db` by default), creating it when it
//! does not exist, and reads SQL from standard input: with prompts from a terminal, as a script
//! otherwise (`ferro-db shop.db < setup.sql`).

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use ferro_db::connectivity::cli::{Invocation, Session, USAGE, parse_arguments, run};

/// The exit code of a command line that cannot be understood, as shells use it.
const USAGE_ERROR: u8 = 2;

fn main() -> ExitCode {
    let path = match parse_arguments(std::env::args_os().skip(1)) {
        Ok(Invocation::Open(path)) => path,
        Ok(Invocation::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("ferro-db: {message}\n{USAGE}");
            return ExitCode::from(USAGE_ERROR);
        }
    };
    let mut session = match Session::open(&path) {
        Ok(session) => session,
        Err(err) => {
            eprintln!("ferro-db: {}: {err}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    let result = run(
        &mut session,
        &mut stdin.lock(),
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
        interactive,
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ferro-db: {err}");
            ExitCode::FAILURE
        }
    }
}
