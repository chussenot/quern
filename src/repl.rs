//! bead: quern-repl-skel — line loop and output formatting.
//!
//! # CLI contract
//!
//! ```text
//! quern [--db <dir>] [script.sql]
//! ```
//!
//! * no `script` — interactive: read semicolon-terminated statements from
//!   stdin until EOF (Ctrl-D). A blank line is not an error.
//! * `script` — run every statement in the file, then exit. A trailing
//!   statement with no final `;` still runs.
//! * `--db <dir>` — the database directory (default `quern.db`), so two
//!   processes can be pointed at the same files. Created by the storage
//!   layer on open, not here.
//! * exit 0 for a run that reached EOF, even if statements failed (statement
//!   errors are output, not process failures); 2 for a CLI failure — bad
//!   flag, unreadable script, database that will not open.
//!
//! # Output contract (shared with `tests/logic/*.slt`, docs/quern.md §5)
//!
//! One row per line, cells tab-separated, every cell formatted by
//! `Display for Value` — so `NULL`, `TRUE`/`FALSE`, bare ints, unquoted
//! text, with exactly one definition of the rules. Non-queries print
//! nothing on success. Errors print `Error: <message>` on **stdout**,
//! because that is what the corpus compares, and never stop the loop.
//! The interactive prompt goes to stderr so a piped driver's stdout stays
//! clean (there is no TTY check — `forbid(unsafe_code)` rules out `isatty`).
//!
//! Stdout is flushed after every statement, and the `quern> ` prompt is
//! written and flushed to stderr before every read. That prompt is the
//! synchronisation point a driver needs: seeing it means the previous
//! statement finished. It replaces bd_30-agents-dwm.37's proposed `ok` line,
//! which cannot go on stdout without breaking the `statement ok` blocks the
//! corpus compares byte for byte.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::sql::lexer;
use crate::types::{QuernError, Result, Row, Value};

const USAGE: &str = "usage: quern [--db <dir>] [script.sql]";
const DEFAULT_DB: &str = "quern.db";

/// Process exit code, so `main.rs` stays a one-liner.
pub fn main() -> i32 {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => 0,
        Err(msg) => {
            eprintln!("quern: {msg}");
            2
        }
    }
}

fn run(args: Vec<String>) -> std::result::Result<(), String> {
    let mut db = None;
    let mut script = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => db = Some(PathBuf::from(args.next().ok_or("--db needs a directory")?)),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`\n{USAGE}")),
            path if script.is_none() => script = Some(PathBuf::from(path)),
            extra => return Err(format!("unexpected argument `{extra}`\n{USAGE}")),
        }
    }

    let db = db.unwrap_or_else(|| PathBuf::from(DEFAULT_DB));
    let mut db = Db::open(&db).map_err(|e| e.to_string())?;
    let stdout = io::stdout();
    let mut out = stdout.lock();

    match script {
        Some(path) => {
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            db.run_text(&text, &mut out)
        }
        None => db.interactive(&mut out),
    }
    .map_err(|e| format!("writing output: {e}"))
}

/// One open database plus the transaction state around it — everything a
/// statement needs. Storage and txn move in here as their beads land; the
/// name and both signatures are the ones bd_30-agents-dwm.36 proposes for
/// `plan/mod.rs`, so that bead's owner can move this type there without
/// touching a single call site.
pub struct Db {
    /// The `--db` directory. The storage layer owns the files under it.
    pub path: PathBuf,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        // Storage/WAL open and replay land here (beads .5/.13/.14); until then
        // opening cannot fail, but the signature already allows it to.
        Ok(Db {
            path: path.to_path_buf(),
        })
    }

    /// The whole pipeline — `lex -> parse -> plan -> execute` — in one place,
    /// so the REPL, the script runner and the `.slt` harness all take the same
    /// path and the later beads have exactly one function to hook into.
    ///
    /// `Ok(None)` means "no rows to print": DDL, DML, or input that was blank
    /// or nothing but a comment. `Ok(Some(rows))` is a query result.
    pub fn execute(&mut self, sql: &str) -> Result<Option<Vec<Row>>> {
        let tokens = lexer::tokenize(sql)?;
        if tokens.is_empty() {
            return Ok(None);
        }
        // parse -> plan -> execute slot in here as they land. Until then this
        // is an error value, not a panic: docs/quern.md §1.
        Err(QuernError::Parse(
            "not implemented: parser, planner and executor".into(),
        ))
    }

    /// Run every statement in `text`, including a trailing one with no `;`.
    fn run_text(&mut self, text: &str, out: &mut impl Write) -> io::Result<()> {
        let (stmts, tail) = split_statements(text);
        for stmt in stmts {
            self.run_one(stmt, out)?;
        }
        self.run_one(tail, out)
    }

    fn interactive(&mut self, out: &mut impl Write) -> io::Result<()> {
        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        let mut buf = String::new();
        let mut line = String::new();
        loop {
            eprint!(
                "{}",
                if buf.trim().is_empty() {
                    "quern> "
                } else {
                    "  ...> "
                }
            );
            io::stderr().flush()?;
            line.clear();
            if stdin.read_line(&mut line)? == 0 {
                eprintln!();
                break;
            }
            buf.push_str(&line);

            let (stmts, tail) = split_statements(&buf);
            let unterminated = tail.len();
            for stmt in stmts {
                self.run_one(stmt, out)?;
            }
            let consumed = buf.len() - unterminated;
            buf.drain(..consumed);
        }
        // Whatever was typed after the last `;` still runs at EOF.
        let tail = std::mem::take(&mut buf);
        self.run_one(&tail, out)
    }

    fn run_one(&mut self, sql: &str, out: &mut impl Write) -> io::Result<()> {
        match self.execute(sql) {
            Ok(None) => {}
            Ok(Some(rows)) => {
                for row in &rows {
                    writeln!(out, "{}", format_row(row))?;
                }
            }
            Err(e) => writeln!(out, "Error: {e}")?,
        }
        // Flushed after every statement, output or not, so a driver reading our
        // stdout never waits on a buffer (bd_30-agents-dwm.37).
        out.flush()
    }
}

/// One result row as the corpus expects it: cells tab-separated, each one
/// through `Display for Value`. The `.slt` runner formats through this too,
/// so the REPL and the corpus cannot drift apart.
pub fn format_row(row: &[Value]) -> String {
    row.iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\t")
}

/// Split SQL text on statement-terminating semicolons, returning the complete
/// statements and the unterminated tail (empty when the text ended with `;`).
///
/// A `;` inside a single-quoted string or after `--` on the same line is data,
/// not a terminator. `''` needs no special case: the two quotes toggle in and
/// straight back out of the string. Statements keep their leading whitespace
/// and comments — the lexer already skips both.
fn split_statements(sql: &str) -> (Vec<&str>, &str) {
    let mut stmts = Vec::new();
    let bytes = sql.as_bytes();
    let mut start = 0;
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                in_string = !in_string;
                i += 1;
            }
            _ if in_string => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b';' => {
                stmts.push(&sql[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    (stmts, &sql[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semicolon_in_string_does_not_split() {
        let (stmts, tail) = split_statements("INSERT INTO t VALUES ('a;b');");
        assert_eq!(stmts, vec!["INSERT INTO t VALUES ('a;b')"]);
        assert_eq!(tail, "");
    }

    #[test]
    fn escaped_quote_keeps_string_state() {
        let (stmts, tail) = split_statements("SELECT 'it''s; fine';SELECT 1;");
        assert_eq!(stmts, vec!["SELECT 'it''s; fine'", "SELECT 1"]);
        assert_eq!(tail, "");
    }

    #[test]
    fn semicolon_in_comment_does_not_split() {
        let (stmts, tail) = split_statements("SELECT 1 -- a; comment\n;");
        assert_eq!(stmts, vec!["SELECT 1 -- a; comment\n"]);
        assert_eq!(tail, "");
    }

    #[test]
    fn several_statements_on_one_line() {
        let (stmts, tail) = split_statements("SELECT 1; SELECT 2; SELECT 3;");
        assert_eq!(stmts, vec!["SELECT 1", " SELECT 2", " SELECT 3"]);
        assert_eq!(tail, "");
    }

    #[test]
    fn trailing_statement_without_semicolon_is_the_tail() {
        let (stmts, tail) = split_statements("SELECT 1;\nSELECT 2\n");
        assert_eq!(stmts, vec!["SELECT 1"]);
        assert_eq!(tail, "\nSELECT 2\n");
    }

    #[test]
    fn string_spanning_lines_is_not_split() {
        let (stmts, tail) = split_statements("SELECT 'a\nb;c';");
        assert_eq!(stmts, vec!["SELECT 'a\nb;c'"]);
        assert_eq!(tail, "");
    }

    #[test]
    fn blank_and_comment_only_input_produces_no_output() {
        let mut s = Db::open(Path::new("unused")).unwrap();
        for input in ["", "   ", "\n\n", "-- just a comment"] {
            let mut out = Vec::new();
            s.run_text(input, &mut out).unwrap();
            assert!(out.is_empty(), "{input:?} printed {out:?}");
        }
    }

    #[test]
    fn statement_error_is_printed_and_the_loop_continues() {
        let mut s = Db::open(Path::new("unused")).unwrap();
        let mut out = Vec::new();
        s.run_text("SELECT 1;\nSELECT 2;\n", &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out.lines().count(), 2, "both statements reported: {out:?}");
        assert!(out.lines().all(|l| l.starts_with("Error: ")), "{out:?}");
    }

    #[test]
    fn a_row_prints_tab_separated_through_display() {
        let row = vec![
            Value::Int(1),
            Value::Text("two".into()),
            Value::Bool(true),
            Value::Bool(false),
            Value::Null,
        ];
        assert_eq!(format_row(&row), "1\ttwo\tTRUE\tFALSE\tNULL");
        assert_eq!(format_row(&[]), "");
    }

    #[test]
    fn unknown_flag_and_missing_db_argument_are_cli_errors() {
        assert!(run(vec!["--nope".into()]).is_err());
        assert!(run(vec!["--db".into()]).is_err());
        assert!(run(vec!["no/such/script.sql".into()]).is_err());
    }
}
