//! bead: machine-core (`.16`) — the stack machine: dispatch, arithmetic,
//! comparison, locals, globals. Calls, frames and the recursion limit are
//! `machine-calls` (`.17`), which shares this file; see [`Machine::call`].
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

use crate::error::{Result, TreadleError};
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
    /// `machine-calls` has nothing special to do for the top level.
    frames: Vec<Frame>,
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

    // ---- calls: the seam for machine-calls (bd .17) -----------------------

    /// **`machine-calls` owns this.** Everything it needs is already here: push
    /// a [`Frame`] with `base: self.stack.len() - argc`, `func:
    /// chunk.find_function(name)` and `ip: 0` — the arguments are already in
    /// slots `0..argc` where the caller pushed them — and add a `depth: usize`
    /// field for the §6/`.36` check. The loop reads the frame fresh every
    /// iteration and resolves its code through [`Machine::code_of`], so growing
    /// `self.frames` needs no change above this line.
    ///
    /// Order is fixed by §6/`.35`: name (builtins `len`/`str`/`int` first), then
    /// arity, then `depth == error::MAX_DEPTH`, then enter.
    fn call(&mut self, _name: u32, _argc: u32, line: u32) -> Result<()> {
        Err(TreadleError::internal(line, "call is not implemented yet"))
    }

    /// **`machine-calls` owns this.** Pop the return value, truncate the stack
    /// to `frame.base`, pop the frame, push the value.
    fn ret(&mut self, line: u32) -> Result<()> {
        Err(TreadleError::internal(
            line,
            "return is not implemented yet",
        ))
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
    use crate::vm::opcode::PLACEHOLDER;

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

    /// `Call`/`Return` are `machine-calls` (bd .17). Until then they fail
    /// cleanly rather than panicking or silently doing nothing — which is also
    /// the assertion that the loop reaches them at all.
    #[test]
    fn machine_call_and_return_are_the_seam_for_bd_17() {
        let c = chunk(|c| {
            let f = c.add_name("f");
            c.main.emit(Op::Call { name: f, argc: 0 }, 5);
        });
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(
            run(&c).to_string(),
            format!(
                "{}\n",
                TreadleError::internal(5, "call is not implemented yet")
            )
        );
    }
}
