//! Group A: compile to bytecode, then run on a stack VM.
pub mod compiler;
pub mod machine;
pub mod opcode;

use crate::engine::Engine;
use crate::output::Output;

/// Group A as an [`Engine`]: source in, [`Output`] out, the whole pipeline.
///
/// Stateless — `run` builds a fresh [`opcode::Chunk`] every time, which is what
/// §3's determinism clause ("nothing observable may carry over between two `run`
/// calls") asks for. `&mut self` is the trait's, not a need of ours.
#[derive(Debug, Default)]
pub struct Vm;

impl Vm {
    pub fn new() -> Vm {
        Vm
    }
}

impl Engine for Vm {
    fn name(&self) -> &'static str {
        "vm"
    }

    /// A `Lex` or `Parse` error is an `Output.error` like any other, with
    /// **empty lines**: nothing ran (§3/§4). Everything past the front end is
    /// infallible until the machine runs — `compile` raises no errors (§6/`.35`).
    ///
    /// Runs on a thread with [`VM_STACK`], for the same reason group B does: the
    /// front end and the compiler both recurse over the AST, so a deeply nested
    /// program overflows the default stack and **aborts the process**, which §4
    /// forbids for any input.
    ///
    /// This was added after the fact, and how it was missed is the interesting
    /// part. Group B added its thread and reasoned the CLI and fuzzer would
    /// inherit it "without knowing" — true, but only for group B. Group A could
    /// not read that it needed one, by the isolation rule. And the differential
    /// fuzzer could not catch the difference either: an abort produces no
    /// `Output`, so there is nothing for the oracle to compare. 35,200 generated
    /// programs found zero divergences while `print 1 + 1 + …` (20000 terms)
    /// returned `20001` on group B and aborted on group A. See the P1 bead.
    fn run(&mut self, src: &str) -> Output {
        std::thread::scope(|s| {
            match std::thread::Builder::new()
                .stack_size(VM_STACK)
                .spawn_scoped(s, || run_source(src))
            {
                Ok(h) => h.join().unwrap_or_else(|_| {
                    // §4 forbids a panic on any input, so reaching this is a bug
                    // in this engine rather than something the program did.
                    let mut out = Output::new();
                    out.fail(crate::error::TreadleError::internal(0, "the vm panicked"));
                    out
                }),
                // The OS refused a thread; nothing in the program caused that, so
                // run inline rather than blame it.
                Err(_) => run_source(src),
            }
        })
    }
}

/// Matches group B's `EVAL_STACK`. The two engines must fail at the same depth,
/// or their disagreement is a behaviour difference the oracle cannot see.
const VM_STACK: usize = 64 << 20;

fn run_source(src: &str) -> Output {
    match crate::front::parser::parse(src) {
        Ok(program) => machine::run(&compiler::compile(&program)),
        Err(e) => Output::new().finish(Err(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TreadleError;

    #[test]
    fn vm_engine_runs_source_end_to_end() {
        let mut vm = Vm::new();
        assert_eq!(vm.name(), "vm");
        let out = vm.run("let x = 2;\nprint x * 3;\n");
        assert_eq!(out.to_string(), "6\n");

        // Through `&mut dyn Engine`, which is all the harness ever holds.
        let e: &mut dyn Engine = &mut vm;
        assert_eq!(
            e.run("fn f(a) { return a + 1; }\nprint f(1);\n").lines,
            ["2"]
        );
    }

    /// A front-end failure is an `Output` with no lines and that error — not a
    /// panic, and not a separate "compile failed" path (§4, corpus 315/317/320).
    #[test]
    fn a_parse_error_is_an_output_with_no_lines() {
        let mut vm = Vm::new();
        let out = vm.run("print 1;\nlet = 2;\n");
        assert_eq!(
            out.lines,
            Vec::<String>::new(),
            "nothing runs when the parse fails, not even the first line"
        );
        assert!(matches!(
            out.error,
            Some(TreadleError::Parse { line: 2, .. })
        ));

        // A Lex error passes through the same way.
        let out = vm.run("print 1 $ 2;\n");
        assert!(matches!(out.error, Some(TreadleError::Lex { .. })));
        assert_eq!(out.lines, Vec::<String>::new());
    }

    /// The engines must survive the same depths. Before the big-stack thread was
    /// added here, `print 1 + 1 + …` with 20000 terms returned `20001` on group B
    /// and aborted the process on group A — a behaviour difference the
    /// differential fuzzer cannot see, because an abort yields no `Output` to
    /// compare. 20000 is well past the ~2000 that overflowed the default stack
    /// and well inside what group B already handled.
    #[test]
    fn a_deeply_nested_expression_does_not_abort_the_process() {
        let src = format!("print 1{};\n", " + 1".repeat(20_000));
        let out = Vm::new().run(&src);
        assert_eq!(out.to_string(), "20001\n");
        assert!(out.error.is_none());
    }

    /// §3: the same source gives the same `Output` on a reused engine.
    #[test]
    fn run_is_deterministic_across_calls_on_one_engine() {
        let mut vm = Vm::new();
        let src = "let x = 1;\nwhile x < 4 { print x; x = x + 1; }\n";
        assert_eq!(vm.run(src), vm.run(src));
        assert_eq!(vm.run(src).to_string(), "1\n2\n3\n");
    }
}
