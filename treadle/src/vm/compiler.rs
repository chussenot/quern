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

use crate::front::ast::{BinOp, Expr, FnDecl, Program, Stmt};
use crate::value::Value;
use crate::vm::opcode::{Chunk, Code, Function, Op};

/// **The entry point of group A's front-to-back seam**: a whole [`Program`]
/// becomes a [`Chunk`] the machine can run. Infallible (§6/`.35`).
///
/// Functions are defined from [`Program::fns`] — the complete hoisted list —
/// **before** `main` is compiled, and `Stmt::Fn` is a no-op when walking a
/// statement list. Doing both would define every function twice; doing neither
/// would never define one (see `ast.rs`). Hoisting is what makes a call to a
/// function declared later in the file work, and a `fn` inside a branch that
/// never runs still callable.
pub fn compile(program: &Program) -> Chunk {
    let mut c = Compiler::new();
    for decl in &program.fns {
        c.function(decl);
    }
    let mut code = Code::new();
    c.stmts(&mut code, &program.stmts);
    c.chunk.main = code;
    c.chunk
}

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
    /// How many blocks deep in the frame being compiled. `0` is the frame's own
    /// top level, and at `0` in `main` — and only there — a `let` is a global
    /// (§6/`.37`: top-level scope *is* global scope).
    scope_depth: u32,
    /// Whether the frame being compiled is a function body rather than `main`.
    /// A function body cannot create a global (§6/`.37`), so its top level is a
    /// slot even at depth `0`.
    in_function: bool,
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

    /// Compile a function body into its own [`Code`] and install it in the
    /// chunk. The parameters are the callee's **first locals**, in order, which
    /// is exactly where `Op::Call` leaves the arguments (`stack[base + n]`), so
    /// nothing is moved on entry.
    ///
    /// Every body ends in `Const(nil); Return`, so a function with no `return`
    /// returns `Nil` (§2) and the machine can never run off the end of a frame.
    /// The body's own locals need no `Pop`: `Return` truncates the stack to
    /// `frame.base`.
    fn function(&mut self, decl: &FnDecl) {
        let mut code = Code::new();
        self.in_function = true;
        self.scope_depth = 0;
        for p in &decl.params {
            self.declare(p);
        }
        self.stmts(&mut code, &decl.body);
        let nil = self.chunk.add_const(Value::Nil);
        code.emit(Op::Const(nil), decl.line);
        code.emit(Op::Return, decl.line);
        // The next body starts at slot 0.
        self.drop_locals(self.local_count());
        self.in_function = false;
        self.chunk.add_function(Function {
            name: decl.name.clone(),
            arity: decl.params.len() as u32,
            code,
        });
    }

    /// Compile a statement list **in the current scope** — a frame's top level.
    /// A nested block goes through [`Compiler::block`] instead.
    pub fn stmts(&mut self, code: &mut Code, stmts: &[Stmt]) {
        for s in stmts {
            self.stmt(code, s);
        }
    }

    /// Compile a braced block: a new scope (§2), and one `Pop` per local it
    /// declared on the way out.
    ///
    /// Missing those `Pop`s would not fail loudly — later `base + n` slots still
    /// resolve, the stack just grows — so the count comes from the locals table
    /// rather than from counting `Stmt::Let`s, which would be wrong for a `let`
    /// in a nested block.
    fn block(&mut self, code: &mut Code, stmts: &[Stmt], line: u32) {
        let outer = self.local_count();
        self.scope_depth += 1;
        self.stmts(code, stmts);
        self.scope_depth -= 1;
        let n = self.local_count() - outer;
        for _ in 0..n {
            code.emit(Op::Pop, line);
        }
        self.drop_locals(n);
    }

    /// One statement. Leaves the stack **exactly as it found it**, except for a
    /// `let` that declares a local, whose initialiser's value *is* the slot.
    fn stmt(&mut self, code: &mut Code, s: &Stmt) {
        match s {
            // §6/`.37`: the initialiser is compiled FIRST and the slot declared
            // only after, so it is evaluated in the scope as it exists *before*
            // the new binding — `let x = x + 1;` reads the outer `x`. There is
            // no `DeclareLocal` op precisely so this holds by construction.
            Stmt::Let { name, init, line } => {
                self.expr(code, init, *line);
                if self.scope_depth == 0 && !self.in_function {
                    // Top-level scope IS global scope; only `main` can define
                    // one, and `DefineGlobal` is create-or-replace, so a
                    // re-`let` at the top level replaces (later wins).
                    let n = self.chunk.add_name(name);
                    code.emit(Op::DefineGlobal(n), *line);
                } else {
                    // The value the initialiser left on the stack is the slot.
                    self.declare(name);
                }
            }
            // Assignment never creates a binding: a name that is not a live
            // local is a global *by name*, and `SetGlobal` is `assign_unbound`
            // at run time if it does not exist. In a function body that is §2's
            // "walk outward to the nearest existing binding", the body's parent
            // being the global scope.
            Stmt::Assign { name, value, line } => {
                self.expr(code, value, *line);
                let op = match self.resolve(name) {
                    Some(slot) => Op::SetLocal(slot),
                    None => Op::SetGlobal(self.chunk.add_name(name)),
                };
                code.emit(op, *line);
            }
            // §6/`.33`: arguments left to right, then ONE `Print` appending
            // exactly one line — and only if every argument evaluated, which is
            // automatic here because a failing argument never reaches `Print`.
            Stmt::Print { args, line } => {
                for a in args {
                    self.expr(code, a, *line);
                }
                code.emit(Op::Print(args.len() as u32), *line);
            }
            // Forward jumps, back-patched. The condition's `JumpIfFalse` carries
            // the CONDITION's line, not the `if`'s, so a condition on its own
            // line reports itself (§6/`.46`, §6b) — and it pops the condition on
            // both paths, so neither arm needs a `Pop`.
            Stmt::If {
                cond,
                then,
                els,
                line,
            } => {
                let cl = line_of(cond, *line);
                self.expr(code, cond, cl);
                let to_else = code.emit_jump(Op::JumpIfFalse, cl);
                self.block(code, then, *line);
                if els.is_empty() {
                    // An else-less `if` needs no jump over an empty else arm.
                    code.patch_jump(to_else);
                } else {
                    let to_end = code.emit_jump(Op::Jump, *line);
                    code.patch_jump(to_else);
                    self.block(code, els, *line);
                    code.patch_jump(to_end);
                }
            }
            // The condition is re-evaluated every iteration, so the loop jumps
            // BACKWARD to it — `emit_jump_to` with an already-known target,
            // while the exit is a forward jump patched after the body.
            Stmt::While { cond, body, line } => {
                let top = code.len();
                let cl = line_of(cond, *line);
                self.expr(code, cond, cl);
                let to_end = code.emit_jump(Op::JumpIfFalse, cl);
                self.block(code, body, *line);
                code.emit_jump_to(Op::Jump, top, *line);
                code.patch_jump(to_end);
            }
            // A bare `return;` is `Const(nil); Return`, so `Return` always finds
            // its value. No `Pop`s for the scopes being left: `Return` truncates
            // the stack to `frame.base`.
            Stmt::Return { value, line } => {
                match value {
                    Some(e) => self.expr(code, e, *line),
                    None => {
                        let nil = self.chunk.add_const(Value::Nil);
                        code.emit(Op::Const(nil), *line);
                    }
                }
                code.emit(Op::Return, *line);
            }
            // A no-op, emitting NOTHING: the declaration was already compiled
            // from `Program::fns` (see [`compile`]). Compiling it here as well
            // would define it twice.
            Stmt::Fn(_) => {}
        }
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

    // ---- statements, control flow and functions (`.15`) -------------------

    /// Compile a whole program from source. The chunk is `validate`d, so an
    /// unpatched jump, a stray pool index or a duplicate function name fails
    /// here as the compiler bug it is rather than as an `internal` error from
    /// the middle of the machine.
    fn chunk_of(src: &str) -> Chunk {
        let program = crate::front::parser::parse(src).expect("test source must parse");
        // `super::` because the expression tests' own `compile` helper (one
        // `Expr`, not a `Program`) shadows the free function here.
        let chunk = super::compile(&program);
        assert_eq!(chunk.validate(), Ok(()), "compiler produced a bad chunk");
        chunk
    }

    fn run_src(src: &str) -> Output {
        machine::run(&chunk_of(src))
    }

    /// §6/`.37`, the pinned edge this half exists for: the initialiser is
    /// evaluated in the scope **as it exists before** the new binding, so the
    /// `x` in `let x = x + 1;` is the *outer* one. Both forms — a top-level
    /// `let` (a global) and one in a block (a slot).
    #[test]
    fn let_evaluates_its_initialiser_before_the_binding_exists() {
        // Global form: `DefineGlobal` is create-or-replace, and the `GetGlobal`
        // is emitted before it, so it reads the old value.
        let out = run_src("let x = 1;\nlet x = x + 1;\nprint x;\n");
        assert_eq!(out.lines, ["2"]);
        assert_eq!(out.error, None);

        // Local form, in a block: the inner `let` gets a NEW slot and the
        // initialiser resolves to the outer binding — and the outer one is
        // unchanged after the block.
        let out = run_src("let x = 1;\nif true { let x = x + 1; print x; }\nprint x;\n");
        assert_eq!(out.lines, ["2", "1"]);

        // Local form inside a function, twice over: a slot shadowing a slot.
        // (A bare `{ … }` is not a statement in this grammar — the only blocks
        // are `if`/`while`/`fn` bodies — so the inner scope is an `if true`.)
        let out = run_src(
            "fn f() { let x = 1; if true { let x = x + 1; print x; } print x; }\nprint f();\n",
        );
        assert_eq!(out.lines, ["2", "1", "nil"]);

        // And the emission order is the point: initialiser, THEN the binding.
        let chunk = chunk_of("let x = 1;\n");
        assert_eq!(chunk.main.ops(), [Op::Const(0), Op::DefineGlobal(0)]);
    }

    /// Blocks are scopes: a shadowing `let` takes a new slot, and leaving the
    /// block costs one `Pop` per local declared in it. Missing those `Pop`s
    /// drifts silently rather than failing, so the shape is pinned here.
    #[test]
    fn blocks_shadow_by_slot_and_pop_one_per_local_on_exit() {
        let chunk = chunk_of(
            "fn f() { let a = 1; if true { let a = 2; let b = 3; print a, b; } print a; }\n",
        );
        let f = chunk.function(0).expect("f is compiled");
        assert_eq!(
            f.code.ops(),
            [
                Op::Const(0),        // let a = 1     -> slot 0
                Op::Const(1),        // true
                Op::JumpIfFalse(10), // past the block AND its Pops
                Op::Const(2),        // let a = 2     -> slot 1 (shadows)
                Op::Const(3),        // let b = 3     -> slot 2
                Op::GetLocal(1),     // print a, b    -> the INNER a
                Op::GetLocal(2),
                Op::Print(2),
                Op::Pop, // leaving the block: two locals, two Pops
                Op::Pop,
                Op::GetLocal(0), // print a        -> the outer a again
                Op::Print(1),
                Op::Const(4), // the implicit `return nil`
                Op::Return,
            ]
        );
        assert_eq!(
            run_src("fn f() { let a = 1; if true { let a = 2; print a; } print a; }\nprint f();\n")
                .lines,
            ["2", "1", "nil"]
        );
    }

    /// §6/`.37`: re-declaring in the *same* scope is legal and the later `let`
    /// wins — by slot, with the earlier one still occupying its own.
    #[test]
    fn re_declaration_in_one_scope_lets_the_later_let_win() {
        let chunk = chunk_of("fn f() { let a = 1; let a = 2; print a; }\n");
        let f = chunk.function(0).expect("f is compiled");
        assert_eq!(
            f.code.ops(),
            [
                Op::Const(0),
                Op::Const(1),
                Op::GetLocal(1), // the later slot, not slot 0
                Op::Print(1),
                Op::Const(2),
                Op::Return,
            ]
        );
        assert_eq!(
            run_src("fn f() { let a = 1; let a = 2; print a; }\nprint f();\n").lines,
            ["2", "nil"]
        );
    }

    /// §2 hoisting: functions are defined from `Program::fns` before anything
    /// runs, so one declared *later* in the file is callable, and one inside a
    /// branch that never runs is callable too. `Stmt::Fn` itself emits nothing —
    /// compiling it as well would define every function twice.
    #[test]
    fn fns_are_hoisted_including_from_a_branch_that_never_runs() {
        let out = run_src("print later();\nfn later() { return 7; }\n");
        assert_eq!(out.lines, ["7"]);

        let out = run_src("if false { fn dead() { return 9; } }\nprint dead();\n");
        assert_eq!(out.lines, ["9"], "a fn in a never-taken branch is callable");
        assert_eq!(out.error, None);

        // One definition, and no instruction for the declaration itself: the
        // whole of main here is the `if` (cond, jump, pop nothing) and the print.
        let chunk = chunk_of("if false { fn dead() { return 9; } }\nprint dead();\n");
        assert_eq!(chunk.functions().len(), 1);
        assert_eq!(
            chunk.main.ops(),
            [
                // `Const(2)`, not `Const(0)`: the pool is shared and `dead`'s
                // body was compiled first, so it already holds 9 and nil.
                Op::Const(2),       // false
                Op::JumpIfFalse(2), // the then-block is EMPTY of instructions
                Op::Call { name: 0, argc: 0 },
                Op::Print(1),
            ]
        );
        assert_eq!(chunk.constant(2), Some(&Value::Bool(false)));
    }

    /// `if`/`else` are back-patched **forward** jumps, and an else-less `if`
    /// emits no jump over an arm that is not there.
    #[test]
    fn if_else_back_patches_forward_jumps() {
        let chunk = chunk_of("if true { print 1; } else { print 2; }\n");
        assert_eq!(
            chunk.main.ops(),
            [
                Op::Const(0),       // true
                Op::JumpIfFalse(5), // -> the else arm
                Op::Const(1),       // print 1
                Op::Print(1),
                Op::Jump(7), // -> past the else arm
                Op::Const(2),
                Op::Print(1),
            ]
        );
        assert_eq!(
            run_src("if true { print 1; } else { print 2; }\n").lines,
            ["1"]
        );
        assert_eq!(
            run_src("if false { print 1; } else { print 2; }\n").lines,
            ["2"]
        );

        // Else-less: one forward jump, straight past the body.
        let chunk = chunk_of("if false { print 1; }\nprint 2;\n");
        assert_eq!(
            chunk.main.ops(),
            [
                Op::Const(0),
                Op::JumpIfFalse(4),
                Op::Const(1),
                Op::Print(1),
                Op::Const(2),
                Op::Print(1),
            ]
        );
        assert_eq!(run_src("if false { print 1; }\nprint 2;\n").lines, ["2"]);

        // `else if` is `els: vec![Stmt::If{..}]` and needs no special case.
        assert_eq!(
            run_src("let x = 2;\nif x == 1 { print 1; } else if x == 2 { print 2; } else { print 3; }\n").lines,
            ["2"]
        );

        // §6/`.46`, §6b: a non-Bool condition fails at the CONDITION's line, not
        // at the `if`'s, which is only observable when they differ.
        let out = run_src("let x = 1;\nif\nx {\nprint 1;\n}\n");
        assert_eq!(out.error, Some(TreadleError::not_bool(3, "Int")));

        // With §6b's one fallback (bead `.pqj`): a LITERAL condition has no line
        // of its own, so it reports the enclosing node's — the `if`'s, line 1.
        let out = run_src("if\n1 {\nprint 1;\n}\n");
        assert_eq!(out.error, Some(TreadleError::not_bool(1, "Int")));
    }

    /// A `while` jumps **backward** to its condition, and forward out of it.
    #[test]
    fn while_jumps_backward_to_its_condition() {
        let chunk = chunk_of("let i = 0;\nwhile i < 3 { print i; i = i + 1; }\n");
        assert_eq!(
            chunk.main.ops(),
            [
                Op::Const(0), // let i = 0
                Op::DefineGlobal(0),
                Op::GetGlobal(0), // 2: the loop top — the condition
                Op::Const(1),
                Op::Lt,
                Op::JumpIfFalse(13), // out of the loop, patched after the body
                Op::GetGlobal(0),    // print i
                Op::Print(1),
                Op::GetGlobal(0), // i = i + 1
                Op::Const(2),
                Op::Add,
                Op::SetGlobal(0),
                Op::Jump(2), // BACKWARD, to re-test the condition
                             // 13:
            ]
        );
        // The backward jump targets an instruction before itself.
        assert_eq!(chunk.main.op(12).and_then(Op::jump_target), Some(2));
        assert_eq!(
            run_src("let i = 0;\nwhile i < 3 { print i; i = i + 1; }\n").lines,
            ["0", "1", "2"]
        );

        // Zero iterations, and a body-local popped on every pass.
        assert_eq!(run_src("while false { print 1; }\nprint 2;\n").lines, ["2"]);
        assert_eq!(
            run_src("let i = 0;\nwhile i < 2 { let d = i * 10; print d; i = i + 1; }\n").lines,
            ["0", "10"]
        );
    }

    /// §2/§6/`.33`: one `print` is one line, tab-separated, and only once every
    /// argument evaluated — a failing argument leaves no partial line.
    #[test]
    fn print_of_several_values_is_one_tab_separated_line() {
        let out = run_src("print \"a\", 1, true, nil;\n");
        assert_eq!(out.lines, ["a\t1\ttrue\tnil"]);
        assert_eq!(out.to_string(), "a\t1\ttrue\tnil\n");

        let chunk = chunk_of("print 1, 2;\n");
        assert_eq!(
            chunk.main.ops(),
            [Op::Const(0), Op::Const(1), Op::Print(2)],
            "arguments left to right, then ONE Print"
        );

        let out = run_src("print \"a\", 1 / 0;\n");
        assert_eq!(out.lines, Vec::<String>::new(), "no partial line");
        assert_eq!(out.error, Some(TreadleError::divide_by_zero(1)));
    }

    /// §6/`.35`: globals are resolved **by name at run time**, so a program that
    /// mentions an unknown one still produces the output that precedes it. A
    /// statically resolved slot would have made this empty output plus an error.
    #[test]
    fn an_unknown_global_fails_at_run_time_after_the_output_before_it() {
        let out = run_src("print 1;\nprint nope;\n");
        assert_eq!(out.lines, ["1"]);
        assert_eq!(out.error, Some(TreadleError::undefined_name(2, "nope")));
        assert_eq!(
            out.to_string(),
            "1\nerror: Name at line 2: undefined variable 'nope'\n"
        );

        // A call to an unknown function, likewise — and after prior output.
        let out = run_src("print 1;\nprint nope();\n");
        assert_eq!(out.lines, ["1"]);
        assert_eq!(out.error, Some(TreadleError::undefined_function(2, "nope")));

        // Assignment never creates a binding.
        let out = run_src("print 1;\nx = 2;\n");
        assert_eq!(out.lines, ["1"]);
        assert_eq!(out.error, Some(TreadleError::assign_unbound(2, "x")));
    }

    /// Parameters are the callee's first locals, the arity is recorded for the
    /// call site's check, and a body with no `return` returns `Nil` (§2).
    #[test]
    fn function_bodies_record_arity_and_return_nil_by_default() {
        let chunk = chunk_of("fn f(a, b) { return a - b; }\nprint f(7, 2);\n");
        let f = chunk.function(0).expect("f is compiled");
        assert_eq!((f.name.as_str(), f.arity), ("f", 2));
        assert_eq!(
            f.code.ops(),
            [
                Op::GetLocal(0), // a — where Op::Call left the first argument
                Op::GetLocal(1), // b
                Op::Sub,
                Op::Return,
                // The implicit tail — `return nil` — so a body that falls off
                // the end returns Nil (§2) and always ends in `Return`.
                Op::Const(0),
                Op::Return,
            ]
        );
        assert_eq!(chunk.constant(0), Some(&Value::Nil));
        assert_eq!(
            run_src("fn f(a, b) { return a - b; }\nprint f(7, 2);\n").lines,
            ["5"]
        );

        // No `return` at all, and a bare `return;`: both Nil.
        assert_eq!(
            run_src("fn f() { print 1; }\nprint f();\n").lines,
            ["1", "nil"]
        );
        assert_eq!(run_src("fn f() { return; }\nprint f();\n").lines, ["nil"]);

        // The recorded arity is what the call site checks (§6/.35 step (c)).
        let out = run_src("fn f(a) { return a; }\nprint f(1, 2);\n");
        assert_eq!(out.error, Some(TreadleError::wrong_arity(2, "f", 1, 2)));

        // Recursion, and a local declared inside a branch of the body.
        let out = run_src(
            "fn fact(n) { if n < 2 { return 1; } return n * fact(n - 1); }\nprint fact(5);\n",
        );
        assert_eq!(out.lines, ["120"]);
        assert_eq!(out.error, None);

        // A function body cannot create a global (§6/.37): its top-level `let`
        // is a slot, so the name is not visible outside.
        let out = run_src("fn f() { let inner = 1; return inner; }\nprint f();\nprint inner;\n");
        assert_eq!(out.lines, ["1"]);
        assert_eq!(out.error, Some(TreadleError::undefined_name(3, "inner")));
    }

    /// A nested `fn` is hoisted like any other and compiles to its own
    /// `Function`, with its own slots starting at 0 — the enclosing body's
    /// locals are not visible to it (§2: no closures).
    #[test]
    fn a_nested_fn_gets_its_own_frame_and_slots_from_zero() {
        let chunk = chunk_of(
            "fn outer(a) { fn inner(b) { return b + 1; } return inner(a); }\nprint outer(1);\n",
        );
        assert_eq!(chunk.functions().len(), 2);
        let inner = chunk.function(1).expect("inner is compiled");
        assert_eq!((inner.name.as_str(), inner.arity), ("inner", 1));
        assert_eq!(
            inner.code.op(0),
            Some(Op::GetLocal(0)),
            "b is slot 0 in its own frame"
        );
        assert_eq!(
            run_src(
                "fn outer(a) { fn inner(b) { return b + 1; } return inner(a); }\nprint outer(1);\n"
            )
            .lines,
            ["2"]
        );

        // The enclosing function's parameter is NOT in scope in `inner`, so it
        // compiles to a global by name and fails at run time.
        let out =
            run_src("fn outer(a) { fn inner() { return a; } return inner(); }\nprint outer(1);\n");
        assert_eq!(out.error, Some(TreadleError::undefined_name(1, "a")));
    }

    /// The whole seam, end to end, on the program `ast.rs` uses as its
    /// representative: `compile` handles every `Stmt` variant in one program and
    /// the chunk validates.
    #[test]
    fn compile_handles_every_statement_variant_in_one_program() {
        let src = "\
fn f(a, b) {
    let t = a + b;
    while a < b { a = a + 1; }
    if !(a == b) { return -a; } else { return t; }
    return;
}
let g = 0;
g = f(1, 2);
print g, \"done\";
";
        let chunk = chunk_of(src);
        assert_eq!(chunk.functions().len(), 1);
        let out = machine::run(&chunk);
        assert_eq!(out.lines, ["3\tdone"]);
        assert_eq!(out.error, None);
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
