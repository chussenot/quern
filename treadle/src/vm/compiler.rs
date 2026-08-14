//! bead: treadle-compiler — Ast -> Chunk
//!
//! Group A's front-to-back seam: an [`ast::Expr`] tree becomes a run of
//! [`Op`]s that leaves **exactly one value on the stack**, plus a line-table
//! entry per instruction. This file is written in two halves by two beads —
//! expressions (`.14`) and statements, control flow and functions (`.15`) — so
//! the contract between them is spelled out here rather than inferred:
//!
//! * [`Compiler::expr`] appends to a `&mut Code` you own and hand in, and never
//!   touches [`Chunk::main`]. A function body compiles into its own `Code`; the
//!   statement half installs it with `chunk.add_function` (or as `chunk.main`).
//!   The `Code` is a parameter rather than a field precisely because
//!   `self.chunk.add_const(..)` and the code being written must be borrowed at
//!   the same time.
//! * Locals are [`Compiler::declare`]d and looked up by [`Compiler::resolve`];
//!   slot `n` is the `n`th declaration still live in the frame, which is exactly
//!   the stack layout `opcode.rs` documents (`stack[base + n]`). The statement
//!   half owns block scoping — how many `Pop`s leaving a block costs, and
//!   whether a `let` is a slot or a `DefineGlobal` — and this half only ever
//!   *reads* the table.
//!
//! # The three §6 pins this half exists to satisfy
//!
//! * **`.35` — no compile-time errors.** [`Compiler::expr`] returns `()`, not a
//!   `Result`: there is no failure to report. A variable that is not a live
//!   local becomes `GetGlobal(name)` and a call becomes `Call { name }`, both
//!   resolved by name at run time, so `print 1; nope();` prints `1` and *then*
//!   fails, and `if false { nope(); }` runs clean. Resolving a name statically
//!   would have turned that into empty output plus a `Name` error and diverged
//!   from the tree-walker.
//! * **`.40` — `and`/`or` short-circuit.** They compile to *jumps*, not to a
//!   binary instruction; `Op::binary` returns `None` for exactly those two,
//!   which is why the dispatch below is a `match` on that `Option` rather than
//!   two special cases someone could forget. See [`Compiler::short_circuit`].
//! * **`.46`/`6b` — error lines.** Every instruction is emitted with the line of
//!   the **innermost** AST node that produced it. `Expr::Lit` carries no line
//!   (it cannot fail), so `expr` takes the enclosing node's line as a fallback
//!   for it — see [`line_of`]. The `AsBool` closing an `and`/`or` gets the
//!   **right operand's** line and the `JumpIfFalse` opening it the **left
//!   operand's**, because §6b pins an `as_bool` failure to the failing operand,
//!   not to the `Binary` node.

use crate::front::ast::{BinOp, Expr};
use crate::value::Value;
use crate::vm::opcode::{Chunk, Code, Op};

/// Compiles an AST into a [`Chunk`]. Infallible by construction (§6/`.35`).
#[derive(Debug, Default)]
pub struct Compiler {
    /// The chunk being built: the shared constant and name pools, the compiled
    /// functions, and `main`.
    pub chunk: Chunk,
    /// Live locals of the frame being compiled, innermost last. The index *is*
    /// the stack slot (`stack[base + n]`), and a function's parameters are the
    /// first declarations, so they land in slots `0..arity` where `Op::Call`
    /// leaves the arguments.
    locals: Vec<String>,
}

impl Compiler {
    pub fn new() -> Compiler {
        Compiler::default()
    }

    /// Declare a local, returning its slot. Re-declaring a name in scope is
    /// legal (§6/`.37`) and [`Compiler::resolve`] picks the later one.
    pub fn declare(&mut self, name: &str) -> u32 {
        self.locals.push(name.to_string());
        (self.locals.len() - 1) as u32
    }

    /// The slot of `name`, or `None` if it is not a live local — in which case
    /// it is a global, *by name*, decided at run time (§6/`.35`).
    ///
    /// Searches from the innermost declaration outward, so the later of two
    /// `let`s with the same name wins.
    pub fn resolve(&self, name: &str) -> Option<u32> {
        self.locals
            .iter()
            .rposition(|n| n == name)
            .map(|i| i as u32)
    }

    /// Forget the last `n` locals — the statement half's `end_scope`, which
    /// also emits one `Pop` per local it drops.
    pub fn drop_locals(&mut self, n: usize) {
        let keep = self.locals.len().saturating_sub(n);
        self.locals.truncate(keep);
    }

    /// How many locals are live in the frame being compiled.
    pub fn local_count(&self) -> usize {
        self.locals.len()
    }

    /// Compile `e`, appending to `code`. **Leaves exactly one value on the
    /// stack**, always — that is the whole post-condition, and it holds on every
    /// path through an `and`/`or`.
    ///
    /// `line` is the fallback for nodes that carry none (`Expr::Lit`); pass the
    /// enclosing node's line.
    pub fn expr(&mut self, code: &mut Code, e: &Expr, line: u32) {
        match e {
            // Infallible, so its line is only ever used by a *following*
            // instruction (the `AsBool` of an `and`/`or`, say) — which is
            // exactly why the fallback has to be the enclosing node's line and
            // not zero.
            Expr::Lit(v) => {
                let k = self.chunk.add_const(v.clone());
                code.emit(Op::Const(k), line);
            }
            Expr::Var { name, line } => {
                let op = match self.resolve(name) {
                    Some(slot) => Op::GetLocal(slot),
                    None => Op::GetGlobal(self.chunk.add_name(name)),
                };
                code.emit(op, *line);
            }
            Expr::Unary { op, rhs, line } => {
                self.expr(code, rhs, *line);
                code.emit(Op::unary(*op), *line);
            }
            // `None` is `Or`/`And` and nothing else: `Op::binary` is total over
            // the other eleven. Routing on the `Option` rather than matching the
            // two names means a new short-circuiting operator cannot be compiled
            // as a strict one by omission.
            Expr::Binary {
                op,
                lhs,
                rhs,
                line: node,
            } => match Op::binary(*op) {
                Some(i) => {
                    // §6/`.33`: lhs completely, side effects included, before
                    // rhs is touched. Bytecode forces it.
                    self.expr(code, lhs, *node);
                    self.expr(code, rhs, *node);
                    code.emit(i, *node);
                }
                None => self.short_circuit(code, *op, lhs, rhs, *node),
            },
            Expr::Call { name, args, line } => {
                for a in args {
                    self.expr(code, a, *line);
                }
                let name = self.chunk.add_name(name);
                code.emit(
                    Op::Call {
                        name,
                        argc: args.len() as u32,
                    },
                    *line,
                );
            }
        }
    }

    /// `and` / `or` as jumps (§2 short-circuit, §6/`.40`):
    ///
    /// ```text
    /// a and b                        a or b
    ///     <a>                            <a>
    ///     JumpIfFalse -> L               JumpIfFalse -> L
    ///     <b>                            Const true
    ///     AsBool                         Jump -> E
    ///     Jump -> E                  L:  <b>
    /// L:  Const false                    AsBool
    /// E:                             E:
    /// ```
    ///
    /// Both arms leave one `Bool`. `JumpIfFalse` pops and `as_bool`s the left
    /// operand on **both** paths, so the left is always type-checked; the
    /// `AsBool` type-checks the right one only on the path that evaluated it.
    /// Hence `false and 1` is `false` while `true and 1` is a `Type` error at
    /// the right operand's line, and a side effect in `b` — a print, a
    /// `1/0` — never happens when `a` already decided.
    ///
    /// The two `as_bool` sites carry their **operand's** line, not `node`'s,
    /// per §6b.
    fn short_circuit(&mut self, code: &mut Code, op: BinOp, lhs: &Expr, rhs: &Expr, node: u32) {
        let ll = line_of(lhs, node);
        let rl = line_of(rhs, node);
        self.expr(code, lhs, ll);
        let take_short = code.emit_jump(Op::JumpIfFalse, ll);

        // The one `Const` either shape needs is the value of the WHOLE
        // expression on the path that skipped the rhs: `false` when `and`'s lhs
        // was false, `true` when `or`'s lhs was true. Which of the two paths
        // that is differs — `and` jumps to it, `or` falls into it.
        let is_and = op == BinOp::And;
        let k = self.chunk.add_const(Value::Bool(!is_and));

        if is_and {
            // and: fall through to the rhs, jump to `Const false`.
            self.expr(code, rhs, rl);
            code.emit(Op::AsBool, rl);
            let end = code.emit_jump(Op::Jump, node);
            code.patch_jump(take_short);
            code.emit(Op::Const(k), node);
            code.patch_jump(end);
        } else {
            // or: fall through to `Const true`, jump to the rhs.
            code.emit(Op::Const(k), node);
            let end = code.emit_jump(Op::Jump, node);
            code.patch_jump(take_short);
            self.expr(code, rhs, rl);
            code.emit(Op::AsBool, rl);
            code.patch_jump(end);
        }
    }
}

/// The line an expression's own failures report, falling back to the enclosing
/// node's line for `Expr::Lit` — the one variant with no line of its own,
/// because a literal cannot fail on its own account.
///
/// Only reachable failure for a `Lit` operand is the `as_bool` gate of an
/// enclosing `and`/`or` (`true and 1`), and §6b's "the failing operand's line"
/// is not available there, so both engines use the enclosing node's.
pub fn line_of(e: &Expr, fallback: u32) -> u32 {
    match e {
        Expr::Lit(_) => fallback,
        Expr::Var { line, .. }
        | Expr::Unary { line, .. }
        | Expr::Binary { line, .. }
        | Expr::Call { line, .. } => *line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TreadleError;
    use crate::front::ast::UnOp;
    use crate::output::Output;
    use crate::vm::machine;

    fn lit(v: Value) -> Expr {
        Expr::Lit(v)
    }

    fn int(n: i64) -> Expr {
        lit(Value::Int(n))
    }

    fn var(name: &str, line: u32) -> Expr {
        Expr::Var {
            name: name.to_string(),
            line,
        }
    }

    fn bin(op: BinOp, lhs: Expr, rhs: Expr, line: u32) -> Expr {
        Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            line,
        }
    }

    /// `1 / 0` — a side effect visible in `Output` without needing a call, so
    /// the short-circuit tests can run on the landed machine (`Op::Call` is
    /// still a stub) and still fail loudly if the right operand is evaluated.
    fn div_by_zero(line: u32) -> Expr {
        bin(BinOp::Div, int(1), int(0), line)
    }

    /// Compile `e` on its own and return the code, with the invariants the
    /// machine relies on checked.
    fn compile(e: &Expr, line: u32) -> (Compiler, Code) {
        let mut c = Compiler::new();
        let mut code = Code::new();
        c.expr(&mut code, e, line);
        // Every instruction has a line-table entry, fallible or not (§6/`.46`).
        for ip in 0..code.len() {
            assert!(code.line(ip).is_some(), "no line for instruction {ip}");
        }
        (c, code)
    }

    fn ops_of(e: &Expr, line: u32) -> Vec<Op> {
        compile(e, line).1.ops().to_vec()
    }

    /// Compile `e` as `print e;` and run it. Asserts the chunk validates first,
    /// so an unpatched jump or a bad pool index fails as a compiler bug here
    /// rather than as an `internal` error in the machine.
    fn run_printed(e: &Expr, line: u32) -> Output {
        let (mut c, mut code) = compile(e, line);
        code.emit(Op::Print(1), line);
        c.chunk.main = code;
        assert_eq!(c.chunk.validate(), Ok(()), "compiler produced a bad chunk");
        machine::run(&c.chunk)
    }

    /// Same, with globals pre-defined by hand (the statement half owns `let`).
    fn run_printed_with(globals: &[(&str, Value)], e: &Expr, line: u32) -> Output {
        let mut c = Compiler::new();
        let mut code = Code::new();
        for (name, v) in globals {
            let k = c.chunk.add_const(v.clone());
            let n = c.chunk.add_name(name);
            code.emit(Op::Const(k), 1);
            code.emit(Op::DefineGlobal(n), 1);
        }
        c.expr(&mut code, e, line);
        code.emit(Op::Print(1), line);
        c.chunk.main = code;
        assert_eq!(c.chunk.validate(), Ok(()));
        machine::run(&c.chunk)
    }

    // ---- opcode sequences ------------------------------------------------

    #[test]
    fn constants_are_interned_and_carry_the_enclosing_line() {
        // `2 * 2` on line 5: one constant slot, three instructions, three
        // line-table entries.
        let (c, code) = compile(&bin(BinOp::Mul, int(2), int(2), 5), 9);
        assert_eq!(code.ops(), [Op::Const(0), Op::Const(0), Op::Mul]);
        assert_eq!(
            (code.line(0), code.line(1), code.line(2)),
            (Some(5), Some(5), Some(5)),
            "a Lit inherits the enclosing node's line, not the fallback"
        );
        assert_eq!(c.chunk.constant(0), Some(&Value::Int(2)));
        assert_eq!(c.chunk.constant(1), None, "equal constants share a slot");
    }

    #[test]
    fn variables_are_local_slots_or_globals_by_name() {
        let mut c = Compiler::new();
        let mut code = Code::new();
        assert_eq!(c.declare("a"), 0);
        assert_eq!(c.declare("b"), 1);
        assert_eq!(c.declare("a"), 2, "re-declaring takes a new slot");

        c.expr(&mut code, &var("a", 1), 1);
        c.expr(&mut code, &var("b", 1), 1);
        c.expr(&mut code, &var("g", 2), 2);

        assert_eq!(
            code.ops(),
            [Op::GetLocal(2), Op::GetLocal(1), Op::GetGlobal(0)],
            "the later `let a` wins (§6/.37); a non-local is a global by name"
        );
        assert_eq!(c.chunk.name(0), Some("g"));
        assert_eq!(code.line(2), Some(2));

        // Scope exit forgets the inner declarations, so `a` resolves outward.
        c.drop_locals(1);
        assert_eq!(c.local_count(), 2);
        assert_eq!(c.resolve("a"), Some(0));
        assert_eq!(c.resolve("nope"), None);
    }

    /// The shape `opcode.rs` documents, pinned by a test rather than by a
    /// comment: `a and b` is a jump, and `b`'s code sits *after* the jump that
    /// can skip it.
    #[test]
    fn and_compiles_to_the_documented_jump_shape() {
        let e = bin(BinOp::And, var("a", 1), var("b", 1), 1);
        assert_eq!(
            ops_of(&e, 1),
            [
                Op::GetGlobal(0),   // <a>
                Op::JumpIfFalse(5), // -> L, popping and as_bool-ing a
                Op::GetGlobal(1),   // <b>
                Op::AsBool,         // b must be Bool, only here
                Op::Jump(6),        // -> E
                Op::Const(0),       // L: false
                                    // E:
            ]
        );
        let (c, _) = compile(&e, 1);
        assert_eq!(c.chunk.constant(0), Some(&Value::Bool(false)));
    }

    #[test]
    fn or_compiles_to_the_documented_jump_shape() {
        let e = bin(BinOp::Or, var("a", 1), var("b", 1), 1);
        assert_eq!(
            ops_of(&e, 1),
            [
                Op::GetGlobal(0),   // <a>
                Op::JumpIfFalse(4), // -> L
                Op::Const(0),       // true
                Op::Jump(6),        // -> E
                Op::GetGlobal(1),   // L: <b>
                Op::AsBool,         // b must be Bool, only here
                                    // E:
            ]
        );
        let (c, _) = compile(&e, 1);
        assert_eq!(c.chunk.constant(0), Some(&Value::Bool(true)));
    }

    #[test]
    fn unary_and_call_shapes() {
        assert_eq!(
            ops_of(
                &Expr::Unary {
                    op: UnOp::Neg,
                    rhs: Box::new(var("x", 4)),
                    line: 4,
                },
                4
            ),
            [Op::GetGlobal(0), Op::Neg]
        );

        // Arguments are pushed left to right, a1 deepest, then one Call naming
        // the callee — never an address (§6/.35).
        let e = Expr::Call {
            name: "f".to_string(),
            args: vec![int(1), int(2)],
            line: 7,
        };
        let (c, code) = compile(&e, 7);
        assert_eq!(
            code.ops(),
            [Op::Const(0), Op::Const(1), Op::Call { name: 0, argc: 2 }]
        );
        assert_eq!(c.chunk.constant(0), Some(&Value::Int(1)));
        assert_eq!(c.chunk.name(0), Some("f"));
        assert_eq!(code.line(2), Some(7), "the Call's line is the error's line");
    }

    /// §6/`.35`: the compiler raises no errors. Neither an unknown variable nor
    /// an unknown callee is a compile-time failure — `expr` has no error
    /// channel at all, and the chunk it produces is valid.
    #[test]
    fn unknown_names_compile_and_fail_only_at_run_time() {
        let (c, code) = compile(&var("nope", 3), 3);
        assert_eq!(code.ops(), [Op::GetGlobal(0)]);
        assert_eq!(c.chunk.validate(), Ok(()));
        assert_eq!(
            c.chunk.find_function("nope"),
            None,
            "resolution is the machine's, at run time"
        );

        let out = run_printed(&var("nope", 3), 3);
        assert_eq!(out.lines, Vec::<String>::new());
        assert_eq!(out.error, Some(TreadleError::undefined_name(3, "nope")));
    }

    // ---- executed ---------------------------------------------------------

    #[test]
    fn arithmetic_runs_and_the_stack_is_left_with_one_value() {
        // (1 + 2) * 3 — grouping is the parser's, so this is the tree as given.
        let e = bin(BinOp::Mul, bin(BinOp::Add, int(1), int(2), 1), int(3), 1);
        let out = run_printed(&e, 1);
        assert_eq!(out.lines, ["9"]);
        assert_eq!(out.error, None);
    }

    /// The divergence this bead exists to prevent: `false and <side effect>`
    /// must not evaluate the right operand. Corpus `112`–`121` assert it with a
    /// printing function; `1 / 0` is the same observation without needing
    /// `Op::Call`.
    #[test]
    fn and_does_not_evaluate_its_right_operand_when_the_left_is_false() {
        let out = run_printed(
            &bin(BinOp::And, lit(Value::Bool(false)), div_by_zero(2), 1),
            1,
        );
        assert_eq!(out.lines, ["false"]);
        assert_eq!(out.error, None, "the rhs must not have been evaluated");

        // …and does evaluate it when the left does not decide.
        let out = run_printed(
            &bin(BinOp::And, lit(Value::Bool(true)), div_by_zero(2), 1),
            1,
        );
        assert_eq!(out.lines, Vec::<String>::new());
        assert_eq!(out.error, Some(TreadleError::divide_by_zero(2)));
    }

    #[test]
    fn or_does_not_evaluate_its_right_operand_when_the_left_is_true() {
        let out = run_printed(
            &bin(BinOp::Or, lit(Value::Bool(true)), div_by_zero(2), 1),
            1,
        );
        assert_eq!(out.lines, ["true"]);
        assert_eq!(out.error, None);

        let out = run_printed(
            &bin(BinOp::Or, lit(Value::Bool(false)), div_by_zero(2), 1),
            1,
        );
        assert_eq!(out.lines, Vec::<String>::new());
        assert_eq!(out.error, Some(TreadleError::divide_by_zero(2)));
    }

    /// §6/`.40`: the left operand is always type-checked, the right only when
    /// evaluated, and the result is always a `Bool`.
    #[test]
    fn short_circuit_typing() {
        // `false and 1` is false: the rhs is never type-checked.
        let out = run_printed(&bin(BinOp::And, lit(Value::Bool(false)), int(1), 1), 1);
        assert_eq!(out.lines, ["false"]);
        assert_eq!(out.error, None);

        // `true or nil` is true.
        let out = run_printed(
            &bin(BinOp::Or, lit(Value::Bool(true)), lit(Value::Nil), 1),
            1,
        );
        assert_eq!(out.lines, ["true"]);

        // `true and 1` is a Type error — the AsBool on the taken path.
        let out = run_printed(&bin(BinOp::And, lit(Value::Bool(true)), int(1), 1), 1);
        assert_eq!(out.error, Some(TreadleError::not_bool(1, "Int")));

        // A non-Bool left operand fails through JumpIfFalse's own as_bool,
        // with the same message (§6a) — on both operators.
        let out = run_printed(&bin(BinOp::And, int(1), lit(Value::Bool(true)), 1), 1);
        assert_eq!(out.error, Some(TreadleError::not_bool(1, "Int")));
        let out = run_printed(&bin(BinOp::Or, int(1), lit(Value::Bool(true)), 1), 1);
        assert_eq!(out.error, Some(TreadleError::not_bool(1, "Int")));
    }

    /// §6b: an `as_bool` failure reports the line of the **failing operand**,
    /// not the `Binary` node's. Only observable when an operator is split
    /// across lines, which is exactly why it needed pinning.
    ///
    /// ```text
    /// 1  print a
    /// 2      and
    /// 3      b;
    /// ```
    #[test]
    fn as_bool_reports_the_failing_operands_line() {
        let e = bin(BinOp::And, var("a", 1), var("b", 3), 2);

        // The left operand fails: line 1, where `a` is.
        let out = run_printed_with(&[("a", Value::Int(7)), ("b", Value::Bool(true))], &e, 2);
        assert_eq!(out.error, Some(TreadleError::not_bool(1, "Int")));

        // The right operand fails: line 3, where `b` is.
        let out = run_printed_with(&[("a", Value::Bool(true)), ("b", Value::Int(7))], &e, 2);
        assert_eq!(out.error, Some(TreadleError::not_bool(3, "Int")));

        // The instructions that carry those lines are the two as_bool sites.
        let (_, code) = compile(&e, 2);
        assert_eq!(
            (code.op(1), code.line(1)),
            (Some(Op::JumpIfFalse(5)), Some(1))
        );
        assert_eq!((code.op(3), code.line(3)), (Some(Op::AsBool), Some(3)));
    }

    /// §6/`.33` and `.35`: arguments are evaluated left to right *before* the
    /// callee is resolved, so `nope(1/0)` is `divide by zero` and not
    /// `undefined function`. Bytecode forces the order; this pins it.
    #[test]
    fn arguments_are_evaluated_before_the_callee_is_resolved() {
        let e = Expr::Call {
            name: "nope".to_string(),
            args: vec![div_by_zero(4)],
            line: 4,
        };
        let out = run_printed(&e, 4);
        assert_eq!(out.error, Some(TreadleError::divide_by_zero(4)));
    }

    /// The `Unary`/`Binary` line is the innermost failing node's, so a nested
    /// failure reports its own line and not the outer expression's (§6/`.46`).
    #[test]
    fn the_innermost_node_owns_the_error_line() {
        // `-(1 / 0)` with the division on line 8 inside a unary on line 2.
        let e = Expr::Unary {
            op: UnOp::Neg,
            rhs: Box::new(div_by_zero(8)),
            line: 2,
        };
        let out = run_printed(&e, 2);
        assert_eq!(out.error, Some(TreadleError::divide_by_zero(8)));

        // And the unary's own failure reports the unary's line.
        let e = Expr::Unary {
            op: UnOp::Neg,
            rhs: Box::new(lit(Value::Bool(true))),
            line: 2,
        };
        let out = run_printed(&e, 2);
        assert_eq!(
            out.error,
            Some(TreadleError::unary_type_mismatch(2, "-", "Int", "Bool"))
        );
    }

    /// Every `BinOp` compiles: eleven to one instruction, `and`/`or` to jumps,
    /// and none of them to nothing — a `Binary` that emitted no instruction
    /// would leave two values on the stack and drift silently.
    #[test]
    fn every_binop_compiles_and_leaves_one_value() {
        use BinOp::*;
        for op in [Or, And, Eq, Ne, Lt, Gt, Le, Ge, Add, Sub, Mul, Div, Rem] {
            let e = bin(op, var("a", 1), var("b", 1), 1);
            let (c, code) = compile(&e, 1);
            let strict = Op::binary(op).is_some();
            assert_eq!(
                code.len(),
                if strict { 3 } else { 6 },
                "{op:?} compiled to {} instructions",
                code.len()
            );
            assert_eq!(c.chunk.validate(), Ok(()), "{op:?} produced a bad chunk");
        }
    }
}
