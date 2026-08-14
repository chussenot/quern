//! bead: treadle-opcode — the instruction set and Chunk
//!
//! Group A's bytecode. `compiler-*` writes it, `machine-*` executes it, and the
//! **stack effect documented on every [`Op`] variant is the contract between
//! them** — they are different authors, so a mismatch here is a divergence the
//! differential fuzzer would report against the wrong engine.
//!
//! # The three §6 pins this design exists to satisfy
//!
//! * **`.35` — the VM raises no compile-time errors.** Globals are accessed by
//!   **name** ([`Op::GetGlobal`] and friends carry an index into [`Chunk`]'s
//!   *name* pool, never a resolved slot), and a `Call` carries the callee's
//!   name, not its address. So `print 1; nope();` prints `1` and *then* fails
//!   with a `Name` error, exactly like the tree-walker; and
//!   `if false { nope(); }` compiles and runs clean. Anything that resolved a
//!   name at compile time would destroy the partial-output rule.
//! * **`.36` — the recursion limit counts active invocations.** [`Op::Call`]
//!   is a single instruction that performs, in order: (a) the arguments are
//!   already on the stack, evaluated left to right, (b) resolve the name,
//!   (c) check arity, (d) check `depth == error::MAX_DEPTH`, (e) enter. The
//!   line table entry for the `Call` is the **call's** line, which is where
//!   `error::recursion_limit` must point.
//! * **`.33` — `print` appends exactly one line, only if every argument
//!   succeeded.** [`Op::Print`] takes the argument *count*, consumes that many
//!   already-evaluated values, and appends one line. There is no
//!   emit-as-you-go instruction, so a partial line is not expressible.
//!
//! # Values, and who owns arithmetic
//!
//! Values are **cloned** (§ non-goals: say which — `Value::Str` is an `Rc`, so
//! a clone is a refcount bump). Every operator instruction is executed by
//! calling the matching `value.rs` method with the instruction's line; the VM
//! never does raw `i64` arithmetic and never formats an error message.
//!
//! # The line table
//!
//! A **parallel `Vec<u32>` inside [`Code`]**, one entry per instruction —
//! `code.line(ip)` is the line of `code.op(ip)`. §6/`.46` only requires an
//! entry per *fallible* instruction; one per instruction is a superset that
//! [`Code::emit`] makes impossible to forget, and it costs 4 bytes per op.
//! The line is always the line of the **innermost** AST node that produced the
//! instruction, never the enclosing statement's.
//!
//! # Frame layout (implied by the stack effects, so stated once here)
//!
//! Locals live **on the value stack**. A frame has a `base`, and slot `n` is
//! `stack[base + n]`. A callee's parameters are therefore already in slots
//! `0..arity`: `Op::Call` leaves the arguments where the caller pushed them and
//! sets `base = stack.len() - argc`. There is no "declare local" instruction —
//! a `let` inside a function compiles to just its initialiser, and the slot the
//! value lands in *is* the local (which is why §6/`.37`, "the initialiser is
//! evaluated before the binding exists", is satisfied for free). Leaving a
//! block pops one value per local declared in it; `Op::Return` truncates the
//! stack to `base`, so an early return needs no pops.

use crate::front::ast::{BinOp, UnOp};
use crate::value::Value;

/// The operand written for a not-yet-patched jump target.
///
/// Out of range by construction, so an unpatched jump is caught by
/// [`Chunk::validate`] rather than silently jumping somewhere plausible.
pub const PLACEHOLDER: u32 = u32::MAX;

/// One instruction.
///
/// All operands are `u32` (§6/`.45`: no `u8` operands — no program-size limit
/// may be observable in `Output`). `Copy`, so the machine can `match` an op by
/// value without borrowing the chunk.
///
/// **Stack effect notation:** `[a b] -> [c]` means the instruction pops `b`
/// then `a` (`b` was on top) and pushes `c`. "fallible" names the errors it can
/// raise, all at the instruction's line from the line table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Push `chunk.constant(i)`. `[] -> [v]`. Infallible.
    Const(u32),
    /// Discard the top value. `[v] -> []`. Infallible. Used for a statement's
    /// leftover value and for popping a block's locals on scope exit (one `Pop`
    /// per local; there is deliberately no `PopN`).
    Pop,

    /// Push the value in local slot `n` of the current frame. `[] -> [v]`.
    /// Infallible: the compiler only emits a slot it has declared.
    GetLocal(u32),
    /// Store the top value into local slot `n`, **consuming** it.
    /// `[v] -> []`. Infallible. This is `x = e;` where `x` is a local.
    SetLocal(u32),

    /// Push the value of the global named `chunk.name(i)`. `[] -> [v]`.
    /// Fallible: `error::undefined_name` if no such global exists — a
    /// **runtime** failure, per §6/`.35`.
    GetGlobal(u32),
    /// Assign the top value to the existing global named `chunk.name(i)`,
    /// consuming it. `[v] -> []`. Fallible: `error::assign_unbound` if the
    /// global does not already exist (§2: assignment never creates a binding).
    SetGlobal(u32),
    /// Create-or-replace the global named `chunk.name(i)` with the top value,
    /// consuming it. `[v] -> []`. Infallible: re-declaring in the same scope is
    /// legal and the later `let` wins (§6/`.37`). This is a top-level `let`;
    /// top-level scope **is** global scope, and a function body never emits it.
    DefineGlobal(u32),

    /// `[a b] -> [a + b]`. Fallible: `Value::add` (overflow, or a `Type` error;
    /// `+` is also `Str` concatenation).
    Add,
    /// `[a b] -> [a - b]`. Fallible: `Value::sub`.
    Sub,
    /// `[a b] -> [a * b]`. Fallible: `Value::mul`.
    Mul,
    /// `[a b] -> [a / b]`. Fallible: `Value::div` (divide by zero, overflow on
    /// `i64::MIN / -1`, `Type`).
    Div,
    /// `[a b] -> [a % b]`. Fallible: `Value::rem`.
    Rem,

    /// `[a b] -> [Bool]`. Fallible: `Value::eq_value` — comparing two different
    /// types is a `Type` error, not `false`.
    Eq,
    /// `[a b] -> [Bool]`, the negation of [`Op::Eq`]. Same fallibility.
    Ne,
    /// `[a b] -> [Bool]`. Fallible: `Value::cmp_value(.., "<", ..)`.
    Lt,
    /// `[a b] -> [Bool]`. Fallible: `Value::cmp_value(.., ">", ..)`.
    Gt,
    /// `[a b] -> [Bool]`. Fallible: `Value::cmp_value(.., "<=", ..)`.
    Le,
    /// `[a b] -> [Bool]`. Fallible: `Value::cmp_value(.., ">=", ..)`.
    Ge,

    /// `[a] -> [-a]`. Fallible: `Value::neg` (overflow on `-i64::MIN`, `Type`).
    Neg,
    /// `[a] -> [!a]`. Fallible: `Value::not` (`Type` on a non-`Bool`).
    Not,
    /// `[a] -> [Bool]` — assert the top value is a `Bool` and leave it there.
    /// Fallible: `Value::as_bool`. There is no truthiness (§6/`.40`); this is
    /// how the **right** operand of `and`/`or` gets type-checked, and it is
    /// emitted only on the path where that operand is evaluated.
    AsBool,

    /// Set `ip` to `target`. `[] -> []`. Infallible. One instruction covers
    /// both directions: `target` is an **absolute** index into the same
    /// [`Code`], so a forward jump is emitted with [`PLACEHOLDER`] and patched
    /// by [`Code::patch_jump`], and `while`'s backward jump is emitted with a
    /// target already known ([`Code::emit_jump_to`]).
    Jump(u32),
    /// Pop one value; if it is `false`, set `ip` to `target`. `[v] -> []`.
    /// Fallible: `Value::as_bool` — the condition of `if`/`while` must be a
    /// `Bool`, at the **condition's** line.
    JumpIfFalse(u32),

    /// Call the function named `chunk.name(name)` with `argc` arguments.
    /// `[a1 .. aN] -> [result]` — pops exactly `argc` values (already
    /// evaluated left to right, `a1` deepest) and pushes the one return value
    /// (`Nil` for a function that falls off its end).
    ///
    /// Fallible, in **exactly this order** (§6/`.35`, `.36`), all at the
    /// call's line:
    /// 1. `error::undefined_function` — the name resolves to neither a builtin
    ///    (`len`/`str`/`int`) nor a [`Function`] in this chunk.
    /// 2. `error::wrong_arity`.
    /// 3. `error::recursion_limit` when `depth == error::MAX_DEPTH`, checked
    ///    *before* the callee's frame exists, so the 1000th nested invocation
    ///    succeeds and the 1001st fails. Builtins do not consume depth.
    ///
    /// Builtins are **not** a separate instruction: `len`, `str` and `int` are
    /// reserved function names (§6/`.42` — declaring one is a `Parse` error and
    /// they are a separate namespace from variables), so a `Call` naming one is
    /// unambiguously the builtin.
    Call { name: u32, argc: u32 },
    /// Return from the current function. `[v] -> []` in the callee — pops the
    /// return value, truncates the stack to the frame's `base`, and pushes the
    /// value in the caller. `[] -> [v]` from the caller's point of view, which
    /// is the second half of [`Op::Call`]'s effect. Infallible.
    ///
    /// The compiler emits `Const(nil); Return` for a bare `return;` and again
    /// at the end of every function body, so a [`Function`]'s code always ends
    /// in a `Return` and the machine never runs off the end of a frame.
    Return,

    /// Pop `n` values and append **exactly one** line to `Output.lines`, by
    /// handing them to `Output::print(&[Value])` in push order — that function
    /// owns the tab separator and the `Display` form, so neither engine spells
    /// them. `[a1 .. aN] -> []`. Infallible — every argument has already been
    /// evaluated, which is precisely what makes a partial line unrepresentable
    /// (§6/`.33`). `n >= 1` always (§6/`.46`).
    Print(u32),
}

impl Op {
    /// The instruction implementing `op`, or `None` for [`BinOp::Or`] and
    /// [`BinOp::And`] — which have **no instruction** because they must
    /// short-circuit (§2), and are compiled to jumps instead:
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
    /// That shape gets §6/`.40` right by construction: the left operand is
    /// always type-checked (`JumpIfFalse` calls `as_bool`), the right one only
    /// on the path where it is evaluated, and the result is always a `Bool`.
    /// `false and 1` is `false`; `true and 1` is a `Type` error.
    pub fn binary(op: BinOp) -> Option<Op> {
        Some(match op {
            BinOp::Or | BinOp::And => return None,
            BinOp::Eq => Op::Eq,
            BinOp::Ne => Op::Ne,
            BinOp::Lt => Op::Lt,
            BinOp::Gt => Op::Gt,
            BinOp::Le => Op::Le,
            BinOp::Ge => Op::Ge,
            BinOp::Add => Op::Add,
            BinOp::Sub => Op::Sub,
            BinOp::Mul => Op::Mul,
            BinOp::Div => Op::Div,
            BinOp::Rem => Op::Rem,
        })
    }

    /// The instruction implementing a prefix operator. Total — both `UnOp`s
    /// have one.
    pub fn unary(op: UnOp) -> Op {
        match op {
            UnOp::Neg => Op::Neg,
            UnOp::Not => Op::Not,
        }
    }

    /// The source spelling of an operator instruction, for the `op` argument of
    /// `Value::cmp_value`. Here rather than in the machine so the four
    /// comparison messages cannot be spelled from memory at the call site.
    pub fn symbol(self) -> Option<&'static str> {
        Some(match self {
            Op::Add => "+",
            Op::Sub => "-",
            Op::Mul => "*",
            Op::Div => "/",
            Op::Rem => "%",
            Op::Eq => "==",
            Op::Ne => "!=",
            Op::Lt => "<",
            Op::Gt => ">",
            Op::Le => "<=",
            Op::Ge => ">=",
            Op::Neg => "-",
            Op::Not => "!",
            _ => return None,
        })
    }

    /// The jump target, for anything that needs to walk the code (validation,
    /// disassembly).
    pub fn jump_target(self) -> Option<u32> {
        match self {
            Op::Jump(t) | Op::JumpIfFalse(t) => Some(t),
            _ => None,
        }
    }
}

/// A run of instructions with its line table. One per function, plus one for
/// the top level ([`Chunk::main`]).
///
/// The two vectors are private and only [`Code::emit`] grows them, so
/// `ops.len() == lines.len()` cannot be broken by a caller — the whole reason
/// the line table is a parallel vector rather than a field on each `Op`.
#[derive(Debug, Clone, Default)]
pub struct Code {
    ops: Vec<Op>,
    lines: Vec<u32>,
}

impl Code {
    pub fn new() -> Code {
        Code::default()
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Append `op` at source `line`, returning its index.
    pub fn emit(&mut self, op: Op, line: u32) -> usize {
        self.ops.push(op);
        self.lines.push(line);
        self.ops.len() - 1
    }

    /// Append a forward jump with an unresolved target, returning the site to
    /// hand to [`Code::patch_jump`]. `make` is the variant constructor, e.g.
    /// `code.emit_jump(Op::JumpIfFalse, line)`.
    pub fn emit_jump(&mut self, make: fn(u32) -> Op, line: u32) -> usize {
        self.emit(make(PLACEHOLDER), line)
    }

    /// Append a jump to an already-known target — `while`'s backward jump.
    pub fn emit_jump_to(&mut self, make: fn(u32) -> Op, target: usize, line: u32) -> usize {
        self.emit(make(target as u32), line)
    }

    /// Point the jump at `site` to the **next** instruction to be emitted.
    ///
    /// Does nothing if `site` does not hold a jump: an unpatched jump keeps
    /// [`PLACEHOLDER`], which [`Chunk::validate`] reports. Nothing here panics,
    /// because §4 forbids a panic on any input.
    pub fn patch_jump(&mut self, site: usize) {
        let target = self.ops.len() as u32;
        self.patch_jump_to(site, target);
    }

    /// Point the jump at `site` to an explicit target.
    pub fn patch_jump_to(&mut self, site: usize, target: u32) {
        if let Some(Op::Jump(t) | Op::JumpIfFalse(t)) = self.ops.get_mut(site) {
            *t = target;
        }
    }

    /// The instruction at `ip`, or `None` past the end.
    pub fn op(&self, ip: usize) -> Option<Op> {
        self.ops.get(ip).copied()
    }

    /// **The line table lookup**: the source line of the instruction at `ip`.
    pub fn line(&self, ip: usize) -> Option<u32> {
        self.lines.get(ip).copied()
    }

    pub fn ops(&self) -> &[Op] {
        &self.ops
    }
}

/// A compiled function. `arity` is checked at the call site; `name` is only
/// used to resolve a `Call` and to fill in `error::wrong_arity`.
#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub arity: u32,
    pub code: Code,
}

/// A whole compiled program: `compile(&ast::Program) -> Chunk`, `run(&Chunk)`.
///
/// The constant and name pools are **shared** across `main` and every
/// [`Function`], so an index means the same thing anywhere in the chunk.
#[derive(Debug, Clone, Default)]
pub struct Chunk {
    /// Top-level code. Execution starts at `ip = 0` here and ends when `ip`
    /// reaches `main.len()`; there is no `Halt`.
    pub main: Code,
    fns: Vec<Function>,
    consts: Vec<Value>,
    names: Vec<String>,
}

impl Chunk {
    pub fn new() -> Chunk {
        Chunk::default()
    }

    /// Intern a constant, returning its [`Op::Const`] operand. Equal values
    /// share one slot, so `add_const` twice gives one index.
    //
    // ponytail: linear scan. The pools are per-program and small; switch to a
    // side map keyed by a hashable projection of Value if a fuzzer program ever
    // makes this show up in a profile.
    pub fn add_const(&mut self, v: Value) -> u32 {
        match self.consts.iter().position(|c| *c == v) {
            Some(i) => i as u32,
            None => {
                self.consts.push(v);
                (self.consts.len() - 1) as u32
            }
        }
    }

    /// Intern a name (a global's or a callee's), returning the operand for
    /// `Get/Set/DefineGlobal` or `Call`.
    pub fn add_name(&mut self, name: &str) -> u32 {
        match self.names.iter().position(|n| n == name) {
            Some(i) => i as u32,
            None => {
                self.names.push(name.to_string());
                (self.names.len() - 1) as u32
            }
        }
    }

    /// The constant for an [`Op::Const`] operand. `None` is a broken engine
    /// invariant, not a program error — report it with `error::internal` and
    /// note that [`Chunk::validate`] proves it cannot happen.
    pub fn constant(&self, i: u32) -> Option<&Value> {
        self.consts.get(i as usize)
    }

    /// The name for a global or `Call` operand. `None` is `error::internal`,
    /// as for [`Chunk::constant`].
    pub fn name(&self, i: u32) -> Option<&str> {
        self.names.get(i as usize).map(String::as_str)
    }

    /// Add a compiled function, returning its index.
    pub fn add_function(&mut self, f: Function) -> u32 {
        self.fns.push(f);
        (self.fns.len() - 1) as u32
    }

    /// Resolve a `Call`'s name to a function index, **at run time** — this is
    /// step (b) of [`Op::Call`] and returning `None` here is what makes an
    /// unknown function a runtime `Name` error rather than a compile-time one
    /// (§6/`.35`). Builtins are not in this table; check them first.
    ///
    /// Returning the index rather than a reference keeps the machine's frames
    /// free of a borrow of the chunk.
    //
    // ponytail: linear scan over a handful of functions. Duplicate names are a
    // Parse error, so the first match is the only match.
    pub fn find_function(&self, name: &str) -> Option<u32> {
        self.fns
            .iter()
            .position(|f| f.name == name)
            .map(|i| i as u32)
    }

    /// The function at an index from [`Chunk::find_function`].
    pub fn function(&self, i: u32) -> Option<&Function> {
        self.fns.get(i as usize)
    }

    pub fn functions(&self) -> &[Function] {
        &self.fns
    }

    /// Check the invariants the machine relies on, so a compiler bug fails here
    /// instead of as an `error::internal` in the middle of a program. Cheap and
    /// structural: every pool index in range, every jump target resolved and
    /// landing inside its own code, no duplicate function names.
    ///
    /// `Err` is a plain `String`: a broken chunk is not a treadle error and has
    /// no line to report.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_code("<main>", &self.main)?;
        for (i, f) in self.fns.iter().enumerate() {
            if self.fns[..i].iter().any(|g| g.name == f.name) {
                return Err(format!("duplicate function {}", f.name));
            }
            self.validate_code(&f.name, &f.code)?;
        }
        Ok(())
    }

    fn validate_code(&self, where_: &str, code: &Code) -> Result<(), String> {
        if code.ops.len() != code.lines.len() {
            return Err(format!(
                "{where_}: line table has {} entries for {} instructions",
                code.lines.len(),
                code.ops.len()
            ));
        }
        for (ip, op) in code.ops.iter().enumerate() {
            let bad = |what: &str| Err(format!("{where_}: instruction {ip} {what}"));
            match *op {
                Op::Const(i) if self.constant(i).is_none() => return bad("has a bad constant"),
                Op::GetGlobal(i)
                | Op::SetGlobal(i)
                | Op::DefineGlobal(i)
                | Op::Call { name: i, .. }
                    if self.name(i).is_none() =>
                {
                    return bad("has a bad name")
                }
                Op::Jump(t) | Op::JumpIfFalse(t) => {
                    if t == PLACEHOLDER {
                        return bad("is an unpatched jump");
                    }
                    if t as usize > code.ops.len() {
                        return bad("jumps outside its code");
                    }
                }
                Op::Print(0) => return bad("prints zero arguments"),
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `print x;` on line 1, `print 2;` on line 7 — the line table must map
    /// each instruction back to its own line, since §4 compares error lines
    /// between the engines and the VM has no AST at run time.
    #[test]
    fn line_table_maps_each_instruction_to_its_line() {
        let mut c = Chunk::new();
        let x = c.add_name("x");
        let two = c.add_const(Value::Int(2));
        c.main.emit(Op::GetGlobal(x), 1);
        c.main.emit(Op::Print(1), 1);
        c.main.emit(Op::Const(two), 7);
        c.main.emit(Op::Print(1), 7);

        assert_eq!(c.main.len(), 4);
        assert_eq!(c.main.line(0), Some(1));
        assert_eq!(c.main.line(1), Some(1));
        assert_eq!(c.main.line(2), Some(7));
        assert_eq!(c.main.line(3), Some(7));
        assert_eq!(c.main.line(4), None, "no line past the end");
        assert_eq!(c.main.op(0), Some(Op::GetGlobal(x)));
        assert_eq!(c.main.op(4), None);
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn pools_intern_and_bounds_check() {
        let mut c = Chunk::new();
        let a = c.add_const(Value::Int(1));
        let b = c.add_const(Value::str("hi"));
        assert_eq!(
            c.add_const(Value::Int(1)),
            a,
            "equal constants share a slot"
        );
        assert_ne!(a, b);
        assert_eq!(c.constant(a), Some(&Value::Int(1)));
        assert_eq!(c.constant(b), Some(&Value::str("hi")));
        assert_eq!(c.constant(2), None);

        let n = c.add_name("total");
        assert_eq!(c.add_name("total"), n);
        assert_eq!(c.name(n), Some("total"));
        assert_eq!(c.name(n + 1), None);
    }

    /// The `if` shape: a forward `JumpIfFalse` patched to the instruction after
    /// the then-branch.
    #[test]
    fn forward_jump_is_patched_to_the_next_instruction() {
        let mut c = Chunk::new();
        let t = c.add_const(Value::Bool(true));
        c.main.emit(Op::Const(t), 1);
        let site = c.main.emit_jump(Op::JumpIfFalse, 1);
        assert_eq!(c.main.op(site), Some(Op::JumpIfFalse(PLACEHOLDER)));
        assert!(c.validate().is_err(), "unpatched jump must not validate");

        c.main.emit(Op::Pop, 2);
        c.main.patch_jump(site);
        assert_eq!(c.main.op(site), Some(Op::JumpIfFalse(3)));
        assert_eq!(c.main.len(), 3, "target may be one past the end");
        assert_eq!(c.validate(), Ok(()));
    }

    /// The `while` shape: the body ends in a backward jump to the condition,
    /// and the condition's forward jump lands after the body.
    #[test]
    fn backward_jump_targets_the_loop_top() {
        let mut c = Chunk::new();
        let t = c.add_const(Value::Bool(true));
        let top = c.main.len();
        c.main.emit(Op::Const(t), 3);
        let exit = c.main.emit_jump(Op::JumpIfFalse, 3);
        c.main.emit(Op::Pop, 4);
        let back = c.main.emit_jump_to(Op::Jump, top, 3);
        c.main.patch_jump(exit);

        assert_eq!(c.main.op(back).and_then(Op::jump_target), Some(top as u32));
        assert!(back > top, "the backward jump really is backward");
        assert_eq!(c.main.op(exit), Some(Op::JumpIfFalse(4)));
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_bad_indices_and_empty_print() {
        let mut c = Chunk::new();
        c.main.emit(Op::Const(0), 1);
        assert!(c.validate().is_err(), "no constant 0 yet");

        let mut c = Chunk::new();
        c.main.emit(Op::GetGlobal(0), 1);
        assert!(c.validate().is_err());

        let mut c = Chunk::new();
        c.main.emit(Op::Print(0), 1);
        assert!(c.validate().is_err(), "print takes >= 1 argument");

        let mut c = Chunk::new();
        c.main.emit(Op::Jump(9), 1);
        assert!(c.validate().is_err(), "jump outside the code");
    }

    #[test]
    fn functions_resolve_by_name_at_runtime() {
        let mut c = Chunk::new();
        let mut body = Code::new();
        let nil = c.add_const(Value::Nil);
        body.emit(Op::GetLocal(0), 1);
        body.emit(Op::Return, 1);
        body.emit(Op::Const(nil), 1);
        body.emit(Op::Return, 1);
        let idx = c.add_function(Function {
            name: "id".to_string(),
            arity: 1,
            code: body,
        });

        let name = c.add_name("id");
        let one = c.add_const(Value::Int(1));
        c.main.emit(Op::Const(one), 2);
        c.main.emit(Op::Call { name, argc: 1 }, 2);
        c.main.emit(Op::Print(1), 2);

        assert_eq!(c.find_function("id"), Some(idx));
        assert_eq!(c.function(idx).map(|f| f.arity), Some(1));
        // §6/.35: an unknown callee is not a compile-time error. The chunk is
        // valid; the machine raises Name when the instruction executes.
        assert_eq!(c.find_function("nope"), None);
        let missing = c.add_name("nope");
        c.main.emit(
            Op::Call {
                name: missing,
                argc: 0,
            },
            3,
        );
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(
            c.main.line(3),
            Some(3),
            "the Call's line is the error's line"
        );
    }

    /// Every `BinOp` is accounted for: 11 have an instruction, and `Or`/`And`
    /// have none because they compile to jumps.
    #[test]
    fn every_operator_maps_to_an_instruction_or_to_jumps() {
        use BinOp::*;
        for op in [Eq, Ne, Lt, Gt, Le, Ge, Add, Sub, Mul, Div, Rem] {
            let i = Op::binary(op).expect("has an instruction");
            assert!(i.symbol().is_some(), "{op:?} must carry its spelling");
        }
        assert_eq!(Op::binary(Or), None);
        assert_eq!(Op::binary(And), None);
        assert_eq!(Op::unary(UnOp::Neg), Op::Neg);
        assert_eq!(Op::unary(UnOp::Not), Op::Not);
        assert_eq!(Op::Le.symbol(), Some("<="));
        assert_eq!(Op::Pop.symbol(), None);
    }
}
