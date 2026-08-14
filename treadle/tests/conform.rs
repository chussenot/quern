//! bead: conform-runner (`bd_30-agents-2jk.21`) — §5 grading: every
//! `tests/conform/*.tr` case, against **every** engine.
//!
//! # The comparison rule, implemented literally
//!
//! §5: the assertion is a **byte comparison of a canonical rendering** — never a
//! comparison made by splitting either side into lines. The rendering is
//! `Display for Output` (`treadle::output`): every print line
//! newline-**terminated**, then the error's display form, also
//! newline-terminated. So the whole assertion is one `==` on two `String`s:
//!
//! ```text
//! case.expect == engine.run(&case.source).to_string()
//! ```
//!
//! Nothing here splits, trims or *classifies* either side, and that is load
//! bearing rather than tidy. A line-splitting runner cannot tell `lines == []`
//! (printed nothing, renders `""`) from `lines == [""]` (printed one empty line,
//! renders `"\n"`), and it fails a **correct** engine on `print "a\nb";` — one
//! line whose bytes contain a newline. It is also what lets a printed line that
//! itself begins with `error: ` need no rule of its own: it renders
//! byte-identically to a real error and is *supposed* to.
//!
//! The one place a line is classified is the file **header**, before the
//! `--- source` delimiter, where `#` starts a comment. Inside the source or
//! expect sections no line is ever looked at except to test byte-equality
//! against the `--- expect` delimiter, so `# not a comment` in a program and
//! `print "--- expect";` both mean exactly what they say.
//!
//! # Both engines, blind
//!
//! Every case runs against every entry of [`engines()`] through
//! `&mut dyn Engine`, so this file cannot tell which implementation it is
//! driving and therefore cannot be written to suit one. Two engines that
//! **disagree with each other** is the most valuable failure the suite can
//! produce — it means the two implementations differ, which is the entire point
//! of the project — so it is reported as a `DIVERGENCE`, separately from and
//! louder than a plain failure.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use treadle::engine::Engine;
use treadle::output::{diff, Output};

/// Every engine the corpus is graded against.
///
/// **Adding an engine is one line here and nothing else in this file changes.**
/// Neither engine exists yet (`src/vm/machine.rs` and `src/tree/eval.rs` are
/// stubs, and nothing in the crate implements [`Engine`]), so the list is empty
/// and [`every_case_against_every_engine`] is `#[ignore]`d — see the reason on
/// that test. It also asserts the list is non-empty, so an empty `engines()` can
/// never masquerade as a green run over zero engines.
fn engines() -> Vec<Box<dyn Engine>> {
    #[allow(unused_mut)]
    let mut engines: Vec<Box<dyn Engine>> = Vec::new();
    // Uncomment one line per engine as it lands, and drop the `#[ignore]`:
    engines.push(Box::new(treadle::vm::Vm::new())); // bd .16 / .17
    // engines.push(Box::new(treadle::tree::eval::Interp::new())); // bd .19 / .20
    engines
}

// ---------------------------------------------------------------------------
// The `.tr` parser. No engine involved; unit-tested at the bottom of this file.
// ---------------------------------------------------------------------------

/// Byte-exact, no trimming: a delimiter is a line *equal* to this.
const SOURCE_DELIM: &str = "--- source";
/// The **first** line equal to this ends the source section (§5).
const EXPECT_DELIM: &str = "--- expect";

/// One conformance case: §5's two sections, as bytes.
#[derive(Debug, PartialEq, Eq)]
struct Case {
    /// Every byte after the `--- source` line, up to the **first** `--- expect`
    /// line.
    source: String,
    /// Every remaining byte of the file, verbatim — the expected canonical
    /// rendering.
    expect: String,
    /// 1-based line of the `--- source` delimiter, named in failure reports.
    block_line: usize,
}

/// Split one `.tr` file into its two sections, or say why it is malformed.
///
/// A malformed file is an `Err` that the caller turns into a failure, never a
/// skip: a corpus that quietly stops grading looks exactly like one that passes.
fn parse_case(text: &str) -> Result<Case, String> {
    // `(byte offset where the program starts, 1-based line of the delimiter)`
    let mut source_at: Option<(usize, usize)> = None;
    let mut offset = 0usize;

    for (i, chunk) in text.split_inclusive('\n').enumerate() {
        let line = chunk.strip_suffix('\n').unwrap_or(chunk);
        let next = offset + chunk.len();
        match source_at {
            // Header. This is the ONLY place a line is classified, and so the
            // only place `#` means comment.
            None => {
                if line == SOURCE_DELIM {
                    source_at = Some((next, i + 1));
                } else if !line.starts_with('#') && !line.trim().is_empty() {
                    return Err(format!(
                        "line {}: expected a `#` comment or `{SOURCE_DELIM}`, found {line:?}",
                        i + 1
                    ));
                }
            }
            // Source section. Nothing is classified here; we are only looking
            // for the first line byte-equal to the expect delimiter.
            Some((start, block_line)) => {
                if line == EXPECT_DELIM {
                    return Ok(Case {
                        source: text[start..offset].to_string(),
                        // Every remaining byte, verbatim. Any later
                        // `--- expect` line is just part of the expectation.
                        expect: text[next..].to_string(),
                        block_line,
                    });
                }
            }
        }
        offset = next;
    }

    Err(match source_at {
        None => format!("no `{SOURCE_DELIM}` line"),
        Some((_, at)) => format!("`{SOURCE_DELIM}` at line {at} has no `{EXPECT_DELIM}` after it"),
    })
}

/// The `Output` that would render `expect`, **for the failure report only**.
///
/// The assertion is always on the bytes; this exists so a failure can be shown
/// through [`diff`] instead of as a `Debug` wall. It is display-preserving
/// exactly when `expect` is empty or ends in a newline, which every real
/// rendering does — [`representable`] is the check, and a case that fails it can
/// never be matched by any engine.
fn expect_as_output(expect: &str) -> Output {
    let mut out = Output::new();
    for chunk in expect.split_inclusive('\n') {
        out.push_line(chunk.strip_suffix('\n').unwrap_or(chunk));
    }
    out
}

/// Whether some `Output` could render exactly these bytes.
fn representable(expect: &str) -> bool {
    expect_as_output(expect).to_string() == expect
}

// ---------------------------------------------------------------------------
// Loading the corpus
// ---------------------------------------------------------------------------

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conform")
}

/// Every `*.tr` in sorted order (§5), parsed. Panics on a malformed file.
fn load_corpus() -> Vec<(PathBuf, Case)> {
    let dir = corpus_dir();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read corpus dir {}: {e}", dir.display()))
        .map(|e| e.expect("corpus dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "tr"))
        .collect();
    paths.sort();

    let mut cases = Vec::new();
    let mut malformed = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        match parse_case(&text) {
            Ok(case) => cases.push((path, case)),
            Err(why) => malformed.push(format!("  {}: {why}", path.display())),
        }
    }
    assert!(
        malformed.is_empty(),
        "malformed conformance file(s) — a file that cannot be parsed is a \
         failure, never a skip:\n{}",
        malformed.join("\n")
    );
    assert!(
        !cases.is_empty(),
        "no *.tr cases found under {} — an empty corpus grades nothing and \
         would otherwise pass",
        dir.display()
    );
    cases
}

// ---------------------------------------------------------------------------
// Failure reporting
// ---------------------------------------------------------------------------

/// What one engine did with one case.
struct Run {
    name: &'static str,
    out: Output,
    /// `out.to_string()`, computed once — the only thing compared.
    rendered: String,
    passed: bool,
}

fn indent(s: &str) -> String {
    s.lines().fold(String::new(), |mut acc, l| {
        let _ = writeln!(acc, "    {l}");
        acc
    })
}

/// File, block line and source — the same preamble for both kinds of failure.
fn preamble(kind: &str, path: &Path, case: &Case) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{kind} {} (block at line {})",
        path.display(),
        case.block_line
    );
    let _ = writeln!(s, "  source:");
    if case.source.is_empty() {
        let _ = writeln!(s, "    | <empty program>");
    }
    for line in case.source.split_inclusive('\n') {
        let _ = writeln!(s, "    | {}", line.strip_suffix('\n').unwrap_or(line));
    }
    s
}

/// One engine against the expectation, through [`diff`] rather than `Debug`.
fn vs_expect(case: &Case, run: &Run) -> String {
    let expected = expect_as_output(&case.expect);
    let mut s = indent(&diff("expect", &expected, run.name, &run.out));
    if !representable(&case.expect) {
        let _ = writeln!(
            s,
            "    NOTE: this expect section is not a representable rendering — every \
             non-empty rendering ends in a newline — so NO engine can ever match it. \
             Check the file's final newline, and bd_30-agents-2jk.34 (the *.tr \
             exclusion from the whitespace-fixing pre-commit hooks)."
        );
    }
    s
}

// ---------------------------------------------------------------------------
// Grading one case
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Every engine's rendering equals the expectation, byte for byte.
    Pass,
    /// Every engine agreed with every other engine, and all of them differ from
    /// `--- expect`: one bug, or one wrong expectation.
    Fail,
    /// Two engines rendered different bytes. At most one of them can be right,
    /// so the two *implementations* disagree about the language — a strictly
    /// bigger finding than a failure, and the one this suite exists to produce.
    /// Subsumes "passes on one engine, fails on the other", and also catches
    /// both engines being wrong in *different* ways.
    Divergence,
}

struct Graded {
    verdict: Verdict,
    /// Pass flag per engine, in `engines()` order.
    passed: Vec<bool>,
    /// Empty exactly when the verdict is [`Verdict::Pass`].
    report: String,
}

/// Run one case against every engine through `&mut dyn Engine` and judge it.
fn grade(path: &Path, case: &Case, engines: &mut [Box<dyn Engine>]) -> Graded {
    // Run every engine on the same bytes first, judge after.
    let runs: Vec<Run> = engines
        .iter_mut()
        .map(|e| {
            let name = e.name();
            let out = e.run(&case.source);
            let rendered = out.to_string();
            // §5, the whole assertion: a byte comparison of the canonical
            // rendering. No splitting, no trimming, no classifying.
            let passed = rendered == case.expect;
            Run {
                name,
                out,
                rendered,
                passed,
            }
        })
        .collect();

    let passed: Vec<bool> = runs.iter().map(|r| r.passed).collect();
    let diverged = runs.iter().any(|r| r.rendered != runs[0].rendered);
    let mut report = String::new();

    if diverged {
        report.push_str(&preamble("!!! DIVERGENCE", path, case));
        let _ = writeln!(
            report,
            "  !!! THE ENGINES DISAGREE WITH EACH OTHER. This is the finding this \
             suite exists to produce; it is not a single test failure."
        );
        let _ = writeln!(
            report,
            "  passed: [{}]   failed: [{}]",
            runs.iter()
                .filter(|r| r.passed)
                .map(|r| r.name)
                .collect::<Vec<_>>()
                .join(", "),
            runs.iter()
                .filter(|r| !r.passed)
                .map(|r| r.name)
                .collect::<Vec<_>>()
                .join(", "),
        );
        let _ = writeln!(report, "  engine vs engine:");
        for other in runs
            .iter()
            .skip(1)
            .filter(|r| r.rendered != runs[0].rendered)
        {
            report.push_str(&indent(&diff(
                runs[0].name,
                &runs[0].out,
                other.name,
                &other.out,
            )));
        }
        for run in runs.iter().filter(|r| !r.passed) {
            let _ = writeln!(report, "  {} vs expect:", run.name);
            report.push_str(&vs_expect(case, run));
        }
        report.push('\n');
        return Graded {
            verdict: Verdict::Divergence,
            passed,
            report,
        };
    }

    match runs.iter().find(|r| !r.passed) {
        // Every engine agreed, and every engine is wrong the same way.
        Some(run) => {
            report.push_str(&preamble("FAIL", path, case));
            let _ = writeln!(
                report,
                "  all {} engine(s) agree with each other and differ from `--- expect`",
                runs.len()
            );
            report.push_str(&vs_expect(case, run));
            report.push('\n');
            Graded {
                verdict: Verdict::Fail,
                passed,
                report,
            }
        }
        None => Graded {
            verdict: Verdict::Pass,
            passed,
            report,
        },
    }
}

// ---------------------------------------------------------------------------
// The corpus run
// ---------------------------------------------------------------------------

#[test]
fn every_case_against_every_engine() {
    let cases = load_corpus();
    let mut engines = engines();
    assert!(
        !engines.is_empty(),
        "engines() is empty: a conformance run over zero engines is a vacuous \
         green, which is what this assert exists to prevent. Add the engine \
         (bd_30-agents-2jk.16/.17 for vm, .19/.20 for tree)."
    );

    let mut pass = vec![0usize; engines.len()];
    let mut fail = vec![0usize; engines.len()];
    let mut divergences = 0usize;
    let mut report = String::new();

    for (path, case) in &cases {
        let graded = grade(path, case, &mut engines);
        for (i, ok) in graded.passed.iter().enumerate() {
            if *ok {
                pass[i] += 1;
            } else {
                fail[i] += 1;
            }
        }
        if graded.verdict == Verdict::Divergence {
            divergences += 1;
        }
        report.push_str(&graded.report);
    }

    // Totals, always — a harness that silently stopped grading must not be able
    // to look green. Per-engine pass + fail must account for every case.
    let mut totals = String::new();
    let _ = writeln!(
        totals,
        "conformance: {} file(s), {} case(s), {} engine(s)",
        cases.len(),
        cases.len(),
        engines.len()
    );
    for (i, e) in engines.iter().enumerate() {
        let _ = writeln!(
            totals,
            "  {}: {} passed, {} failed (of {})",
            e.name(),
            pass[i],
            fail[i],
            cases.len()
        );
        assert_eq!(
            pass[i] + fail[i],
            cases.len(),
            "engine {} was graded on {} of {} cases — grading stopped early",
            e.name(),
            pass[i] + fail[i],
            cases.len()
        );
    }
    let _ = writeln!(
        totals,
        "  divergences (engines disagreeing with each other): {divergences}"
    );
    println!("{totals}");

    assert!(report.is_empty(), "\n{report}{totals}");
}

/// Runs today, with no engine: every corpus file parses, and every expectation
/// is a rendering some `Output` could actually produce.
///
/// The second half is the guard for `bd_30-agents-2jk.34`: a whitespace-fixing
/// pre-commit hook that strips a `.tr`'s final newline turns an assertion into
/// one no engine can ever satisfy, and this says so now rather than as a
/// mystery failure once the engines land.
#[test]
fn every_corpus_file_parses_and_expects_a_representable_rendering() {
    let cases = load_corpus();
    let bad: Vec<String> = cases
        .iter()
        .filter(|(_, c)| !representable(&c.expect))
        .map(|(p, c)| {
            format!(
                "  {}: expect section is {} bytes and does not end in a newline",
                p.display(),
                c.expect.len()
            )
        })
        .collect();
    assert!(
        bad.is_empty(),
        "expect section(s) that no Output can render (see bd_30-agents-2jk.34):\n{}",
        bad.join("\n")
    );
    println!(
        "corpus: {} file(s), {} case(s) parsed",
        cases.len(),
        cases.len()
    );
}

// ---------------------------------------------------------------------------
// The grading itself, exercised today against stand-in engines.
//
// Neither real engine exists yet, but the verdicts, the DIVERGENCE detection and
// the report are logic, and untested logic that only runs the day the engines
// land is logic nobody has run. These fakes render a fixed `Output` for any
// program and know nothing about the language.
// ---------------------------------------------------------------------------

struct Fake {
    name: &'static str,
    out: Output,
}

impl Engine for Fake {
    fn name(&self) -> &'static str {
        self.name
    }
    fn run(&mut self, _src: &str) -> Output {
        self.out.clone()
    }
}

/// A stand-in engine that always prints exactly `lines`.
fn fake(name: &'static str, lines: &[&str]) -> Box<dyn Engine> {
    let mut out = Output::new();
    for l in lines {
        out.push_line(*l);
    }
    Box::new(Fake { name, out })
}

fn a_case(expect: &str) -> Case {
    Case {
        source: "print 1;\n".to_string(),
        expect: expect.to_string(),
        block_line: 2,
    }
}

#[test]
fn engines_that_agree_with_each_other_and_with_expect_pass() {
    let mut engines = vec![fake("vm", &["1"]), fake("tree", &["1"])];
    let g = grade(Path::new("t.tr"), &a_case("1\n"), &mut engines);
    assert_eq!(g.verdict, Verdict::Pass);
    assert_eq!(g.passed, vec![true, true]);
    assert!(
        g.report.is_empty(),
        "a pass must report nothing: {}",
        g.report
    );
}

#[test]
fn engines_that_agree_with_each_other_but_not_with_expect_are_a_plain_failure() {
    let mut engines = vec![fake("vm", &["2"]), fake("tree", &["2"])];
    let g = grade(Path::new("t.tr"), &a_case("1\n"), &mut engines);
    assert_eq!(g.verdict, Verdict::Fail);
    assert_eq!(g.passed, vec![false, false]);
    let r = &g.report;
    // Engine name, file, the block's line, the source, and the diff.
    assert!(r.starts_with("FAIL t.tr (block at line 2)"), "{r}");
    assert!(r.contains("| print 1;"), "source must be shown: {r}");
    assert!(r.contains("divergence: expect vs vm"), "must diff: {r}");
    assert!(r.contains("first differing byte: 0"), "must locate it: {r}");
    assert!(
        !r.contains("!!! DIVERGENCE"),
        "engines agreeing is NOT a divergence: {r}"
    );
}

#[test]
fn one_engine_passing_where_the_other_fails_is_reported_as_a_divergence() {
    let mut engines = vec![fake("vm", &["1"]), fake("tree", &["2"])];
    let g = grade(Path::new("t.tr"), &a_case("1\n"), &mut engines);
    assert_eq!(g.verdict, Verdict::Divergence);
    assert_eq!(g.passed, vec![true, false]);
    let r = &g.report;
    assert!(
        r.starts_with("!!! DIVERGENCE t.tr (block at line 2)"),
        "{r}"
    );
    assert!(r.contains("THE ENGINES DISAGREE WITH EACH OTHER"), "{r}");
    assert!(r.contains("passed: [vm]   failed: [tree]"), "{r}");
    assert!(
        r.contains("divergence: vm vs tree"),
        "engine vs engine: {r}"
    );
    assert!(
        r.contains("divergence: expect vs tree"),
        "vs expect too: {r}"
    );
    // `cargo test -- --nocapture` shows the shape of the loudest report.
    println!("{r}");
}

#[test]
fn printing_nothing_and_printing_one_empty_line_diverge() {
    // The `[]` vs `[""]` pair. A harness that split either side into lines could
    // not tell these apart and would call this agreement.
    let mut engines = vec![fake("vm", &[]), fake("tree", &[""])];
    let g = grade(Path::new("t.tr"), &a_case(""), &mut engines);
    assert_eq!(g.verdict, Verdict::Divergence);
    assert_eq!(g.passed, vec![true, false]);
    assert!(g.report.contains("ended (0 bytes total)"), "{}", g.report);
}

#[test]
fn a_correct_engine_is_not_failed_for_a_printed_line_containing_a_newline() {
    // `print "a\nb";` is ONE line whose bytes contain a newline. Comparing
    // `Output.lines` against the expect section split into lines would compare
    // `["a\nb"]` with `["a", "b"]` and fail a correct engine; the byte
    // comparison of the rendering does not.
    let mut engines = vec![fake("vm", &["a\nb"]), fake("tree", &["a\nb"])];
    let g = grade(Path::new("t.tr"), &a_case("a\nb\n"), &mut engines);
    assert_eq!(g.verdict, Verdict::Pass, "{}", g.report);
}

#[test]
fn every_engine_is_run_even_when_an_earlier_one_already_failed() {
    // Grading must not short-circuit: per-engine totals have to account for
    // every case, or a harness that stopped early looks green.
    let mut engines = vec![fake("a", &["x"]), fake("b", &["y"]), fake("c", &["1"])];
    let g = grade(Path::new("t.tr"), &a_case("1\n"), &mut engines);
    assert_eq!(g.passed, vec![false, false, true]);
    assert_eq!(g.verdict, Verdict::Divergence);
}

// ---------------------------------------------------------------------------
// Unit tests for the `.tr` parser itself — no engine involved.
// ---------------------------------------------------------------------------

#[test]
fn header_comments_and_blank_lines_are_skipped() {
    let c = parse_case("# one\n# two\n\n--- source\nprint 1;\n--- expect\n1\n").unwrap();
    assert_eq!(c.source, "print 1;\n");
    assert_eq!(c.expect, "1\n");
    assert_eq!(c.block_line, 4);
}

#[test]
fn a_hash_line_inside_the_source_is_program_text_not_a_comment() {
    // §5: no line inside a block is classified. `#` is a treadle comment here,
    // and it is the engine's business, not the harness's.
    let c = parse_case("--- source\n# a treadle comment\nprint 1;\n--- expect\n1\n").unwrap();
    assert_eq!(c.source, "# a treadle comment\nprint 1;\n");
}

#[test]
fn a_program_may_print_the_literal_expect_delimiter() {
    // §5 spells this out: a source LINE equal to `--- expect` is not
    // expressible, but `print "--- expect";` is, because that line is not
    // byte-equal to the delimiter.
    let c = parse_case("--- source\nprint \"--- expect\";\n--- expect\n--- expect\n").unwrap();
    assert_eq!(c.source, "print \"--- expect\";\n");
    assert_eq!(c.expect, "--- expect\n");
    // And the expectation is one printed line, with no error and no second split.
    let e = expect_as_output(&c.expect);
    assert_eq!(e.lines, vec!["--- expect".to_string()]);
    assert_eq!(e.error, None);
    assert_eq!(e.to_string(), c.expect);
}

#[test]
fn the_first_expect_delimiter_wins_and_later_ones_are_expected_output() {
    let c = parse_case("--- source\nprint 1;\n--- expect\n1\n--- expect\n2\n").unwrap();
    assert_eq!(c.source, "print 1;\n");
    assert_eq!(c.expect, "1\n--- expect\n2\n");
}

#[test]
fn an_expected_empty_output_is_the_empty_string() {
    // `lines == []` renders "", which is what a file ending at the delimiter
    // asserts — distinguishable from one printed empty line below.
    let c = parse_case("--- source\nlet x = 1;\n--- expect\n").unwrap();
    assert_eq!(c.expect, "");
    assert_eq!(expect_as_output("").to_string(), "");
    assert_eq!(expect_as_output("").lines, Vec::<String>::new());
}

#[test]
fn one_printed_empty_line_is_a_single_newline() {
    // The `[]` vs `[""]` pair a line-splitting runner cannot tell apart.
    let c = parse_case("--- source\nprint \"\";\n--- expect\n\n").unwrap();
    assert_eq!(c.expect, "\n");
    assert_eq!(expect_as_output("\n").lines, vec![String::new()]);
    assert_ne!(expect_as_output("\n"), expect_as_output(""));
}

#[test]
fn a_printed_line_beginning_with_error_is_not_classified_as_an_error() {
    let c = parse_case("--- source\nprint \"error: nope\";\n--- expect\nerror: nope\n").unwrap();
    let e = expect_as_output(&c.expect);
    assert_eq!(e.lines, vec!["error: nope".to_string()]);
    assert_eq!(e.error, None, "the harness must never classify a line");
    // And it renders byte-identically to the real error would-be — deliberately.
    assert_eq!(e.to_string(), c.expect);
}

#[test]
fn a_trailing_newline_in_the_expect_section_is_part_of_the_expectation() {
    // "1\n" and "1\n\n" are different assertions: the second expects a second,
    // empty, printed line. Verbatim bytes, no trimming.
    let one = parse_case("--- source\nprint 1;\n--- expect\n1\n").unwrap();
    let two = parse_case("--- source\nprint 1;\n--- expect\n1\n\n").unwrap();
    assert_eq!(one.expect, "1\n");
    assert_eq!(two.expect, "1\n\n");
    assert_ne!(one, two);
}

#[test]
fn a_file_whose_last_line_has_no_newline_still_parses_verbatim() {
    let c = parse_case("--- source\nprint 1;\n--- expect\n1").unwrap();
    assert_eq!(c.expect, "1");
    // ...and it is unsatisfiable, which the corpus test above reports by name.
    assert!(!representable(&c.expect));
    assert!(representable("1\n"));
    assert!(representable(""));
}

#[test]
fn source_bytes_are_verbatim_including_blank_lines_and_indentation() {
    let c = parse_case("--- source\nif true {\n  print 1;\n}\n\n--- expect\n1\n").unwrap();
    assert_eq!(c.source, "if true {\n  print 1;\n}\n\n");
}

#[test]
fn a_source_delimiter_immediately_followed_by_expect_is_an_empty_program() {
    let c = parse_case("--- source\n--- expect\n").unwrap();
    assert_eq!(c.source, "");
    assert_eq!(c.expect, "");
}

#[test]
fn a_file_with_no_expect_delimiter_fails_loudly() {
    let err = parse_case("# c\n--- source\nprint 1;\n").unwrap_err();
    assert!(err.contains(EXPECT_DELIM), "unhelpful error: {err}");
    assert!(err.contains("line 2"), "should name the block: {err}");
}

#[test]
fn a_file_with_no_source_delimiter_fails_loudly() {
    let err = parse_case("# just a comment\nprint 1;\n").unwrap_err();
    assert!(
        err.contains("line 2"),
        "should name the offending line: {err}"
    );
    assert!(err.contains(SOURCE_DELIM), "unhelpful error: {err}");
}

#[test]
fn an_empty_file_fails_loudly() {
    let err = parse_case("").unwrap_err();
    assert!(err.contains(SOURCE_DELIM), "unhelpful error: {err}");
}

#[test]
fn a_near_miss_delimiter_is_not_a_delimiter() {
    // Byte-exact line equality, no trimming: trailing space, leading space and
    // a typo are all just header text, and the file is malformed rather than
    // silently graded against the wrong sections.
    for near in [
        "--- source \n",
        " --- source\n",
        "--- souce\n",
        "---source\n",
    ] {
        let text = format!("{near}print 1;\n--- expect\n1\n");
        assert!(
            parse_case(&text).is_err(),
            "{near:?} must not be accepted as the delimiter"
        );
    }
}
