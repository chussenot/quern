//! beads: machine-core (`.16`) — dispatch, arithmetic, comparison, locals,
//! globals; machine-calls (`.17`) — calls, frames, `Return`, the builtins and
//! the recursion limit (see [`Machine::call`]).
//!
//! # What this file is allowed to decide, and what it is not
//!
//! Almost nothing. Every operator instruction is executed by calling the
//! matching `value.rs` method with the instruction's line, so overflow,
//! divide-by-zero, truncation and every type mismatch are *identical* to the
//! tree-walker by construction rather than by agreement (§6/`.39`, `6a`). This
//! file contains no `i64` arithmetic, no `format!`ed message and no `"\t"`.
//! `Print` hands its already-evaluated values to `Output::print`, which owns the
//! separator and the display form.
//!
//! # The three pins that shaped the loop
//!
//! * **§6/`.35` — nothing is resolved before it executes.** Globals and callees
//!   are addressed by *name* through the chunk's interned pool, so
//!   `print 1; nope();` prints `1` and *then* fails with a `Name` error, and
//!   `if false { nope(); }` runs clean.
//! * **§6/`.33` — one line per `print`, or none.** [`Op::Print`] pops `n`
//!   already-evaluated values and calls `Output::print` once. There is no
//!   incremental emit anywhere in the machine, so a partial line is not
//!   expressible.
//! * **§6/`.37` — locals are stack slots, not declarations.** Slot `n` is
//!   `stack[frame.base + n]`; there is no `DeclareLocal` instruction, which is
//!   what makes `let x = x + 1` read the *outer* `x` for free.
//!
//! # Never a panic (§4)
//!
//! Every index into the stack, the frame, the constant pool and the name pool
//! goes through an `Option` and turns `None` into [`TreadleError::internal`] — a
//! `Type` error documented as unreachable — never an `unwrap`. A malformed
//! chunk therefore *fails*; it does not abort the process. [`Chunk::validate`]
//! proves those cases away for a chunk the compiler built, and is asserted in
//! the tests below; it is deliberately **not** run on every [`run`], because it
//! is a compiler-bug check with no line to report, and the loop is already
//! total without it.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::error::{Result, TreadleError, MAX_DEPTH};
use crate::output::Output;
use crate::value::Value;
use crate::vm::opcode::{Chunk, Code, Op};

/// Run a compiled chunk to completion. Infallible in the [`crate::engine`]
/// sense: everything observable — the printed lines and at most one error — ends
/// up in the returned [`Output`], in the order it happened.
pub fn run(chunk: &Chunk) -> Output {
    let mut out = Output::new();
    let r = Machine::new(chunk).exec(&mut out);
    out.finish(r)
}

/// One activation. `base` is the stack index of local slot 0; `func` is the
/// index of the running [`crate::vm::opcode::Function`], or `None` for `main`.
///
/// `Copy`, so the loop can read the current frame without borrowing `self`.
#[derive(Debug, Clone, Copy)]
struct Frame {
    base: usize,
    func: Option<u32>,
    ip: usize,
}

/// The stack machine. One per [`run`]; nothing carries between two runs (§2).
struct Machine<'c> {
    chunk: &'c Chunk,
    /// The value stack. Locals live here — see the module docs.
    stack: Vec<Value>,
    /// Globals by NAME (§6/`.35`). Iteration order is never observed.
    globals: HashMap<String, Value>,
    /// `main` is `frames[0]` with `base == 0`, so a frame always exists and
    /// the top level is not a special case.
    frames: Vec<Frame>,
    /// **Active invocations**, not frames (§6/`.36`): `main` is depth 0 and is
    /// not an invocation, so this is always `frames.len() - 1` and is kept
    /// separately only so the check reads as the spec sentence does.
    depth: usize,
}

impl<'c> Machine<'c> {
    fn new(chunk: &'c Chunk) -> Machine<'c> {
        Machine {
            chunk,
            stack: Vec::new(),
            globals: HashMap::new(),
            frames: vec![Frame {
                base: 0,
                func: None,
                ip: 0,
            }],
            depth: 0,
        }
    }

    /// The dispatch loop. Returns on the first error, leaving everything already
    /// printed in `out` (§3: output *then* the error).
    fn exec(&mut self, out: &mut Output) -> Result<()> {
        loop {
            let frame = match self.frames.last() {
                Some(f) => *f,
                None => return Err(TreadleError::internal(0, "no frame")),
            };
            let code = self.code_of(frame.func)?;
            let Some(op) = code.op(frame.ip) else {
                // Off the end of `main` is a normal finish — there is no `Halt`.
                // Off the end of a function body cannot happen: the compiler
                // always emits `Const(nil); Return`.
                if self.frames.len() == 1 {
                    return Ok(());
                }
                return Err(TreadleError::internal(
                    0,
                    "ran off the end of a function body",
                ));
            };
            // §6/`.46`: the error's line is this instruction's own line.
            let line = code.line(frame.ip).unwrap_or(0);
            self.set_ip(frame.ip + 1)?;
            self.step(op, frame, line, code, out)?;
        }
    }

    /// Execute one instruction. Split out of [`Machine::exec`] only so the
    /// bookkeeping above it is read once.
    fn step(
        &mut self,
        op: Op,
        frame: Frame,
        line: u32,
        code: &'c Code,
        out: &mut Output,
    ) -> Result<()> {
        match op {
            Op::Const(i) => {
                // `Value::Str` is an `Rc`, so this clone is a refcount bump.
                let v = match self.chunk.constant(i) {
                    Some(v) => v.clone(),
                    None => {
                        return Err(TreadleError::internal(line, "constant index out of range"))
                    }
                };
                self.stack.push(v);
            }
            Op::Pop => {
                self.pop(line)?;
            }

            Op::GetLocal(n) => {
                let v = match self.slot(frame, n) {
                    Some(i) => self.stack[i].clone(),
                    None => return Err(TreadleError::internal(line, "local slot out of range")),
                };
                self.stack.push(v);
            }
            Op::SetLocal(n) => {
                let v = self.pop(line)?;
                match self.slot(frame, n) {
                    Some(i) => self.stack[i] = v,
                    None => return Err(TreadleError::internal(line, "local slot out of range")),
                }
            }

            Op::GetGlobal(i) => {
                let name = self.name(i, line)?;
                let v = match self.globals.get(name) {
                    Some(v) => v.clone(),
                    // §6/`.35`: a runtime failure, so anything printed first stands.
                    None => return Err(TreadleError::undefined_name(line, name)),
                };
                self.stack.push(v);
            }
            Op::SetGlobal(i) => {
                let v = self.pop(line)?;
                let name = self.name(i, line)?;
                // §2: assignment never creates a binding.
                match self.globals.get_mut(name) {
                    Some(slot) => *slot = v,
                    None => return Err(TreadleError::assign_unbound(line, name)),
                }
            }
            Op::DefineGlobal(i) => {
                let v = self.pop(line)?;
                let name = self.name(i, line)?;
                // Create-or-replace: re-`let` is legal and the later one wins.
                self.globals.insert(name.to_string(), v);
            }

            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::Eq
            | Op::Ne
            | Op::Lt
            | Op::Gt
            | Op::Le
            | Op::Ge => {
                // `b` was pushed last (§: `[a b] -> [c]`).
                let b = self.pop(line)?;
                let a = self.pop(line)?;
                self.stack.push(binary(op, &a, &b, line)?);
            }
            Op::Neg => {
                let a = self.pop(line)?;
                self.stack.push(a.neg(line)?);
            }
            Op::Not => {
                let a = self.pop(line)?;
                self.stack.push(a.not(line)?);
            }
            // No truthiness (§6/`.40`): this is the type check, and it is the
            // same `as_bool` an `if` condition goes through, so the two cannot
            // disagree on the message.
            Op::AsBool => {
                let a = self.pop(line)?;
                self.stack.push(Value::Bool(a.as_bool(line)?));
            }

            Op::Jump(t) => self.jump(t, code, line)?,
            Op::JumpIfFalse(t) => {
                let v = self.pop(line)?;
                if !v.as_bool(line)? {
                    self.jump(t, code, line)?;
                }
            }

            Op::Call { name, argc } => self.call(name, argc, line)?,
            Op::Return => self.ret(line)?,

            Op::Print(n) => {
                let n = n as usize;
                if self.stack.len() < n {
                    return Err(TreadleError::internal(line, "stack underflow in print"));
                }
                // Exactly one line, and only now that every argument has
                // already been evaluated (§6/`.33`).
                let args = self.stack.split_off(self.stack.len() - n);
                out.print(&args);
            }
        }
        Ok(())
    }

    // ---- calls (bd .17) ---------------------------------------------------

    /// Enter a call. The `argc` arguments are already on the stack, evaluated
    /// left to right — that is step (a), and the bytecode is what forces it, so
    /// `nope(1/0)` is `divide by zero`.
    ///
    /// The remaining steps are §6/`.35`'s, in exactly this order and all at the
    /// call's line: **(b)** resolve the name — the three builtins first, since
    /// they are reserved (§6/`.42`) and so cannot be shadowed — then
    /// [`Chunk::find_function`], else `undefined_function`; **(c)** arity;
    /// **(d)** `depth == MAX_DEPTH`; **(e)** enter.
    ///
    /// The order of (c) before (d) is observable: a wrong-arity call made at
    /// depth `MAX_DEPTH` reports the *arity* error. See
    /// `machine_wrong_arity_at_the_depth_limit_reports_the_arity_error`.
    fn call(&mut self, name: u32, argc: u32, line: u32) -> Result<()> {
        let callee = self.name(name, line)?;
        if matches!(callee, "len" | "str" | "int") {
            return self.call_builtin(callee, argc, line);
        }
        let idx = self
            .chunk
            .find_function(callee)
            .ok_or_else(|| TreadleError::undefined_function(line, callee))?;
        let arity = match self.chunk.function(idx) {
            Some(f) => f.arity,
            None => return Err(TreadleError::internal(line, "function index out of range")),
        };
        if arity != argc {
            return Err(TreadleError::wrong_arity(
                line,
                callee,
                arity as usize,
                argc as usize,
            ));
        }
        // (d) — the callee's frame does not exist yet, so the MAX_DEPTHth
        // nested invocation succeeds and the next one fails here.
        if self.depth == MAX_DEPTH {
            return Err(TreadleError::recursion_limit(line));
        }
        // (e) — the arguments become the callee's first locals where they
        // already lie: slot `n` is `stack[base + n]`, so nothing moves. The
        // frame carries no reference to the caller's, which is what makes a
        // body see globals and its own locals and nothing else (§2, no
        // closures).
        let base = match self.stack.len().checked_sub(argc as usize) {
            Some(b) => b,
            None => return Err(TreadleError::internal(line, "stack underflow in call")),
        };
        self.frames.push(Frame {
            base,
            func: Some(idx),
            ip: 0,
        });
        self.depth += 1;
        Ok(())
    }

    /// `len`/`str`/`int`. A builtin is **not an invocation**: it consumes no
    /// depth and cannot hit the recursion limit (§6/`.36`), because there is no
    /// frame to enter — it runs here and leaves its result on the stack.
    ///
    /// Every builtin takes exactly one argument, so (c) is `argc != 1`.
    //
    // ponytail: the three bodies are here rather than in `value.rs` because
    // §6/`.39` (the bead that would have put them there) never landed and
    // value.rs is frozen. Both engines therefore spell them independently —
    // the wording is still error.rs's, but the *choice* of constructor is not
    // pinned anywhere. bd .39 is the upgrade path.
    fn call_builtin(&mut self, name: &str, argc: u32, line: u32) -> Result<()> {
        if argc != 1 {
            return Err(TreadleError::wrong_arity(line, name, 1, argc as usize));
        }
        let v = self.pop(line)?;
        let out = match (name, &v) {
            // §2: bytes, not characters.
            ("len", Value::Str(s)) => Value::Int(s.len() as i64),
            // §3's one display form, so `str(x)` and `print x` agree.
            ("str", _) => Value::str(v.to_string()),
            // §6/`.41`: exactly `s.parse::<i64>()`.
            ("int", Value::Str(s)) => match s.parse::<i64>() {
                Ok(n) => Value::Int(n),
                Err(_) => return Err(TreadleError::bad_int(line, s.as_str())),
            },
            ("len" | "int", _) => {
                return Err(TreadleError::unary_type_mismatch(
                    line,
                    name,
                    "Str",
                    v.type_name(),
                ))
            }
            _ => return Err(TreadleError::internal(line, "not a builtin")),
        };
        self.stack.push(out);
        Ok(())
    }

    /// Leave a call: pop the return value, drop the callee's whole frame —
    /// arguments and locals both — and push the value in the caller, whose `ip`
    /// the loop already advanced past its `Call`.
    ///
    /// `Return` in `main` is unreachable: `return` outside a function is a
    /// `Parse` error (§2), so the front end never produces it.
    fn ret(&mut self, line: u32) -> Result<()> {
        if self.frames.len() < 2 {
            return Err(TreadleError::internal(line, "return outside of a function"));
        }
        let v = self.pop(line)?;
        let frame = match self.frames.pop() {
            Some(f) => f,
            None => return Err(TreadleError::internal(line, "no frame")),
        };
        self.stack.truncate(frame.base);
        self.stack.push(v);
        self.depth = self.depth.saturating_sub(1);
        Ok(())
    }

    // ---- helpers, all total ----------------------------------------------

    /// The code of the running function, or `main`.
    fn code_of(&self, func: Option<u32>) -> Result<&'c Code> {
        match func {
            None => Ok(&self.chunk.main),
            Some(i) => match self.chunk.function(i) {
                Some(f) => Ok(&f.code),
                None => Err(TreadleError::internal(0, "function index out of range")),
            },
        }
    }

    fn set_ip(&mut self, ip: usize) -> Result<()> {
        match self.frames.last_mut() {
            Some(f) => {
                f.ip = ip;
                Ok(())
            }
            None => Err(TreadleError::internal(0, "no frame")),
        }
    }

    /// A jump target is an absolute index into `code`; one past the end is
    /// legal and means "done". Anything further — including an unpatched
    /// [`crate::vm::opcode::PLACEHOLDER`] — is a broken chunk, not a silent halt.
    fn jump(&mut self, target: u32, code: &Code, line: u32) -> Result<()> {
        let t = target as usize;
        if t > code.len() {
            return Err(TreadleError::internal(line, "jump target out of range"));
        }
        self.set_ip(t)
    }

    fn pop(&mut self, line: u32) -> Result<Value> {
        match self.stack.pop() {
            Some(v) => Ok(v),
            None => Err(TreadleError::internal(line, "stack underflow")),
        }
    }

    /// The absolute stack index of local slot `n`, if it is in the frame.
    fn slot(&self, frame: Frame, n: u32) -> Option<usize> {
        let i = frame.base.checked_add(n as usize)?;
        (i < self.stack.len()).then_some(i)
    }

    fn name(&self, i: u32, line: u32) -> Result<&'c str> {
        self.chunk
            .name(i)
            .ok_or_else(|| TreadleError::internal(line, "name index out of range"))
    }
}

/// Every binary instruction, executed by `value.rs`. The comparison spelling
/// comes from [`Op::symbol`], so no operator message is spelled here.
fn binary(op: Op, a: &Value, b: &Value, line: u32) -> Result<Value> {
    Ok(match op {
        Op::Add => a.add(b, line)?,
        Op::Sub => a.sub(b, line)?,
        Op::Mul => a.mul(b, line)?,
        Op::Div => a.div(b, line)?,
        Op::Rem => a.rem(b, line)?,
        Op::Eq => Value::Bool(a.eq_value(b, line)?),
        Op::Ne => Value::Bool(!a.eq_value(b, line)?),
        Op::Lt | Op::Gt | Op::Le | Op::Ge => {
            let sym = op
                .symbol()
                .ok_or_else(|| TreadleError::internal(line, "comparison without a spelling"))?;
            let ord = a.cmp_value(b, sym, line)?;
            Value::Bool(match op {
                Op::Lt => ord == Ordering::Less,
                Op::Gt => ord == Ordering::Greater,
                Op::Le => ord != Ordering::Greater,
                _ => ord != Ordering::Less,
            })
        }
        _ => return Err(TreadleError::internal(line, "not a binary instruction")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::opcode::{Function, PLACEHOLDER};

    /// A chunk whose `main` prints one expression, built from a closure so each
    /// test reads as the program it stands for. Everything is on line 1 unless a
    /// test says otherwise.
    fn chunk(build: impl FnOnce(&mut Chunk)) -> Chunk {
        let mut c = Chunk::new();
        build(&mut c);
        c
    }

    /// Push two constants and apply `op`, then print — the shape every binary
    /// test wants.
    fn bin(a: Value, b: Value, op: Op, line: u32) -> Output {
        let c = chunk(|c| {
            let (ia, ib) = (c.add_const(a), c.add_const(b));
            c.main.emit(Op::Const(ia), line);
            c.main.emit(Op::Const(ib), line);
            c.main.emit(op, line);
            c.main.emit(Op::Print(1), line);
        });
        assert_eq!(c.validate(), Ok(()), "the test built a valid chunk");
        run(&c)
    }

    // ---- arithmetic and comparison, all of it through value.rs -----------

    #[test]
    fn machine_runs_every_binary_operator() {
        for (op, want) in [
            (Op::Add, "9\n"),
            (Op::Sub, "5\n"),
            (Op::Mul, "14\n"),
            (Op::Div, "3\n"),
            (Op::Rem, "1\n"),
            (Op::Eq, "false\n"),
            (Op::Ne, "true\n"),
            (Op::Lt, "false\n"),
            (Op::Gt, "true\n"),
            (Op::Le, "false\n"),
            (Op::Ge, "true\n"),
        ] {
            let got = bin(Value::Int(7), Value::Int(2), op, 1).to_string();
            assert_eq!(got, want, "7 {op:?} 2");
        }
        // `+` on two Strs is concatenation, and the operands are in push order.
        assert_eq!(
            bin(Value::str("ab"), Value::str("cd"), Op::Add, 1).to_string(),
            "abcd\n"
        );
        // Str ordering is byte-lexicographic, decided by value.rs.
        assert_eq!(
            bin(Value::str("a"), Value::str("b"), Op::Lt, 1).to_string(),
            "true\n"
        );
    }

    #[test]
    fn machine_runs_both_unary_operators() {
        let c = chunk(|c| {
            let i = c.add_const(Value::Int(5));
            let t = c.add_const(Value::Bool(true));
            c.main.emit(Op::Const(i), 1);
            c.main.emit(Op::Neg, 1);
            c.main.emit(Op::Print(1), 1);
            c.main.emit(Op::Const(t), 2);
            c.main.emit(Op::Not, 2);
            c.main.emit(Op::Print(1), 2);
        });
        assert_eq!(run(&c).to_string(), "-5\nfalse\n");
    }

    /// The whole point of routing through `value.rs`: the VM produces the
    /// error `error.rs`'s constructor produces, at the *instruction's* line.
    #[test]
    fn machine_overflow_and_divide_by_zero_come_from_the_constructors() {
        let over = bin(Value::Int(i64::MAX), Value::Int(1), Op::Add, 4);
        assert_eq!(over.to_string(), format!("{}\n", TreadleError::overflow(4)));

        let div = bin(Value::Int(1), Value::Int(0), Op::Div, 9);
        assert_eq!(
            div.to_string(),
            format!("{}\n", TreadleError::divide_by_zero(9))
        );

        let rem = bin(Value::Int(1), Value::Int(0), Op::Rem, 9);
        assert_eq!(
            rem.to_string(),
            format!("{}\n", TreadleError::modulo_by_zero(9))
        );

        // i64::MIN / -1 is an overflow, not a panic (§6/.41).
        let min = bin(Value::Int(i64::MIN), Value::Int(-1), Op::Div, 2);
        assert_eq!(min.to_string(), format!("{}\n", TreadleError::overflow(2)));

        // A type mismatch is value.rs's message too, never a local format!.
        let bad = bin(Value::Int(1), Value::Bool(true), Op::Sub, 3);
        assert_eq!(
            bad.to_string(),
            format!(
                "{}\n",
                TreadleError::type_mismatch(3, "-", "Int", "Int", "Bool")
            )
        );
    }

    // ---- print ------------------------------------------------------------

    #[test]
    fn machine_print_makes_exactly_one_line_from_n_values() {
        let c = chunk(|c| {
            let a = c.add_const(Value::Int(1));
            let b = c.add_const(Value::str("x"));
            let n = c.add_const(Value::Nil);
            c.main.emit(Op::Const(a), 1);
            c.main.emit(Op::Const(b), 1);
            c.main.emit(Op::Const(n), 1);
            c.main.emit(Op::Print(3), 1);
        });
        // output.rs owns the separator; this asserts one line, not the tab.
        assert_eq!(run(&c).to_string().lines().count(), 1);
        assert_eq!(run(&c).to_string(), "1\tx\tnil\n");
    }

    /// §6/`.33`: `print "a", 1/0;` — no partial line, because `Print` cannot
    /// run until every argument has already been evaluated.
    #[test]
    fn machine_print_emits_nothing_when_an_argument_fails() {
        let c = chunk(|c| {
            let a = c.add_const(Value::str("a"));
            let one = c.add_const(Value::Int(1));
            let zero = c.add_const(Value::Int(0));
            c.main.emit(Op::Const(a), 1);
            c.main.emit(Op::Const(one), 1);
            c.main.emit(Op::Const(zero), 1);
            c.main.emit(Op::Div, 1);
            c.main.emit(Op::Print(2), 1);
        });
        assert_eq!(
            run(&c).to_string(),
            format!("{}\n", TreadleError::divide_by_zero(1))
        );
    }

    // ---- globals and locals ----------------------------------------------

    /// §6/`.35`: the failure is the *instruction's*, so everything printed
    /// before it stands. `print 1; nope();` — here `print nope;`.
    #[test]
    fn machine_unknown_global_fails_at_runtime_after_earlier_output() {
        let c = chunk(|c| {
            let one = c.add_const(Value::Int(1));
            let nope = c.add_name("nope");
            c.main.emit(Op::Const(one), 1);
            c.main.emit(Op::Print(1), 1);
            c.main.emit(Op::GetGlobal(nope), 2);
            c.main.emit(Op::Print(1), 2);
        });
        assert_eq!(c.validate(), Ok(()), "an unknown global is a valid chunk");
        assert_eq!(
            run(&c).to_string(),
            format!("1\n{}\n", TreadleError::undefined_name(2, "nope"))
        );
    }

    #[test]
    fn machine_globals_define_read_and_assign() {
        let c = chunk(|c| {
            let x = c.add_name("x");
            let one = c.add_const(Value::Int(1));
            let two = c.add_const(Value::Int(2));
            c.main.emit(Op::Const(one), 1);
            c.main.emit(Op::DefineGlobal(x), 1);
            c.main.emit(Op::Const(two), 2);
            c.main.emit(Op::SetGlobal(x), 2);
            c.main.emit(Op::GetGlobal(x), 3);
            c.main.emit(Op::Print(1), 3);
            // Re-`let` in the same scope is legal; the later one wins.
            c.main.emit(Op::Const(one), 4);
            c.main.emit(Op::DefineGlobal(x), 4);
            c.main.emit(Op::GetGlobal(x), 5);
            c.main.emit(Op::Print(1), 5);
        });
        assert_eq!(run(&c).to_string(), "2\n1\n");
    }

    /// §2: assignment never creates a binding.
    #[test]
    fn machine_assigning_an_unbound_global_is_a_name_error() {
        let c = chunk(|c| {
            let y = c.add_name("y");
            let one = c.add_const(Value::Int(1));
            c.main.emit(Op::Const(one), 6);
            c.main.emit(Op::SetGlobal(y), 6);
        });
        assert_eq!(
            run(&c).to_string(),
            format!("{}\n", TreadleError::assign_unbound(6, "y"))
        );
    }

    /// Locals are stack slots at `base + n` and there is no declaration
    /// instruction, which is exactly why §6/`.37` (`let x = x + 1` sees the
    /// outer binding) holds without the machine doing anything: the initialiser
    /// has already been evaluated when the slot appears.
    #[test]
    fn machine_locals_are_stack_slots_at_base_plus_n() {
        let c = chunk(|c| {
            let one = c.add_const(Value::Int(1));
            let ten = c.add_const(Value::Int(10));
            let x = c.add_name("x");
            // let x = 1;            (global, so the inner `let` can read it)
            c.main.emit(Op::Const(one), 1);
            c.main.emit(Op::DefineGlobal(x), 1);
            // { let y = x + 10;  y = y + 1;  print y; }   -> slot 0 of main
            c.main.emit(Op::GetGlobal(x), 2);
            c.main.emit(Op::Const(ten), 2);
            c.main.emit(Op::Add, 2);
            c.main.emit(Op::GetLocal(0), 3);
            c.main.emit(Op::Const(one), 3);
            c.main.emit(Op::Add, 3);
            c.main.emit(Op::SetLocal(0), 3);
            c.main.emit(Op::GetLocal(0), 4);
            c.main.emit(Op::Print(1), 4);
            c.main.emit(Op::Pop, 5); // leaving the block pops the local
        });
        assert_eq!(run(&c).to_string(), "12\n");
    }

    // ---- conditions and control flow -------------------------------------

    #[test]
    fn machine_as_bool_on_a_non_bool_is_a_type_error() {
        let c = chunk(|c| {
            let i = c.add_const(Value::Int(0));
            c.main.emit(Op::Const(i), 7);
            c.main.emit(Op::AsBool, 7);
            c.main.emit(Op::Print(1), 7);
        });
        assert_eq!(
            run(&c).to_string(),
            format!("{}\n", TreadleError::not_bool(7, "Int"))
        );

        // The same message for a condition, which is the point of one `as_bool`.
        let c = chunk(|c| {
            let s = c.add_const(Value::str(""));
            c.main.emit(Op::Const(s), 8);
            c.main.emit(Op::JumpIfFalse(3), 8);
        });
        assert_eq!(
            run(&c).to_string(),
            format!("{}\n", TreadleError::not_bool(8, "Str"))
        );
    }

    /// `and`/`or` have no instruction — they are the jump shape in opcode's
    /// contract, and this asserts the machine implements exactly it: the left
    /// operand is always type-checked (`JumpIfFalse` pops it through
    /// `as_bool`), the right one only on the path where it is evaluated, and
    /// the result is always a `Bool`. §6/`.40`.
    #[test]
    fn machine_and_shape_short_circuits_and_types_only_a_taken_rhs() {
        // print <lhs> and 1;   with the rhs on line 2.
        let and = |lhs: Value| {
            let c = chunk(|c| {
                let l = c.add_const(lhs);
                let one = c.add_const(Value::Int(1));
                let f = c.add_const(Value::Bool(false));
                c.main.emit(Op::Const(l), 1);
                let skip = c.main.emit_jump(Op::JumpIfFalse, 1);
                c.main.emit(Op::Const(one), 2);
                c.main.emit(Op::AsBool, 2);
                let done = c.main.emit_jump(Op::Jump, 1);
                c.main.patch_jump(skip);
                c.main.emit(Op::Const(f), 1);
                c.main.patch_jump(done);
                c.main.emit(Op::Print(1), 1);
            });
            assert_eq!(c.validate(), Ok(()));
            run(&c).to_string()
        };
        // The rhs is never touched, so its type never matters.
        assert_eq!(and(Value::Bool(false)), "false\n");
        // Evaluated, so `AsBool` raises at the RHS's line, not the lhs's.
        assert_eq!(
            and(Value::Bool(true)),
            format!("{}\n", TreadleError::not_bool(2, "Int"))
        );
        // The lhs is type-checked on both paths, by the same `as_bool`.
        assert_eq!(
            and(Value::Nil),
            format!("{}\n", TreadleError::not_bool(1, "Nil"))
        );
    }

    /// A forward jump: `if false { print 1; } print 2;`.
    #[test]
    fn machine_jumps_forward_over_an_untaken_branch() {
        let c = chunk(|c| {
            let f = c.add_const(Value::Bool(false));
            let one = c.add_const(Value::Int(1));
            let two = c.add_const(Value::Int(2));
            c.main.emit(Op::Const(f), 1);
            let site = c.main.emit_jump(Op::JumpIfFalse, 1);
            c.main.emit(Op::Const(one), 2);
            c.main.emit(Op::Print(1), 2);
            c.main.patch_jump(site);
            c.main.emit(Op::Const(two), 4);
            c.main.emit(Op::Print(1), 4);
        });
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(run(&c).to_string(), "2\n");
    }

    /// A backward jump: `let i = 0; while i < 3 { print i; i = i + 1; }`,
    /// with `i` as a global so the loop body needs no slot bookkeeping.
    #[test]
    fn machine_jumps_backward_around_a_loop() {
        let c = chunk(|c| {
            let i = c.add_name("i");
            let zero = c.add_const(Value::Int(0));
            let one = c.add_const(Value::Int(1));
            let three = c.add_const(Value::Int(3));
            c.main.emit(Op::Const(zero), 1);
            c.main.emit(Op::DefineGlobal(i), 1);
            let top = c.main.len();
            c.main.emit(Op::GetGlobal(i), 2);
            c.main.emit(Op::Const(three), 2);
            c.main.emit(Op::Lt, 2);
            let exit = c.main.emit_jump(Op::JumpIfFalse, 2);
            c.main.emit(Op::GetGlobal(i), 3);
            c.main.emit(Op::Print(1), 3);
            c.main.emit(Op::GetGlobal(i), 4);
            c.main.emit(Op::Const(one), 4);
            c.main.emit(Op::Add, 4);
            c.main.emit(Op::SetGlobal(i), 4);
            c.main.emit_jump_to(Op::Jump, top, 2);
            c.main.patch_jump(exit);
        });
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(run(&c).to_string(), "0\n1\n2\n");
    }

    /// A jump whose target is exactly one past the end is a normal finish —
    /// `patch_jump` on the last instruction produces it, so this is the common
    /// case, not an edge one.
    #[test]
    fn machine_jump_one_past_the_end_finishes_cleanly() {
        let c = chunk(|c| {
            let t = c.add_const(Value::Bool(true));
            c.main.emit(Op::Const(t), 1);
            let site = c.main.emit_jump(Op::JumpIfFalse, 1);
            c.main.patch_jump(site);
        });
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(run(&c).to_string(), "");
    }

    // ---- §4: a malformed chunk fails, it does not panic -------------------

    #[test]
    fn machine_malformed_chunks_error_instead_of_panicking() {
        // Each of these is rejected by validate(), and each would be an
        // `unwrap` in a machine that trusted its input.
        let cases: Vec<(&str, Chunk)> = vec![
            (
                "bad constant index",
                chunk(|c| {
                    c.main.emit(Op::Const(3), 1);
                }),
            ),
            (
                "bad name index",
                chunk(|c| {
                    c.main.emit(Op::GetGlobal(0), 1);
                }),
            ),
            (
                "stack underflow",
                chunk(|c| {
                    c.main.emit(Op::Pop, 1);
                }),
            ),
            (
                "underflowing binary",
                chunk(|c| {
                    c.main.emit(Op::Add, 1);
                }),
            ),
            (
                "missing local slot",
                chunk(|c| {
                    c.main.emit(Op::GetLocal(4), 1);
                }),
            ),
            (
                "unpatched jump",
                chunk(|c| {
                    c.main.emit(Op::Jump(PLACEHOLDER), 1);
                }),
            ),
            (
                "jump past the end",
                chunk(|c| {
                    c.main.emit(Op::Jump(99), 1);
                }),
            ),
            (
                "print of more values than exist",
                chunk(|c| {
                    c.main.emit(Op::Print(2), 1);
                }),
            ),
        ];
        for (what, c) in cases {
            let rendered = run(&c).to_string();
            assert!(
                rendered.starts_with("error: Type at line 1: internal error: "),
                "{what}: expected an internal error, got {rendered:?}"
            );
        }
        // A bad function index in a frame is unreachable from `main`, but the
        // helper that would hit it is total too.
        let empty = Chunk::new();
        let m = Machine::new(&empty);
        assert!(m.code_of(Some(0)).is_err());
        assert!(m.code_of(None).is_ok());
    }

    // ---- calls, frames and Return (bd .17) --------------------------------

    /// A function body, built the way `main` is.
    fn func(name: &str, arity: u32, build: impl FnOnce(&mut Code)) -> Function {
        let mut code = Code::new();
        build(&mut code);
        Function {
            name: name.to_string(),
            arity,
            code,
        }
    }

    /// `fn add(a, b) { return a + b; }  print add(1, 2);  print add(10, 20);`
    /// — two calls in a row, so a leaked frame or a stack the callee failed to
    /// clean up shows as a wrong second answer rather than as nothing.
    #[test]
    fn machine_calls_a_function_and_returns_its_value() {
        let c = chunk(|c| {
            let add = c.add_name("add");
            let (a, b) = (c.add_const(Value::Int(1)), c.add_const(Value::Int(2)));
            let (x, y) = (c.add_const(Value::Int(10)), c.add_const(Value::Int(20)));
            c.add_function(func("add", 2, |code| {
                code.emit(Op::GetLocal(0), 1);
                code.emit(Op::GetLocal(1), 1);
                code.emit(Op::Add, 1);
                code.emit(Op::Return, 1);
            }));
            c.main.emit(Op::Const(a), 3);
            c.main.emit(Op::Const(b), 3);
            c.main.emit(Op::Call { name: add, argc: 2 }, 3);
            c.main.emit(Op::Print(1), 3);
            c.main.emit(Op::Const(x), 4);
            c.main.emit(Op::Const(y), 4);
            c.main.emit(Op::Call { name: add, argc: 2 }, 4);
            c.main.emit(Op::Print(1), 4);
        });
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(run(&c).to_string(), "3\n30\n");
    }

    /// §2, no closures: a body sees globals and its **own** locals, never the
    /// caller's. `main` holds 7 in its own slot 0 and calls `f(1)`; `f` reads
    /// slot 0 and a global. If the callee's frame were not rebased it would see
    /// the caller's 7 and print 12.
    #[test]
    fn machine_a_body_sees_globals_and_its_own_locals_not_the_callers() {
        let c = chunk(|c| {
            let f = c.add_name("f");
            let g = c.add_name("g");
            let five = c.add_const(Value::Int(5));
            let seven = c.add_const(Value::Int(7));
            let one = c.add_const(Value::Int(1));
            c.add_function(func("f", 1, |code| {
                code.emit(Op::GetGlobal(g), 1);
                code.emit(Op::GetLocal(0), 1);
                code.emit(Op::Add, 1);
                code.emit(Op::Return, 1);
            }));
            c.main.emit(Op::Const(five), 2);
            c.main.emit(Op::DefineGlobal(g), 2);
            c.main.emit(Op::Const(seven), 3); // main's own slot 0
            c.main.emit(Op::Const(one), 4);
            c.main.emit(Op::Call { name: f, argc: 1 }, 4);
            c.main.emit(Op::Print(1), 4);
            c.main.emit(Op::Pop, 5); // leaving the block pops main's local
        });
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(run(&c).to_string(), "6\n");
    }

    /// A function with no `return`: the compiler ends every body with
    /// `Const(nil); Return`, so the call yields `Nil`.
    #[test]
    fn machine_a_function_without_a_return_yields_nil() {
        let c = chunk(|c| {
            let f = c.add_name("f");
            let nil = c.add_const(Value::Nil);
            let one = c.add_const(Value::Int(1));
            c.add_function(func("f", 0, |code| {
                code.emit(Op::Const(one), 1);
                code.emit(Op::Print(1), 1);
                code.emit(Op::Const(nil), 2);
                code.emit(Op::Return, 2);
            }));
            c.main.emit(Op::Call { name: f, argc: 0 }, 5);
            c.main.emit(Op::Print(1), 5);
        });
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(run(&c).to_string(), "1\nnil\n");
    }

    /// `fn f(n) { if n == 0 { <extra> return 0; } return f(n - 1); }` and
    /// `print f(start);`. The recursive call is on **line 3**, the top-level one
    /// on line 9 and `extra` on line 4, so a test can tell which call failed.
    /// `extra` interns whatever it needs and returns the instructions to run in
    /// the base case, at depth `start + 1`.
    fn countdown(start: i64, extra: impl FnOnce(&mut Chunk) -> Vec<(Op, u32)>) -> Chunk {
        let mut c = Chunk::new();
        let extra = extra(&mut c);
        let zero = c.add_const(Value::Int(0));
        let one = c.add_const(Value::Int(1));
        let n = c.add_const(Value::Int(start));
        let f = c.add_name("f");
        c.add_function(func("f", 1, |code| {
            code.emit(Op::GetLocal(0), 2);
            code.emit(Op::Const(zero), 2);
            code.emit(Op::Eq, 2);
            let els = code.emit_jump(Op::JumpIfFalse, 2);
            for (op, line) in extra {
                code.emit(op, line);
            }
            code.emit(Op::Const(zero), 2);
            code.emit(Op::Return, 2);
            code.patch_jump(els);
            code.emit(Op::GetLocal(0), 3);
            code.emit(Op::Const(one), 3);
            code.emit(Op::Sub, 3);
            code.emit(Op::Call { name: f, argc: 1 }, 3);
            code.emit(Op::Return, 3);
        }));
        c.main.emit(Op::Const(n), 9);
        c.main.emit(Op::Call { name: f, argc: 1 }, 9);
        c.main.emit(Op::Print(1), 9);
        c
    }

    /// §6/`.36`, the pinned edge: the counted quantity is **active
    /// invocations**, `main` is depth 0 and is not one, and the check is
    /// `depth == MAX_DEPTH` at the call site — so a chain of exactly
    /// `MAX_DEPTH` invocations succeeds and the next call fails, at the failing
    /// **call's** line. The frames are heap-allocated, so 1000 of them cost the
    /// test thread nothing.
    #[test]
    fn machine_recurses_to_the_depth_limit_and_fails_on_the_next_call() {
        // f(999) is invocation 1 ... f(0) is invocation 1000.
        let c = countdown((MAX_DEPTH - 1) as i64, |_| vec![]);
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(run(&c).to_string(), "0\n");

        // One deeper: the 1001st invocation is refused by the recursive call on
        // line 3, not by the top-level call on line 9.
        let c = countdown(MAX_DEPTH as i64, |_| vec![]);
        assert_eq!(
            run(&c).to_string(),
            format!("{}\n", TreadleError::recursion_limit(3))
        );
    }

    /// Wrong argument count is a `Type` error naming both counts, from
    /// `error.rs` — `add expects 2 arguments, got 1`.
    #[test]
    fn machine_wrong_argument_count_is_a_type_error_naming_both_counts() {
        let c = chunk(|c| {
            let add = c.add_name("add");
            let one = c.add_const(Value::Int(1));
            c.add_function(func("add", 2, |code| {
                code.emit(Op::GetLocal(0), 1);
                code.emit(Op::Return, 1);
            }));
            c.main.emit(Op::Const(one), 4);
            c.main.emit(Op::Call { name: add, argc: 1 }, 4);
            c.main.emit(Op::Print(1), 4);
        });
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(
            run(&c).to_string(),
            format!("{}\n", TreadleError::wrong_arity(4, "add", 2, 1))
        );
    }

    /// The observable consequence of arity (c) preceding depth (d): a
    /// wrong-arity call made *at* the limit reports the arity error. If the two
    /// checks were swapped this would say `recursion limit`.
    #[test]
    fn machine_wrong_arity_at_the_depth_limit_reports_the_arity_error() {
        let c = countdown((MAX_DEPTH - 1) as i64, |c| {
            let g = c.add_name("g");
            let nil = c.add_const(Value::Nil);
            c.add_function(func("g", 1, |code| {
                code.emit(Op::Const(nil), 1);
                code.emit(Op::Return, 1);
            }));
            vec![(Op::Call { name: g, argc: 0 }, 4), (Op::Pop, 4)]
        });
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(
            run(&c).to_string(),
            format!("{}\n", TreadleError::wrong_arity(4, "g", 1, 0))
        );
    }

    /// §6/`.36`: builtins are not invocations. This calls `len` at depth
    /// `MAX_DEPTH` — if a builtin consumed depth, or were checked against the
    /// limit at all, it would be a `recursion limit` error instead of `4`.
    #[test]
    fn machine_builtins_consume_no_depth() {
        let c = countdown((MAX_DEPTH - 1) as i64, |c| {
            let len = c.add_name("len");
            let s = c.add_const(Value::str("abcd"));
            vec![
                (Op::Const(s), 4),
                (Op::Call { name: len, argc: 1 }, 4),
                (Op::Return, 4),
            ]
        });
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(run(&c).to_string(), "4\n");
    }

    /// The three builtins (§2), which are reserved names and so resolve before
    /// the function table. Their errors are `error.rs` constructors like every
    /// other.
    #[test]
    fn machine_builtins_are_len_str_and_int() {
        // `print <builtin>(v);` with the call on line 2.
        let call1 = |name: &str, v: Value| {
            let c = chunk(|c| {
                let b = c.add_name(name);
                let i = c.add_const(v);
                c.main.emit(Op::Const(i), 2);
                c.main.emit(Op::Call { name: b, argc: 1 }, 2);
                c.main.emit(Op::Print(1), 2);
            });
            assert_eq!(c.validate(), Ok(()));
            run(&c).to_string()
        };

        // len is BYTES, not characters: "hé" is three.
        assert_eq!(call1("len", Value::str("hé")), "3\n");
        assert_eq!(call1("len", Value::str("")), "0\n");
        // str is §3's one display form, the same one `print` uses.
        assert_eq!(call1("str", Value::Nil), "nil\n");
        assert_eq!(call1("str", Value::Int(-12)), "-12\n");
        assert_eq!(call1("str", Value::Bool(true)), "true\n");
        // int is exactly `parse::<i64>()`.
        assert_eq!(call1("int", Value::str("-12")), "-12\n");
        assert_eq!(
            call1("int", Value::str("x")),
            format!("{}\n", TreadleError::bad_int(2, "x"))
        );
        assert_eq!(
            call1("int", Value::str(" 1")),
            format!("{}\n", TreadleError::bad_int(2, " 1"))
        );
        // The wrong argument type, and the wrong argument count.
        assert_eq!(
            call1("len", Value::Int(1)),
            format!(
                "{}\n",
                TreadleError::unary_type_mismatch(2, "len", "Str", "Int")
            )
        );
        assert_eq!(
            call1("int", Value::Nil),
            format!(
                "{}\n",
                TreadleError::unary_type_mismatch(2, "int", "Str", "Nil")
            )
        );
        let two_args = chunk(|c| {
            let len = c.add_name("len");
            let s = c.add_const(Value::str("ab"));
            c.main.emit(Op::Const(s), 6);
            c.main.emit(Op::Const(s), 6);
            c.main.emit(Op::Call { name: len, argc: 2 }, 6);
            c.main.emit(Op::Print(1), 6);
        });
        assert_eq!(
            run(&two_args).to_string(),
            format!("{}\n", TreadleError::wrong_arity(6, "len", 1, 2))
        );
    }

    /// §2: functions are hoisted, and §6/`.35` resolves a callee by name when
    /// the `Call` executes — so a call emitted *before* the function exists
    /// works, and a call to a name that never gets one is a runtime `Name`
    /// error with everything printed before it intact.
    #[test]
    fn machine_calls_a_function_declared_after_the_call_site() {
        let mut c = Chunk::new();
        let f = c.add_name("f");
        let one = c.add_const(Value::Int(1));
        c.main.emit(Op::Call { name: f, argc: 0 }, 1);
        c.main.emit(Op::Print(1), 1);
        // ... and only now does `f` exist.
        c.add_function(func("f", 0, |code| {
            code.emit(Op::Const(one), 4);
            code.emit(Op::Return, 4);
        }));
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(run(&c).to_string(), "1\n");

        let c = chunk(|c| {
            let nope = c.add_name("nope");
            let one = c.add_const(Value::Int(1));
            c.main.emit(Op::Const(one), 1);
            c.main.emit(Op::Print(1), 1);
            c.main.emit(
                Op::Call {
                    name: nope,
                    argc: 0,
                },
                2,
            );
            c.main.emit(Op::Print(1), 2);
        });
        assert_eq!(
            run(&c).to_string(),
            format!("1\n{}\n", TreadleError::undefined_function(2, "nope"))
        );
    }

    /// Mutual recursion: `even`/`odd` call each other, so every frame's code
    /// comes from a different function and the frame stack — not a single
    /// callee — is what carries the state.
    #[test]
    fn machine_mutual_recursion_alternates_between_two_functions() {
        let c = chunk(|c| {
            let (even, odd) = (c.add_name("even"), c.add_name("odd"));
            let zero = c.add_const(Value::Int(0));
            let one = c.add_const(Value::Int(1));
            let four = c.add_const(Value::Int(4));
            let t = c.add_const(Value::Bool(true));
            let fa = c.add_const(Value::Bool(false));
            // fn even(n) { if n == 0 { return true; } return odd(n - 1); }
            let base = |code: &mut Code, answer: u32, other: u32, line: u32| {
                code.emit(Op::GetLocal(0), line);
                code.emit(Op::Const(zero), line);
                code.emit(Op::Eq, line);
                let els = code.emit_jump(Op::JumpIfFalse, line);
                code.emit(Op::Const(answer), line);
                code.emit(Op::Return, line);
                code.patch_jump(els);
                code.emit(Op::GetLocal(0), line);
                code.emit(Op::Const(one), line);
                code.emit(Op::Sub, line);
                code.emit(
                    Op::Call {
                        name: other,
                        argc: 1,
                    },
                    line,
                );
                code.emit(Op::Return, line);
            };
            c.add_function(func("even", 1, |code| base(code, t, odd, 1)));
            c.add_function(func("odd", 1, |code| base(code, fa, even, 2)));
            c.main.emit(Op::Const(four), 5);
            c.main.emit(
                Op::Call {
                    name: even,
                    argc: 1,
                },
                5,
            );
            c.main.emit(Op::Print(1), 5);
            c.main.emit(Op::Const(four), 6);
            c.main.emit(Op::Call { name: odd, argc: 1 }, 6);
            c.main.emit(Op::Print(1), 6);
        });
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(run(&c).to_string(), "true\nfalse\n");
    }

    /// `Return` from inside a loop inside an `if`: the frame goes away whatever
    /// the `ip` was doing and whatever the body left on the stack, and the
    /// caller resumes past its own `Call` — the loop advanced `ip` before
    /// executing, so there is no return address to fix up.
    #[test]
    fn machine_returns_from_inside_nested_jumps() {
        let c = chunk(|c| {
            let f = c.add_name("f");
            let zero = c.add_const(Value::Int(0));
            let one = c.add_const(Value::Int(1));
            let three = c.add_const(Value::Int(3));
            let answer = c.add_const(Value::Int(42));
            let nil = c.add_const(Value::Nil);
            let t = c.add_const(Value::Bool(true));
            // fn f(n) { while true { if n == 0 { return 42; } n = n - 1; } }
            c.add_function(func("f", 1, |code| {
                let top = code.len();
                code.emit(Op::Const(t), 1);
                let exit = code.emit_jump(Op::JumpIfFalse, 1);
                code.emit(Op::GetLocal(0), 2);
                code.emit(Op::Const(zero), 2);
                code.emit(Op::Eq, 2);
                let skip = code.emit_jump(Op::JumpIfFalse, 2);
                code.emit(Op::Const(answer), 3);
                code.emit(Op::Return, 3);
                code.patch_jump(skip);
                code.emit(Op::GetLocal(0), 4);
                code.emit(Op::Const(one), 4);
                code.emit(Op::Sub, 4);
                code.emit(Op::SetLocal(0), 4);
                code.emit_jump_to(Op::Jump, top, 1);
                code.patch_jump(exit);
                code.emit(Op::Const(nil), 6);
                code.emit(Op::Return, 6);
            }));
            c.main.emit(Op::Const(three), 8);
            c.main.emit(Op::Call { name: f, argc: 1 }, 8);
            c.main.emit(Op::Print(1), 8);
        });
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(run(&c).to_string(), "42\n");
    }

    /// `return` outside a function is a `Parse` error (§2), so a `Return` in
    /// `main` is a broken chunk — and like every other broken chunk it fails
    /// instead of popping the last frame out from under the loop.
    #[test]
    fn machine_return_at_the_top_level_is_an_internal_error() {
        let c = chunk(|c| {
            let one = c.add_const(Value::Int(1));
            c.main.emit(Op::Const(one), 1);
            c.main.emit(Op::Return, 1);
        });
        assert_eq!(
            run(&c).to_string(),
            format!(
                "{}\n",
                TreadleError::internal(1, "return outside of a function")
            )
        );
    }
}
