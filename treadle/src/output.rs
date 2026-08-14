//! bead: treadle-output — FROZEN: Output, the observable both engines must match. §3
//!
//! Everything a treadle program can do is in an [`Output`]: the `print` lines it
//! produced, in order, and at most one error that stopped it. That is what makes
//! "the two engines agree" a *total* statement about behaviour rather than a
//! statement about the cases somebody remembered to check.
//!
//! # A failing program keeps its output
//!
//! A program that fails at line 9 has the lines printed by 1..8 **and** the
//! error. An engine that buffers its output and drops it when an error occurs
//! therefore diverges from one that does not, immediately and deliberately —
//! `{lines: ["1"], error: Some(divide by zero)}` is not equal to
//! `{lines: [], error: Some(divide by zero)}`, and the fuzzer will say so on the
//! first generated program that prints before it fails. Push each line as you
//! produce it; do not accumulate and flush at the end.
//!
//! # Comparison goes through `Display`, and only through `Display`
//!
//! `tests/conform.rs` and `tests/differential.rs` both compare through
//! [`Display for Output`](Output#impl-Display-for-Output) and nothing else. §5:
//! the assertion is a **byte comparison of a canonical rendering** — never a
//! comparison made by splitting either side into lines. The rendering is every
//! print line newline-**terminated** (not separated), then the error's display
//! form, also newline-terminated.
//!
//! "Terminated, not separated" is the single load-bearing detail in this file
//! and §5 says so, because it is what makes
//!
//! | `lines`  | rendering |
//! |----------|-----------|
//! | `[]`     | `""`      |
//! | `[""]`   | `"\n"`    |
//!
//! two distinguishable things. Get it backwards — join with `"\n"` — and the
//! conformance runner cannot tell "printed nothing" from "printed one empty
//! line", and it also fails a *correct* engine on `print "a\nb";`, which is one
//! line whose bytes happen to contain a newline. Nothing here classifies a line,
//! which is exactly why an empty printed line, an embedded `\n`, and a printed
//! line that itself begins with `error: ` all need no special rule.

use std::fmt;

use crate::error::TreadleError;
use crate::value::Value;

/// The separator between the arguments of one `print` (§2: "several values,
/// tab-separated, one line"). It lives here rather than in each engine so the
/// two cannot drift on it — same reason `error::MAX_DEPTH` lives in one place.
const PRINT_SEP: char = '\t';

/// **The observable. FROZEN, §3** — the two public fields below are verbatim
/// from the spec; the derives and the inherent helpers are additive.
///
/// `PartialEq` is the fuzzer's oracle: it asserts two `Output`s are equal
/// directly, so the derive is load-bearing and not convenience.
///
/// **Do not "fix" this to use `Value::eq_value`.** `Value`'s `==` *operator*
/// semantics — §2's rule that comparing two different types is a `Type` error
/// rather than `false` — are deliberately distinct from the derived `PartialEq`
/// used here. Derived equality is the Rust-side structural one, it answers
/// `false` across types instead of erroring, and it is the one the fuzzer wants:
/// an oracle that could itself raise a language-level error would have no way to
/// report a divergence. Two `Output`s are equal iff their bytes are equal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// Every `print` line, in order. One `print` statement contributes exactly
    /// one element (§6/`.33`) — see [`Output::print`].
    pub lines: Vec<String>,
    /// The one error that stopped the program, if any.
    pub error: Option<TreadleError>,
}

impl Output {
    /// An empty output: nothing printed, no error. Renders as `""`.
    pub fn new() -> Output {
        Output::default()
    }

    /// Append **exactly one** finished line.
    ///
    /// There is deliberately no incremental-write API here — no `push_str`, no
    /// `fmt::Write`, nothing an engine could use to emit half a line and then
    /// fail. §6/`.33` pins that a `print` appends exactly one element and only
    /// once *every* argument evaluated successfully, so `print "a", 1/0;` is
    /// `{lines: [], error: divide by zero}` with **no partial line**. The way to
    /// obey that rule is to not be able to express its violation: evaluate all
    /// the arguments first, then call this once with the result.
    ///
    /// The string is stored as given. It may be empty, and it may contain
    /// newlines; the rendering keeps both distinguishable.
    pub fn push_line(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    /// Append the one line a `print` of already-evaluated `args` produces:
    /// their §3 display forms joined by a tab (§2).
    ///
    /// Take the `&[Value]` route rather than joining in the engine — the
    /// separator is a spec fact, not an implementation choice, and two engines
    /// that each pick their own are two engines that disagree on `print 1, 2;`.
    /// Taking a *slice* is also the point at which "all arguments evaluated
    /// successfully" is already true: you cannot call this holding a `Result`.
    pub fn print(&mut self, args: &[Value]) {
        let mut line = String::new();
        for (i, v) in args.iter().enumerate() {
            if i > 0 {
                line.push(PRINT_SEP);
            }
            // `Display for Value` is the one display form (§3): `print`, `str()`
            // and every error that names a value all go through it.
            line.push_str(&v.to_string());
        }
        self.lines.push(line);
    }

    /// Record the error that stopped the program.
    ///
    /// The **first** error wins: a program has at most one, and once it is set
    /// execution has stopped, so a later call is a bug in the caller rather than
    /// a reason to overwrite history. Silently keeping the first is the safer
    /// half of that — the alternative loses the error the fuzzer needs to see.
    pub fn fail(&mut self, err: TreadleError) {
        if self.error.is_none() {
            self.error = Some(err);
        }
    }

    /// Fold a run's terminating `Result` into this output and return it — the
    /// natural last line of an `Engine::run` implementation:
    ///
    /// ```
    /// # use treadle::error::Result;
    /// # use treadle::output::Output;
    /// # fn body(out: &mut Output) -> Result<()> { out.push_line("1"); Ok(()) }
    /// let mut out = Output::new();
    /// let r = body(&mut out);          // pushes lines as it goes
    /// let out = out.finish(r);         // Err becomes Output.error
    /// assert_eq!(out.to_string(), "1\n");
    /// ```
    ///
    /// Note the shape: `body` writes into `out` *as it runs*, so the lines
    /// printed before the failure are already there when the `Err` arrives.
    pub fn finish(mut self, result: crate::error::Result<()>) -> Output {
        if let Err(e) = result {
            self.fail(e);
        }
        self
    }

    /// `true` if an error stopped the program.
    pub fn failed(&self) -> bool {
        self.error.is_some()
    }
}

/// **The canonical rendering — the only thing anything compares.** §5.
///
/// Every print line newline-**terminated**, then the error's display form, also
/// newline-terminated. `Display for TreadleError` emits no trailing newline; the
/// one after the error is added here.
impl fmt::Display for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for line in &self.lines {
            // Terminated, not separated. `writeln!` per line is the whole rule.
            writeln!(f, "{line}")?;
        }
        if let Some(e) = &self.error {
            writeln!(f, "{e}")?;
        }
        Ok(())
    }
}

/// Describe how two `Output`s differ, readably, for the differential fuzzer and
/// the conformance runner.
///
/// Not a `Debug` dump: at 3am the useful facts are *where* the two renderings
/// first differ and what each side has there, so that is what comes first. The
/// labels name the sides — `Engine::name()` for the fuzzer
/// (`diff("vm", &a, "tree", &b)`), `"expect"` / `"actual"` for the corpus runner.
///
/// Every chunk is shown with `escape_debug`, so a difference that is invisible
/// in raw text — a trailing space, an embedded `\n`, a missing final newline —
/// is visible in the report. Callers only call this when the two differ; if they
/// do not, it says so rather than lying.
pub fn diff(left_label: &str, left: &Output, right_label: &str, right: &Output) -> String {
    use fmt::Write as _;

    let (ls, rs) = (left.to_string(), right.to_string());
    let mut s = String::new();
    let _ = writeln!(s, "divergence: {left_label} vs {right_label}");

    if ls == rs {
        let _ = writeln!(s, "  (no difference: both render {} bytes)", ls.len());
        return s;
    }

    // First differing byte of the canonical rendering — the comparison §5
    // actually performs, so this is the offset that made the test fail.
    let at = ls
        .bytes()
        .zip(rs.bytes())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| ls.len().min(rs.len()));
    let _ = writeln!(s, "  first differing byte: {at}");
    for (label, bytes) in [(left_label, ls.as_bytes()), (right_label, rs.as_bytes())] {
        match bytes.get(at) {
            Some(b) => {
                let _ = writeln!(
                    s,
                    "    {label}: byte {at} = 0x{b:02x} {:?} ({} bytes total)",
                    (*b as char).escape_debug().to_string(),
                    bytes.len()
                );
            }
            // One rendering is a prefix of the other: the shorter one ended
            // here. That is the `[]` vs `[""]` case, among others.
            None => {
                let _ = writeln!(s, "    {label}: ended ({} bytes total)", bytes.len());
            }
        }
    }

    for (label, out, rendered) in [(left_label, left, &ls), (right_label, right, &rs)] {
        let _ = writeln!(
            s,
            "  {label}: {} line(s), error {}, {} rendered bytes",
            out.lines.len(),
            match &out.error {
                Some(e) => e.to_string(),
                None => "none".to_string(),
            },
            rendered.len()
        );
        if rendered.is_empty() {
            let _ = writeln!(s, "    <empty rendering>");
        }
        // `split_inclusive` keeps each terminator with its chunk, so an
        // unterminated final chunk shows up as one — which is a bug in whatever
        // produced it, and worth seeing rather than smoothing over.
        for (i, chunk) in rendered.split_inclusive('\n').enumerate() {
            let _ = writeln!(s, "    [{i}] \"{}\"", chunk.escape_debug());
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TreadleError;

    // ---- the rendering, which is the contract -----------------------------

    #[test]
    fn empty_output_renders_empty() {
        assert_eq!(Output::new().to_string(), "");
    }

    #[test]
    fn one_empty_printed_line_renders_one_newline() {
        // The whole reason §5 says "terminated, not separated": this must be
        // distinguishable from the case above.
        let mut out = Output::new();
        out.push_line("");
        assert_eq!(out.to_string(), "\n");
        assert_ne!(out.to_string(), Output::new().to_string());
    }

    #[test]
    fn printed_string_containing_a_newline_round_trips() {
        // ONE line whose bytes contain a newline. A line-splitting comparison
        // would fail a correct engine here.
        let mut out = Output::new();
        out.push_line("a\nb");
        assert_eq!(out.to_string(), "a\nb\n");
        assert_eq!(out.lines.len(), 1);
    }

    #[test]
    fn output_then_error_renders_both_in_order() {
        let mut out = Output::new();
        out.push_line("1");
        out.fail(TreadleError::Value {
            line: 3,
            msg: "divide by zero".to_string(),
        });
        assert_eq!(
            out.to_string(),
            "1\nerror: Value at line 3: divide by zero\n"
        );
    }

    #[test]
    fn error_alone_renders_just_the_error_line() {
        let mut out = Output::new();
        out.fail(TreadleError::Parse {
            line: 1,
            msg: "unexpected ';'".to_string(),
        });
        assert_eq!(out.to_string(), "error: Parse at line 1: unexpected ';'\n");
    }

    #[test]
    fn a_printed_error_line_is_not_a_real_error() {
        // Nothing in the rendering classifies a line, so a program printing
        // something that looks like an error is representable and is not
        // confused with `Output.error`.
        let mut out = Output::new();
        out.push_line("error: Value at line 3: divide by zero");
        assert_eq!(out.to_string(), "error: Value at line 3: divide by zero\n");
        assert!(!out.failed());

        let mut real = Output::new();
        real.fail(TreadleError::Value {
            line: 3,
            msg: "divide by zero".to_string(),
        });
        // Byte-identical renderings, different Outputs. Deliberate: the corpus
        // compares bytes, and a `.tr` case cannot tell these apart — which is
        // fine, because no *engine* can produce the second from a program that
        // could produce the first.
        assert_eq!(real.to_string(), out.to_string());
        assert_ne!(real, out);
    }

    #[test]
    fn many_lines_are_terminated_not_separated() {
        let mut out = Output::new();
        out.push_line("a");
        out.push_line("b");
        assert_eq!(out.to_string(), "a\nb\n");
    }

    // ---- helpers ----------------------------------------------------------

    #[test]
    fn print_joins_arguments_with_a_tab_into_one_line() {
        let mut out = Output::new();
        out.print(&[Value::str("a"), Value::Int(1), Value::Bool(true)]);
        assert_eq!(out.lines, vec!["a\t1\ttrue".to_string()]);
        assert_eq!(out.to_string(), "a\t1\ttrue\n");
    }

    #[test]
    fn print_of_one_argument_has_no_separator() {
        let mut out = Output::new();
        out.print(&[Value::Nil]);
        assert_eq!(out.to_string(), "nil\n");
    }

    #[test]
    fn the_first_error_wins() {
        let mut out = Output::new();
        out.fail(TreadleError::Value {
            line: 1,
            msg: "first".to_string(),
        });
        out.fail(TreadleError::Value {
            line: 2,
            msg: "second".to_string(),
        });
        assert_eq!(out.error.as_ref().map(|e| e.msg()), Some("first"));
    }

    #[test]
    fn finish_folds_an_err_and_keeps_the_lines_printed_before_it() {
        let mut out = Output::new();
        out.push_line("1");
        let out = out.finish(Err(TreadleError::Value {
            line: 9,
            msg: "divide by zero".to_string(),
        }));
        // The §3 promise: failing at line 9 keeps the output of lines 1..8.
        assert_eq!(out.lines, vec!["1".to_string()]);
        assert!(out.failed());
    }

    #[test]
    fn finish_on_ok_changes_nothing() {
        let out = Output::new().finish(Ok(()));
        assert_eq!(out, Output::new());
    }

    // ---- equality and the diff helper ------------------------------------

    #[test]
    fn dropping_output_on_error_is_a_divergence() {
        let err = TreadleError::Value {
            line: 2,
            msg: "divide by zero".to_string(),
        };
        let mut kept = Output::new();
        kept.push_line("1");
        kept.fail(err.clone());
        let dropped = Output {
            lines: vec![],
            error: Some(err),
        };
        assert_ne!(kept, dropped);
        assert_ne!(kept.to_string(), dropped.to_string());
    }

    #[test]
    fn diff_names_the_first_differing_byte_and_both_sides() {
        let mut a = Output::new();
        a.push_line("1");
        let mut b = Output::new();
        b.push_line("2");
        let d = diff("vm", &a, "tree", &b);
        assert!(d.starts_with("divergence: vm vs tree\n"), "{d}");
        assert!(d.contains("first differing byte: 0"), "{d}");
        assert!(d.contains("[0] \"1\\n\""), "{d}");
        assert!(d.contains("[0] \"2\\n\""), "{d}");
    }

    #[test]
    fn diff_reports_a_prefix_as_one_side_ending() {
        // The `[]` vs `[""]` pair, which is the pair most likely to be gotten
        // wrong, and the one a naive `zip` comparison finds no difference in.
        let empty = Output::new();
        let mut blank = Output::new();
        blank.push_line("");
        let d = diff("expect", &empty, "actual", &blank);
        assert!(d.contains("first differing byte: 0"), "{d}");
        assert!(d.contains("expect: ended (0 bytes total)"), "{d}");
        assert!(d.contains("<empty rendering>"), "{d}");
        assert!(d.contains("actual: byte 0 = 0x0a"), "{d}");
    }

    #[test]
    fn diff_of_equal_outputs_says_so() {
        let mut a = Output::new();
        a.push_line("x");
        assert!(diff("vm", &a, "tree", &a.clone()).contains("no difference"));
    }
}
