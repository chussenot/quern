//! bead: bd_30-agents-dwm.27 — the `.slt` harness that grades the corpus.
//!
//! `docs/quern.md` §5 pins six format rules and §6 pins the comment rule.
//! They are implemented here literally, and nothing in this file is lenient
//! beyond them: a line outside a block that is neither blank nor a `#` comment
//! is a malformed-corpus error, a `query` block with no `----` is a
//! malformed-corpus error, and every mismatch is reported with the file, the
//! line of the block, the SQL, the expected rows and the actual rows so a
//! failure is diagnosable without re-running it by hand.
//!
//! One `Db` per file, on its own `tempfile` tempdir, statements fed in file
//! order — so a file's `CREATE TABLE`s are visible to its later cases and no
//! file can see another's tables.
//!
//! Rows are rendered with `quern::repl::format_row`, the same function the REPL
//! prints through, so the corpus and the REPL cannot drift.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use quern::plan::{self, Outcome};
use quern::repl::format_row;
use quern::storage::Db;

/// What a block asserts. `Query` carries its expected rows (rule 2), which may
/// legally be empty (rule 3).
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Ok,
    Error,
    Query(Vec<String>),
}

struct Block {
    /// 1-based line of the directive, for the failure message.
    line: usize,
    kind: Kind,
    sql: String,
}

/// Rules 1-3 plus §6. `file` only ever appears in error text.
///
/// Panics on a malformed corpus: the harness grades the fleet, so a file it
/// cannot parse is a failure of the corpus, never a silently skipped case.
fn parse(text: &str, file: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        // §6: a line whose first character is `#` is a comment. Between blocks
        // only — inside one it would be part of the statement text rule 6 scans.
        if line.trim().is_empty() || line.starts_with('#') {
            i += 1;
            continue;
        }
        let directive_line = i + 1;
        // Rule 1: a block starts on a line *beginning* with one of the three.
        let is_query = if line.starts_with("statement ok") || line.starts_with("statement error") {
            false
        } else if line.starts_with("query") {
            true
        } else {
            panic!(
                "{file}:{directive_line}: malformed corpus: a line outside a block must be blank, \
                 a '#' comment, or one of `statement ok` / `statement error` / `query`; got {line:?}"
            );
        };
        let is_error = line.starts_with("statement error");
        i += 1;

        // Rule 1: SQL runs from the next line to `----` (query) or to the next
        // blank line or EOF (statement).
        let sql_start = i;
        let mut saw_separator = false;
        while i < lines.len() {
            if is_query {
                if lines[i] == "----" {
                    saw_separator = true;
                    break;
                }
            } else if lines[i].trim().is_empty() {
                break;
            }
            i += 1;
        }
        let sql = lines[sql_start..i].join("\n");
        if sql.trim().is_empty() {
            panic!("{file}:{directive_line}: malformed corpus: block has no SQL");
        }
        if is_query && !saw_separator {
            panic!(
                "{file}:{directive_line}: malformed corpus: `query` block reached EOF with no \
                 `----` separator"
            );
        }

        let kind = if !is_query {
            if is_error {
                Kind::Error
            } else {
                Kind::Ok
            }
        } else {
            // Rule 2: expected rows run from the line after `----` to the next
            // blank line or EOF. Rule 3: zero of them is legal and means the
            // query must return no rows.
            i += 1; // past `----`
            let rows_start = i;
            while i < lines.len() && !lines[i].trim().is_empty() {
                i += 1;
            }
            Kind::Query(lines[rows_start..i].iter().map(|s| s.to_string()).collect())
        };

        blocks.push(Block {
            line: directive_line,
            kind,
            sql,
        });
    }
    blocks
}

/// Rules 5 and 6. `None` on a match; on a mismatch, the detail lines of the
/// failure report.
fn compare(sql: &str, expected: &[String], actual: &[String]) -> Option<String> {
    // Rule 6: ordered iff the statement text contains `ORDER BY`,
    // case-insensitively. Deliberately textual — the runner has no plan access.
    let ordered = sql.to_ascii_uppercase().contains("ORDER BY");
    let (mut exp, mut act) = (expected.to_vec(), actual.to_vec());
    if !ordered {
        // Rule 5: sort both vectors by Rust's `str` byte order and compare
        // element by element, so duplicates must match in count.
        exp.sort();
        act.sort();
    }
    if exp == act {
        return None;
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "  comparison: {} (rule {})",
        if ordered { "ordered" } else { "unordered" },
        if ordered { 6 } else { 5 }
    );
    let _ = writeln!(out, "  expected {} row(s):", exp.len());
    for r in &exp {
        let _ = writeln!(out, "    {r:?}");
    }
    let _ = writeln!(out, "  actual {} row(s):", act.len());
    for r in &act {
        let _ = writeln!(out, "    {r:?}");
    }
    Some(out)
}

fn indent(sql: &str) -> String {
    sql.lines()
        .map(|l| format!("    {l}\n"))
        .collect::<String>()
}

/// What the run actually asserted. A green suite that compared nothing is the
/// failure mode worth spending four counters on: if `rows_compared` collapses,
/// the harness stopped grading even though it still says "passed".
#[derive(Default)]
struct Tally {
    ok: usize,
    error: usize,
    query: usize,
    rows_compared: usize,
}

/// Runs one file against a fresh database. Returns (blocks run, failure reports).
fn run_file(path: &Path, tally: &mut Tally) -> (usize, Vec<String>) {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let text = std::fs::read_to_string(path).expect("read corpus file");
    let blocks = parse(&text, &name);
    let dir = tempfile::tempdir().expect("tempdir");
    let mut db = Db::open(dir.path()).expect("open a fresh database");

    let mut failures = Vec::new();
    for block in &blocks {
        let loc = format!("{name}:{}", block.line);
        match &block.kind {
            Kind::Ok => tally.ok += 1,
            Kind::Error => tally.error += 1,
            Kind::Query(expected) => {
                tally.query += 1;
                tally.rows_compared += expected.len();
            }
        }
        let result = plan::execute(&block.sql, &mut db);
        let detail = match (&block.kind, result) {
            (Kind::Ok, Ok(_)) => None,
            (Kind::Ok, Err(e)) => Some(format!(
                "  `statement ok`, but the engine returned an error:\n    {e}\n"
            )),
            (Kind::Error, Err(_)) => None,
            (Kind::Error, Ok(outcome)) => Some(format!(
                "  `statement error`, but the statement succeeded: {}\n",
                describe(&outcome)
            )),
            (Kind::Query(expected), Ok(Outcome::Rows { rows, .. })) => {
                let actual: Vec<String> = rows.iter().map(|r| format_row(r)).collect();
                compare(&block.sql, expected, &actual)
            }
            (Kind::Query(_), Ok(outcome)) => Some(format!(
                "  `query`, but the statement returned no result set: {}\n",
                describe(&outcome)
            )),
            (Kind::Query(_), Err(e)) => Some(format!(
                "  `query`, but the engine returned an error:\n    {e}\n"
            )),
        };
        if let Some(detail) = detail {
            failures.push(format!("{loc}\n  sql:\n{}{detail}", indent(&block.sql)));
        }
    }
    (blocks.len(), failures)
}

fn describe(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Rows { rows, .. } => format!("Rows({} row(s))", rows.len()),
        Outcome::Count(n) => format!("Count({n})"),
        Outcome::Done => "Done".to_string(),
    }
}

/// The grade. Every `.slt` file in `tests/logic`, in sorted order, each against
/// its own fresh database. Every failure in the corpus is reported in one run —
/// stopping at the first would make the harness useless for grading.
#[test]
fn logic_corpus() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/logic");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("tests/logic must exist")
        .map(|e| e.expect("read dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "slt"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no .slt files in {}", dir.display());

    let mut total = 0usize;
    let mut all = Vec::new();
    let mut report = String::new();
    let mut tally = Tally::default();
    for path in &files {
        let (run, failures) = run_file(path, &mut tally);
        total += run;
        let _ = writeln!(
            report,
            "{}: {run} blocks, {} passed, {} failed",
            path.file_name().unwrap().to_string_lossy(),
            run - failures.len(),
            failures.len()
        );
        all.extend(failures);
    }
    let _ = writeln!(
        report,
        "\ntotal: {total} blocks, {} passed, {} failed\n\
         asserted: {} `statement ok`, {} `statement error`, {} `query` \
         ({} expected rows compared)",
        total - all.len(),
        all.len(),
        tally.ok,
        tally.error,
        tally.query,
        tally.rows_compared
    );
    eprintln!("{report}");

    if !all.is_empty() {
        let mut msg = format!(
            "\n{} of {total} .slt blocks failed\n\n{}\n",
            all.len(),
            report
        );
        for f in &all {
            let _ = writeln!(msg, "{f}");
        }
        panic!("{msg}");
    }
}

// The parser and the two comparison rules are the only non-trivial logic in
// this file, and a bug in either silently mis-grades the fleet. These check
// them against the rules directly, with no engine involved.

#[test]
fn parses_the_three_directives_and_skips_comments() {
    let text = "\
# a comment
statement ok
CREATE TABLE t (a INT);

# another comment

statement error
INSERT INTO nosuch VALUES (1);

query
SELECT a FROM t
  ORDER BY a;
----
1
2
";
    let blocks = parse(text, "mem.slt");
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks[0].kind, Kind::Ok);
    assert_eq!(blocks[0].line, 2);
    assert_eq!(blocks[0].sql, "CREATE TABLE t (a INT);");
    assert_eq!(blocks[1].kind, Kind::Error);
    assert_eq!(blocks[1].line, 7);
    // Rule 1: SQL may span several lines.
    assert_eq!(blocks[2].sql, "SELECT a FROM t\n  ORDER BY a;");
    assert_eq!(
        blocks[2].kind,
        Kind::Query(vec!["1".to_string(), "2".to_string()])
    );
}

#[test]
fn rule_3_zero_expected_rows() {
    // `----` then a blank line, and `----` then EOF, are both zero rows.
    let blocks = parse(
        "query\nSELECT a FROM t;\n----\n\nquery\nSELECT b FROM t;\n----\n",
        "m",
    );
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].kind, Kind::Query(vec![]));
    assert_eq!(blocks[1].kind, Kind::Query(vec![]));
}

#[test]
fn rule_5_is_a_sorted_multiset_and_rule_6_is_ordered() {
    let unordered = "SELECT a FROM t;";
    let rows = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    // Order does not matter without ORDER BY...
    assert!(compare(unordered, &rows(&["1", "2"]), &rows(&["2", "1"])).is_none());
    // ...but multiplicity does.
    assert!(compare(unordered, &rows(&["1", "1"]), &rows(&["1"])).is_some());
    assert!(compare(unordered, &rows(&["1"]), &rows(&["1", "1"])).is_some());
    // Rule 3: zero expected rows requires zero back.
    assert!(compare(unordered, &[], &rows(&["1"])).is_some());
    assert!(compare(unordered, &[], &[]).is_none());
    // Rule 6: ORDER BY anywhere in the text, case-insensitive, means ordered.
    assert!(compare(
        "SELECT a FROM t order by a;",
        &rows(&["1", "2"]),
        &rows(&["2", "1"])
    )
    .is_some());
    assert!(compare(
        "SELECT a FROM t ORDER BY a;",
        &rows(&["1", "2"]),
        &rows(&["1", "2"])
    )
    .is_none());
}

#[test]
#[should_panic(expected = "malformed corpus")]
fn a_stray_line_outside_a_block_is_a_corpus_error() {
    parse("statement ok\nCREATE TABLE t (a INT);\n\nSELECT 1;\n", "m");
}

#[test]
#[should_panic(expected = "no `----` separator")]
fn a_query_block_without_a_separator_is_a_corpus_error() {
    parse("query\nSELECT a FROM t;\n", "m");
}
