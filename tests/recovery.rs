//! bead: quern-recovery-test — crash recovery, docs/quern.md §4 (WAL) and §5.
//!
//! Three tests, two of them `kill -9` tests against the real `quern` binary as
//! a child process:
//!
//! * uncommitted work must be absent after a crash — `ROLLBACK` and `kill -9`
//!   leave identical bytes (no commit record), and replay discards both;
//! * committed work must be present after a crash — this is the half that
//!   catches a WAL that forgets to fsync;
//! * and the same discard property asserted through `Db::open` alone, with no
//!   child process, so there is one green assertion of it regardless of how far
//!   the REPL's `sql -> rows` wiring has got.
//!
//! # Synchronisation: no sleeps
//!
//! The CLI contract pinned by bead .15 puts the `quern> ` prompt on **stderr**
//! and flushes stdout after every statement, precisely so a driver can tell
//! when a statement finished. Reading stderr until the next prompt appears is
//! therefore an exact happens-after edge — including after `COMMIT`, whose
//! fsync has returned by then. Nothing here sleeps or polls.
//!
//! The only unbounded wait is that stderr read. It ends when the prompt
//! arrives, or with a named panic when the child dies (read returns 0). A child
//! that hung *without* dying would hang the test; there is no thread and no
//! timeout guarding that, because introducing one costs more than it buys for a
//! child we feed a fixed script.
//!
//! # Why two of the three are `#[ignore]`d
//!
//! Not because they are flaky: because the engine they drive is not connected
//! yet. `repl::Db::execute` still returns `not implemented: parser, planner and
//! executor` for every statement (bead .44, `plan::execute`, is unwritten), and
//! `repl::Db` is a placeholder that never opens the real `storage::Db`, so a run
//! today creates no database at all. The assertions below are the ones §5 asks
//! for and are deliberately NOT weakened to pass against that; remove both
//! `#[ignore]`s once .44 lands and the REPL holds a `storage::Db`.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use quern::repl::format_row;
use quern::storage::wal::{Wal, KIND_INSERT};
use quern::storage::{Db, Storage};
use quern::types::{Column, Row, Schema, Type, Value};

/// The interactive prompt, per the CLI contract. `  ...> ` (continuation) is
/// never expected here: every statement we send is `;`-terminated on one line.
const PROMPT: &str = "quern> ";

#[test]
#[ignore = "blocked on bd_30-agents-dwm.44: the REPL has no planner wired in yet"]
fn kill_9_before_commit_discards_the_open_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn(dir.path());

    // Committed baseline: an implicit transaction, so it fsyncs the WAL and
    // flushes the pages before the prompt we wait for comes back.
    statement(&mut child, "CREATE TABLE t (a INT PRIMARY KEY, b TEXT);");
    statement(&mut child, "INSERT INTO t VALUES (1, 'committed');");

    // Then an explicit transaction that never commits. The DELETE is here so
    // the test also proves the crash did not take the committed row with it.
    statement(&mut child, "BEGIN;");
    statement(&mut child, "INSERT INTO t VALUES (2, 'crashed');");
    statement(&mut child, "DELETE FROM t WHERE a = 1;");

    let out = kill9(&mut child);

    // No commit record was ever written for that transaction, so replay drops
    // its records exactly as it drops a ROLLBACK's.
    assert_eq!(
        rows(dir.path(), "t"),
        vec!["1\tcommitted".to_string()],
        "uncommitted INSERT and DELETE must both be gone, and the committed row \
         must survive. child stdout:\n{out}"
    );
}

#[test]
#[ignore = "blocked on bd_30-agents-dwm.44: the REPL has no planner wired in yet"]
fn kill_9_after_commit_keeps_the_committed_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn(dir.path());

    statement(&mut child, "CREATE TABLE t (a INT PRIMARY KEY, b TEXT);");
    statement(&mut child, "BEGIN;");
    statement(&mut child, "INSERT INTO t VALUES (1, 'durable');");
    statement(&mut child, "INSERT INTO t VALUES (2, 'durable');");
    // `statement` returns only once the prompt after COMMIT has been read, and
    // the prompt is written after the statement finished — so COMMIT's fsync of
    // the WAL has returned before the kill below. This is the half of the test
    // that fails if the WAL buffers instead of fsyncing.
    statement(&mut child, "COMMIT;");

    let out = kill9(&mut child);

    assert_eq!(
        rows(dir.path(), "t"),
        vec!["1\tdurable".to_string(), "2\tdurable".to_string()],
        "a COMMIT that returned must survive a kill -9. child stdout:\n{out}"
    );
}

/// The same property as the first test, in-process and without the REPL: a WAL
/// holding an uncommitted transaction's records must not replay them.
///
/// This is what a `kill -9` mid-transaction leaves behind — a mutation record
/// with no commit record — written here directly through the WAL API instead of
/// by crashing a child, so it holds regardless of the state of the REPL wiring.
#[test]
fn open_discards_an_uncommitted_transaction_left_in_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = Db::open(dir.path()).unwrap();
        db.create_table(&schema()).unwrap();
        let row: Row = vec![Value::Int(1), Value::Text("committed".into())];
        db.insert("t", &row).unwrap();
    }

    // The payload is deliberately not a valid encoded row: it is never decoded,
    // because the record is never replayed. If `Db::open` did replay it,
    // `decode_row` would fail and the reopen below would return Err — so this
    // asserts the record was discarded, not merely that no row appeared.
    Wal::open(&dir.path().join("quern.wal"))
        .unwrap()
        .append(7, KIND_INSERT, "t", b"not an encoded row")
        .unwrap();

    assert_eq!(rows(dir.path(), "t"), vec!["1\tcommitted".to_string()]);

    // And the log is spent: recovery checkpoints unconditionally, so a later
    // process reusing txn id 7 cannot resurrect those records.
    let wal = Wal::open(&dir.path().join("quern.wal")).unwrap();
    assert!(wal.replay().unwrap().is_empty(), "open must checkpoint");
}

fn schema() -> Schema {
    Schema {
        table: "t".into(),
        columns: vec![
            Column {
                name: "a".into(),
                ty: Type::Int,
                primary_key: true,
            },
            Column {
                name: "b".into(),
                ty: Type::Text,
                primary_key: false,
            },
        ],
    }
}

/// The built binary, interactive (no script argument) on a temp database.
/// `CARGO_BIN_EXE_quern` is set by cargo for integration tests, so there is no
/// path guessing and no dependency on the profile or target dir.
fn spawn(dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_quern"))
        .arg("--db")
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|mut child| {
            // The prompt is printed before the first read, too.
            await_prompt(&mut child, "startup");
            child
        })
        .expect("spawn quern")
}

/// Send one statement and return once the child has printed the next prompt —
/// i.e. once that statement, and any fsync it performed, has finished.
fn statement(child: &mut Child, sql: &str) {
    let stdin = child.stdin.as_mut().expect("stdin piped");
    writeln!(stdin, "{sql}").expect("write statement");
    stdin.flush().expect("flush statement");
    await_prompt(child, sql);
}

/// Read stderr byte by byte until it ends with the prompt. Byte-at-a-time is
/// fine for a 7-byte prompt and avoids buffering past it, which matters: the
/// bytes after a prompt belong to the next statement's window.
fn await_prompt(child: &mut Child, after: &str) {
    let stderr = child.stderr.as_mut().expect("stderr piped");
    let mut seen = String::new();
    let mut byte = [0u8; 1];
    while !seen.ends_with(PROMPT) {
        match stderr.read(&mut byte) {
            Ok(0) => panic!("quern exited without a prompt after {after:?}; stderr: {seen:?}"),
            Ok(_) => seen.push(byte[0] as char),
            Err(e) => panic!("reading quern stderr after {after:?}: {e}"),
        }
    }
}

/// `kill -9` (std's `kill` is SIGKILL on unix — no libc dependency), reap, and
/// return whatever the child had printed, for failure messages.
fn kill9(child: &mut Child) -> String {
    child.kill().expect("kill -9 quern");
    child.wait().expect("reap quern");
    let mut out = String::new();
    if let Some(stdout) = child.stdout.as_mut() {
        let _ = stdout.read_to_string(&mut out);
    }
    out
}

/// Reopen the database — which runs recovery — and return every row of `table`,
/// formatted through the one formatter the REPL and the `.slt` corpus use, and
/// sorted so the assertion does not depend on heap order.
fn rows(dir: &Path, table: &str) -> Vec<String> {
    let db = Db::open(dir).expect("reopen the database after the crash");
    let mut rows: Vec<String> = db
        .scan(table)
        .unwrap_or_else(|e| {
            // `no such table` here means the child's statements never reached
            // storage at all, not that recovery lost them — see bead .44.
            panic!("scan {table} after reopen: {e}")
        })
        .map(|r| format_row(&r.expect("row").1))
        .collect();
    rows.sort();
    rows
}
