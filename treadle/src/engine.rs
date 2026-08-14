//! bead: treadle-engine — FROZEN: the Engine trait. §3
//!
//! The one seam the grading harness sees. `tests/conform.rs` and
//! `tests/differential.rs` both drive a `&mut dyn Engine`, so neither knows
//! whether it is running the bytecode VM or the tree-walker — which is the point:
//! a test that cannot tell them apart cannot accidentally be written to suit one.

use crate::output::Output;

/// **FROZEN, §3.** Implemented by both back ends.
///
/// ```text
/// pub trait Engine { fn name(&self) -> &'static str; fn run(&mut self, src: &str) -> Output; }
/// ```
///
/// Contract, all of it from §3/§4:
///
/// * `run` is **infallible** — it returns an [`Output`], never a `Result` and
///   never a panic. Every failure, including deliberately pathological input, is
///   `Output.error`. A panic on any input is a bug in both engines' book.
/// * `run` takes source, not an `Ast`: both engines own the whole pipeline from
///   bytes, so a `Lex` or `Parse` error is an `Output.error` like any other.
/// * Lines are pushed **as they are produced**. A program that fails at line 9
///   returns the output of lines 1..8 *and* the error; buffering and dropping it
///   diverges deliberately (see [`crate::output`]).
/// * `&mut self` so an engine may carry reusable state across runs, but `run`
///   must be **deterministic**: the same source produces the same `Output` on
///   every call, on either engine, on any platform (§2 Determinism). Nothing
///   observable may carry over between two `run` calls.
/// * `name` is the label the harness reports a divergence under (`"vm"`,
///   `"tree"`), and what [`crate::output::diff`] takes as its side labels.
pub trait Engine {
    /// A short, stable name for this engine, used in failure reports.
    fn name(&self) -> &'static str;
    /// Run `src` to completion and return everything it observably did.
    fn run(&mut self, src: &str) -> Output;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trait must be object-safe: the whole harness holds `&mut dyn Engine`,
    /// and losing that (a generic method, a `Self: Sized` bound) would break both
    /// test binaries rather than this file.
    struct Fake(&'static str);
    impl Engine for Fake {
        fn name(&self) -> &'static str {
            self.0
        }
        fn run(&mut self, src: &str) -> Output {
            let mut out = Output::new();
            out.push_line(src);
            out
        }
    }

    #[test]
    fn engine_is_object_safe_and_drivable_as_dyn() {
        let mut fake = Fake("fake");
        let e: &mut dyn Engine = &mut fake;
        assert_eq!(e.name(), "fake");
        assert_eq!(e.run("hi").to_string(), "hi\n");
    }

    #[test]
    fn a_boxed_engine_works_too() {
        // `tests/differential.rs` wants both engines in one collection.
        let mut engines: Vec<Box<dyn Engine>> = vec![Box::new(Fake("a")), Box::new(Fake("b"))];
        let names: Vec<&str> = engines.iter().map(|e| e.name()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(engines[0].run("x"), engines[1].run("x"));
    }
}
