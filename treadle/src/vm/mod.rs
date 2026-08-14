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
    fn run(&mut self, src: &str) -> Output {
        match crate::front::parser::parse(src) {
            Ok(program) => machine::run(&compiler::compile(&program)),
            Err(e) => Output::new().finish(Err(e)),
        }
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

    /// §3: the same source gives the same `Output` on a reused engine.
    #[test]
    fn run_is_deterministic_across_calls_on_one_engine() {
        let mut vm = Vm::new();
        let src = "let x = 1;\nwhile x < 4 { print x; x = x + 1; }\n";
        assert_eq!(vm.run(src), vm.run(src));
        assert_eq!(vm.run(src).to_string(), "1\n2\n3\n");
    }
}
