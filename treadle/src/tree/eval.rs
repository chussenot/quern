//! bead: treadle-eval — tree-walking evaluator
//!
//! Group B's back end: walk the frozen `Expr`/`Stmt` tree directly against an
//! [`Env`], pushing lines into an [`Output`] as they are produced.
//!
//! # Two beads share this file
//!
//! `.19` (eval-expr) owns **expressions**: [`Eval::eval`] and everything it
//! reaches. `.20` (eval-stmt) owns **statements**, control flow, function-body
//! execution and the `Engine` impl. The seam between them is
//! [`Eval::exec_body`] — see its doc comment, which describes what is
//! deliberately temporary there.
//!
//! # Why almost nothing is decided here
//!
//! The point of two engines is that they agree, so this module owns as few
//! decisions as it can:
//!
//! * every arithmetic, comparison and Bool-gate **semantic and message** comes
//!   from `value.rs` (§6/`.39`) — this file never adds, divides or compares two
//!   `Value`s itself, because a second implementation of overflow or
//!   truncate-toward-zero is exactly the drift the fuzzer exists to find;
//! * every error is an `error.rs` constructor (§4) — there is no `format!` in
//!   this file and there must never be one;
//! * the recursion limit is `error::MAX_DEPTH` (§6/`.36`), not a local `1000`;
//! * scope, shadowing and the two namespaces are `env.rs`'s (§6/`.37`, `.42`).
//!
//! What *is* decided here is **order**, which no other module can express:
//! left-to-right operands and arguments, short-circuiting before `rhs` exists,
//! and the five steps of a call.

use std::cmp::Ordering;
use std::rc::Rc;

use crate::error::{Result, TreadleError, MAX_DEPTH};
use crate::front::ast::{BinOp, Expr, FnDecl, Stmt, UnOp};
use crate::output::Output;
use crate::tree::env::Env;
use crate::value::Value;

/// The tree-walking evaluator.
///
/// Holds only the recursion depth: variables live in [`Env`] and output in
/// [`Output`], both passed in. §6/`.36` counts **active invocations**, so the
/// top level is 0 and this is bumped at step (e) of a call, never by `Env`.
#[derive(Debug, Default)]
pub struct Eval {
    depth: usize,
}

impl Eval {
    pub fn new() -> Eval {
        Eval::default()
    }

    /// Evaluate one expression.
    ///
    /// Takes `out` because an expression can print: a `Call` runs a function
    /// body, and §3 requires the lines it produced before a later failure to
    /// survive in the `Output`. Nothing here buffers.
    pub fn eval(&mut self, e: &Expr, env: &mut Env, out: &mut Output) -> Result<Value> {
        match e {
            // The lexer already range-checked an integer literal (§6/`.41`), so
            // there is nothing to fail here.
            Expr::Lit(v) => Ok(v.clone()),
            // The VARIABLE namespace only (§6/`.42`): a function name used as a
            // value is `undefined variable`, which falls out of asking `env`.
            Expr::Var { name, line } => env.get(name, *line),
            Expr::Unary { op, rhs, line } => {
                let v = self.eval(rhs, env, out)?;
                match op {
                    UnOp::Neg => v.neg(*line),
                    // Bool-only: there is no truthiness (§6/`.40`), and `not`
                    // routes through the same `as_bool` gate as `if`/`while`.
                    UnOp::Not => v.not(*line),
                }
            }
            Expr::Binary { op, lhs, rhs, line } => self.binary(*op, lhs, rhs, *line, env, out),
            Expr::Call { name, args, line } => self.call(name, args, *line, env, out),
        }
    }

    /// A binary operator.
    ///
    /// **Order (§6/`.33`): `lhs` completely — side effects included — before
    /// `rhs` is touched.** That is observable: `print 1 % 0 + 1 / 0;` must be
    /// `modulo by zero`, and a callee that prints in `lhs` prints first.
    ///
    /// `and`/`or` are handled **before** `rhs` is evaluated at all (§2). The VM
    /// compiles them as a conditional jump over `rhs`; a tree-walker has to
    /// special-case the node, so this is one rule reached by two entirely
    /// different mechanisms — the likeliest place in the language for the
    /// engines to diverge, and the reason corpus `112`–`121` assert it with a
    /// callee that prints.
    fn binary(
        &mut self,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
        line: u32,
        env: &mut Env,
        out: &mut Output,
    ) -> Result<Value> {
        let l = self.eval(lhs, env, out)?;
        match op {
            BinOp::And | BinOp::Or => {
                // §6/`.40`: the LEFT operand must be Bool. The right is
                // type-checked only if it is evaluated, so `false and 1` is
                // `false` and `true or nil` is `true`.
                let left = l.as_bool(line)?;
                let decided = match op {
                    BinOp::And => !left,
                    _ => left,
                };
                if decided {
                    return Ok(Value::Bool(left));
                }
                let r = self.eval(rhs, env, out)?;
                Ok(Value::Bool(r.as_bool(line)?))
            }
            _ => {
                let r = self.eval(rhs, env, out)?;
                strict_binop(op, &l, &r, line)
            }
        }
    }

    /// A call, in exactly the five steps §6/`.35` pins:
    /// (a) arguments left to right, (b) resolve the name, (c) arity,
    /// (d) depth, (e) enter.
    ///
    /// `(a)` before `(b)` is not an accident — bytecode forces that order, so
    /// `nope(1 / 0)` is `divide by zero` and not `undefined function 'nope'`,
    /// and `two(shout(1))` prints before the arity complaint (corpus `314`).
    fn call(
        &mut self,
        name: &str,
        args: &[Expr],
        line: u32,
        env: &mut Env,
        out: &mut Output,
    ) -> Result<Value> {
        // (a) LEFT TO RIGHT, and each argument fully — including anything it
        // printed — before the next is started (§6/`.33`).
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            vals.push(self.eval(a, env, out)?);
        }

        // (b) resolve. Builtins FIRST: §6/`.42` reserves `len`/`str`/`int` as
        // function names in the parser, so they never reach `env`'s table and
        // `get_fn("len")` would answer `undefined function 'len'`. They are
        // still ordinary variable names — `let len = 1; print len;` prints 1,
        // which works because `Expr::Var` never consults this path.
        if let Some(v) = builtin(name, &vals, line)? {
            return Ok(v);
        }
        let decl = env.get_fn(name, line)?;

        // (c) arity. A `Type` error, not `Name` and not `Value` (§3).
        if decl.params.len() != vals.len() {
            return Err(TreadleError::wrong_arity(
                line,
                name,
                decl.params.len(),
                vals.len(),
            ));
        }

        // (d) depth, counted in ACTIVE INVOCATIONS with the top level at 0, so
        // 1000 nested invocations succeed and the 1001st fails at the CALL's
        // line (§6/`.36`). Checked before the callee's frame exists, and after
        // (a)–(c), so a bad argument or a wrong arity at depth 1000 still
        // reports its own error. Builtins returned above and cost no depth.
        if self.depth == MAX_DEPTH {
            return Err(TreadleError::recursion_limit(line));
        }

        // (e) enter.
        self.depth += 1;
        let result = self.invoke(&decl, vals, env, out);
        self.depth -= 1;
        result
    }

    /// Step (e): enter the callee, run its body, produce its value.
    ///
    /// The frame — not a plain scope — is what makes §2's "a function body's
    /// parent is the global scope, never the caller's" true, and `pop_frame`
    /// discards every scope the body pushed, so a `return` out of nested blocks
    /// needs no matching `pop_scope`s.
    fn invoke(
        &mut self,
        decl: &Rc<FnDecl>,
        args: Vec<Value>,
        env: &mut Env,
        out: &mut Output,
    ) -> Result<Value> {
        let frame = env.push_frame();
        // Arity is already checked, so `zip` drops nothing. Parameters are
        // ordinary locals of the body scope.
        for (p, a) in decl.params.iter().zip(args) {
            env.define(p, a);
        }
        let returned = self.exec_body(&decl.body, env, out);
        env.pop_frame(frame);
        // §2: a function that falls off the end returns Nil.
        returned.map(|v| v.unwrap_or(Value::Nil))
    }

    /// Execute a statement list, yielding `Some(v)` if it returned.
    ///
    /// **This is the seam, and bead `.20` (eval-stmt) owns the real version.**
    /// What is here executes only `print`, `return` and the no-op `fn`, which
    /// is the minimum that makes the expression-half tests below assert
    /// anything real: short-circuiting and argument order are only observable
    /// through a callee with a side effect, so without a callee that can
    /// `print` there is nothing to assert. Every other statement raises
    /// `internal` (§6/`.45`) rather than being silently skipped — a skipped
    /// `let` would make a half-built engine look green, while `internal error:`
    /// is greppable in CI and can never be mistaken for a language error.
    ///
    /// `.20` replaces this whole function; it should not try to keep the two
    /// arms below.
    fn exec_body(
        &mut self,
        body: &[Stmt],
        env: &mut Env,
        out: &mut Output,
    ) -> Result<Option<Value>> {
        for s in body {
            match s {
                Stmt::Print { args, .. } => {
                    // §6/`.33`: arguments left to right, and the line is
                    // appended only once every one of them succeeded — hence a
                    // slice of finished values and no partial line.
                    let mut vals = Vec::with_capacity(args.len());
                    for a in args {
                        vals.push(self.eval(a, env, out)?);
                    }
                    out.print(&vals);
                }
                Stmt::Return { value, .. } => {
                    let v = match value {
                        Some(e) => self.eval(e, env, out)?,
                        None => Value::Nil,
                    };
                    return Ok(Some(v));
                }
                // A declaration is not a runtime action: it is defined from
                // `Program::fns` before the first statement (see `ast.rs`).
                Stmt::Fn(_) => {}
                Stmt::Let { line, .. }
                | Stmt::Assign { line, .. }
                | Stmt::If { line, .. }
                | Stmt::While { line, .. } => {
                    return Err(TreadleError::internal(*line, "statement half is bead .20"));
                }
            }
        }
        Ok(None)
    }
}

/// Every binary operator except `and`/`or`, both operands already evaluated.
///
/// Not a method, and it takes no `Env`: by the time control is here nothing can
/// have a side effect, which is what keeps the order rules in one place. Every
/// arm delegates to `value.rs` — §6/`.39` gives it the semantics *and* the
/// message of every operator.
fn strict_binop(op: BinOp, l: &Value, r: &Value, line: u32) -> Result<Value> {
    match op {
        BinOp::Add => l.add(r, line),
        BinOp::Sub => l.sub(r, line),
        BinOp::Mul => l.mul(r, line),
        BinOp::Div => l.div(r, line),
        BinOp::Rem => l.rem(r, line),
        // `==` across two different types is a `Type` error, not `false`
        // (§6/`.40`). `value.rs` exposes one entry point, so `!=` is its
        // negation and the message names `==` even for `!=` (corpus `040`).
        BinOp::Eq => Ok(Value::Bool(l.eq_value(r, line)?)),
        BinOp::Ne => Ok(Value::Bool(!l.eq_value(r, line)?)),
        // The operator in a comparison's error message is chosen by the caller,
        // so it is the SOURCE spelling and not the `BinOp` variant name — two
        // engines printing `<` and `Lt` would be a divergence (corpus `030`).
        BinOp::Lt => cmp(l, r, "<", line, Ordering::is_lt),
        BinOp::Gt => cmp(l, r, ">", line, Ordering::is_gt),
        BinOp::Le => cmp(l, r, "<=", line, Ordering::is_le),
        BinOp::Ge => cmp(l, r, ">=", line, Ordering::is_ge),
        // Unreachable: `Eval::binary` handles both before `rhs` exists. §4
        // forbids a panic on any input, so a broken invariant is an `internal`
        // error (§6/`.45`) rather than an `unreachable!`.
        BinOp::And | BinOp::Or => Err(TreadleError::internal(
            line,
            "and/or reached the strict operator path",
        )),
    }
}

/// `<` `>` `<=` `>=`: `Int` with `Int` or `Str` with `Str` (bytewise), every
/// other pairing a `Type` error — all of that is `cmp_value`'s.
fn cmp(l: &Value, r: &Value, sym: &str, line: u32, want: fn(Ordering) -> bool) -> Result<Value> {
    Ok(Value::Bool(want(l.cmp_value(r, sym, line)?)))
}

/// The three builtins (§2), or `None` if `name` is not one of them so the caller
/// falls through to the function namespace.
///
/// They are deliberately *not* in `env`'s function table: declaring one is a
/// `Parse` error (§6/`.42`), so the front end has already made shadowing
/// impossible and this is the only place they exist.
///
/// `len(5)` and `int(5)` need a one-operand `Type` complaint that `error.rs`
/// has no builtin-specific constructor for, so they reuse
/// `unary_type_mismatch` — the same constructor unary `-` uses, giving
/// `len expects a Str operand, got Int`. §4 forbids the alternative (a
/// `format!` here), and the conformance corpus (`022`, `027`) already asserts
/// exactly those bytes.
fn builtin(name: &str, args: &[Value], line: u32) -> Result<Option<Value>> {
    if !matches!(name, "len" | "str" | "int") {
        return Ok(None);
    }
    // All three take exactly one argument; §3 files arity under `Type`.
    let [arg] = args else {
        return Err(TreadleError::wrong_arity(line, name, 1, args.len()));
    };
    let v = match name {
        // BYTES, not characters (§2), so a 2-byte `é` measures 2.
        "len" => match arg {
            Value::Str(s) => Value::Int(s.len() as i64),
            _ => {
                return Err(TreadleError::unary_type_mismatch(
                    line,
                    "len",
                    "Str",
                    arg.type_name(),
                ))
            }
        },
        // The §3 display form, which is `Display for Value` and nothing local.
        "str" => Value::str(arg.to_string()),
        // Exactly `s.parse::<i64>()` (§6/`.41`): a leading `+` and leading
        // zeros are accepted, surrounding whitespace is not, and out of range
        // is the same `bad_int` as non-numeric. `int` of a non-`Str` is a
        // `Type` error, not the identity on `Int`.
        "int" => match arg {
            Value::Str(s) => match s.parse::<i64>() {
                Ok(n) => Value::Int(n),
                Err(_) => return Err(TreadleError::bad_int(line, s)),
            },
            _ => {
                return Err(TreadleError::unary_type_mismatch(
                    line,
                    "int",
                    "Str",
                    arg.type_name(),
                ))
            }
        },
        // Every other name returned `None` above.
        _ => return Ok(None),
    };
    Ok(Some(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: u32 = 1;

    // ---- AST builders ----------------------------------------------------

    fn lit(v: Value) -> Expr {
        Expr::Lit(v)
    }

    fn int(n: i64) -> Expr {
        lit(Value::Int(n))
    }

    fn b(v: bool) -> Expr {
        lit(Value::Bool(v))
    }

    fn s(t: &str) -> Expr {
        lit(Value::str(t))
    }

    fn bin(op: BinOp, lhs: Expr, rhs: Expr) -> Expr {
        Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            line: L,
        }
    }

    fn un(op: UnOp, rhs: Expr) -> Expr {
        Expr::Unary {
            op,
            rhs: Box::new(rhs),
            line: L,
        }
    }

    fn call(name: &str, args: Vec<Expr>) -> Expr {
        Expr::Call {
            name: name.to_string(),
            args,
            line: L,
        }
    }

    fn var(name: &str) -> Expr {
        Expr::Var {
            name: name.to_string(),
            line: L,
        }
    }

    fn decl(name: &str, params: &[&str], body: Vec<Stmt>) -> Rc<FnDecl> {
        Rc::new(FnDecl {
            name: name.to_string(),
            params: params.iter().map(|p| p.to_string()).collect(),
            body,
            line: L,
        })
    }

    /// `fn <name>() { print "<msg>"; return <ret>; }` — the printing callee the
    /// short-circuit and order assertions need.
    fn printer(name: &str, msg: &str, ret: Expr) -> Rc<FnDecl> {
        decl(
            name,
            &[],
            vec![
                Stmt::Print {
                    args: vec![s(msg)],
                    line: L,
                },
                Stmt::Return {
                    value: Some(ret),
                    line: L,
                },
            ],
        )
    }

    // ---- harness ---------------------------------------------------------

    /// Evaluate in a fresh environment.
    fn ev(e: &Expr) -> Result<Value> {
        let mut env = Env::new();
        let mut out = Output::new();
        Eval::new().eval(e, &mut env, &mut out)
    }

    /// Evaluate against a prepared environment, returning what was printed
    /// alongside the result — the only way a side effect is assertable.
    fn ev_env(e: &Expr, env: &mut Env) -> (Result<Value>, Vec<String>) {
        let mut out = Output::new();
        let r = Eval::new().eval(e, env, &mut out);
        (r, out.lines)
    }

    fn err(e: &Expr) -> TreadleError {
        ev(e).expect_err("expected an error")
    }

    // ---- literals, variables, operators ----------------------------------

    #[test]
    fn literals_and_variables() {
        assert_eq!(ev(&int(7)).unwrap(), Value::Int(7));
        assert_eq!(ev(&lit(Value::Nil)).unwrap(), Value::Nil);
        assert_eq!(ev(&s("hi")).unwrap(), Value::str("hi"));

        let mut env = Env::new();
        env.define("x", Value::Int(4));
        assert_eq!(ev_env(&var("x"), &mut env).0.unwrap(), Value::Int(4));
        // A name that is only a function is `undefined variable` (§6/`.42`).
        env.define_fn(decl("f", &[], vec![]));
        assert_eq!(
            ev_env(&var("f"), &mut env).0.unwrap_err(),
            TreadleError::undefined_name(L, "f")
        );
    }

    /// Every one of the 13 `BinOp`s, on values that succeed. `Or`/`And` are
    /// here for their *value*; their short-circuiting is asserted below.
    #[test]
    fn every_binop_computes() {
        let cases: [(BinOp, Expr, Expr, Value); 13] = [
            (BinOp::Or, b(false), b(true), Value::Bool(true)),
            (BinOp::And, b(true), b(true), Value::Bool(true)),
            (BinOp::Eq, int(1), int(1), Value::Bool(true)),
            (BinOp::Ne, int(1), int(2), Value::Bool(true)),
            (BinOp::Lt, int(1), int(2), Value::Bool(true)),
            (BinOp::Gt, int(1), int(2), Value::Bool(false)),
            (BinOp::Le, int(2), int(2), Value::Bool(true)),
            (BinOp::Ge, int(3), int(4), Value::Bool(false)),
            (BinOp::Add, int(2), int(3), Value::Int(5)),
            (BinOp::Sub, int(1), int(2), Value::Int(-1)),
            (BinOp::Mul, int(6), int(7), Value::Int(42)),
            (BinOp::Div, int(-7), int(2), Value::Int(-3)),
            (BinOp::Rem, int(-7), int(2), Value::Int(-1)),
        ];
        assert_eq!(cases.len(), 13, "one case per BinOp variant");
        for (op, l, r, want) in cases {
            let got = ev(&bin(op, l, r)).unwrap_or_else(|e| panic!("{op:?}: {e}"));
            assert_eq!(got, want, "{op:?}");
        }
        // `+` also concatenates `Str`, and `Str` orders bytewise, so a prefix
        // sorts before the string it prefixes.
        assert_eq!(
            ev(&bin(BinOp::Add, s("ab"), s("cd"))).unwrap(),
            Value::str("abcd")
        );
        assert_eq!(
            ev(&bin(BinOp::Lt, s("ab"), s("b"))).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            ev(&bin(BinOp::Eq, lit(Value::Nil), lit(Value::Nil))).unwrap(),
            Value::Bool(true)
        );
    }

    /// Both `UnOp`s, and the fact that they bind to their own operand.
    #[test]
    fn both_unops() {
        assert_eq!(ev(&un(UnOp::Neg, int(2))).unwrap(), Value::Int(-2));
        assert_eq!(ev(&un(UnOp::Not, b(false))).unwrap(), Value::Bool(true));
        // `-2 + 3` is `(-2) + 3`, which is the tree the parser hands us.
        assert_eq!(
            ev(&bin(BinOp::Add, un(UnOp::Neg, int(2)), int(3))).unwrap(),
            Value::Int(1)
        );
    }

    // ---- type errors -----------------------------------------------------

    /// Type errors come from `value.rs`'s wording via `error.rs`'s
    /// constructors, asserted as whole errors rather than by variant — the
    /// message text is a contract (§4). `Nil` operands are covered on purpose:
    /// there is no coercion anywhere, so `nil` is a type like any other.
    #[test]
    fn type_errors_are_value_rs_wording() {
        assert_eq!(
            err(&bin(BinOp::Add, int(1), b(true))),
            TreadleError::type_mismatch(L, "+", "Int or Str", "Int", "Bool")
        );
        assert_eq!(
            err(&bin(BinOp::Sub, lit(Value::Nil), int(1))),
            TreadleError::type_mismatch(L, "-", "Int", "Nil", "Int")
        );
        assert_eq!(
            err(&bin(BinOp::Mul, s("a"), s("b"))),
            TreadleError::type_mismatch(L, "*", "Int", "Str", "Str")
        );
        assert_eq!(
            err(&bin(BinOp::Div, lit(Value::Nil), lit(Value::Nil))),
            TreadleError::type_mismatch(L, "/", "Int", "Nil", "Nil")
        );
        assert_eq!(
            err(&bin(BinOp::Rem, b(true), int(1))),
            TreadleError::type_mismatch(L, "%", "Int", "Bool", "Int")
        );
        // `==`/`!=` across types is an error, not `false`, and `!=` reports
        // `==` because `value.rs` has one entry point (corpus `040`).
        assert_eq!(
            err(&bin(BinOp::Eq, int(1), s("1"))),
            TreadleError::eq_type_mismatch(L, "Int", "Str")
        );
        assert_eq!(
            err(&bin(BinOp::Ne, int(1), s("1"))),
            TreadleError::eq_type_mismatch(L, "Int", "Str")
        );
        // Ordering names the operator's SOURCE spelling, not `Lt`/`Ge`.
        for (op, sym) in [
            (BinOp::Lt, "<"),
            (BinOp::Gt, ">"),
            (BinOp::Le, "<="),
            (BinOp::Ge, ">="),
        ] {
            assert_eq!(
                err(&bin(op, int(1), s("a"))),
                TreadleError::type_mismatch(L, sym, "two Int or two Str", "Int", "Str"),
                "{op:?}"
            );
            assert_eq!(
                err(&bin(op, lit(Value::Nil), lit(Value::Nil))),
                TreadleError::type_mismatch(L, sym, "two Int or two Str", "Nil", "Nil"),
                "{op:?}"
            );
        }
        // Unary, and the Bool gate, which names no operator.
        assert_eq!(
            err(&un(UnOp::Neg, b(true))),
            TreadleError::unary_type_mismatch(L, "-", "Int", "Bool")
        );
        assert_eq!(
            err(&un(UnOp::Not, int(1))),
            TreadleError::not_bool(L, "Int")
        );
        assert_eq!(
            err(&un(UnOp::Not, lit(Value::Nil))),
            TreadleError::not_bool(L, "Nil")
        );
        // §6/`.40`: no truthiness, so the LEFT operand of and/or must be Bool.
        assert_eq!(
            err(&bin(BinOp::And, int(0), b(true))),
            TreadleError::not_bool(L, "Int")
        );
        assert_eq!(
            err(&bin(BinOp::Or, s(""), b(true))),
            TreadleError::not_bool(L, "Str")
        );
    }

    #[test]
    fn overflow_and_zero_divisors_surface_the_right_constructor() {
        assert_eq!(
            err(&bin(BinOp::Add, int(i64::MAX), int(1))),
            TreadleError::overflow(L)
        );
        assert_eq!(
            err(&bin(BinOp::Sub, int(i64::MIN), int(1))),
            TreadleError::overflow(L)
        );
        assert_eq!(
            err(&bin(BinOp::Mul, int(i64::MAX), int(2))),
            TreadleError::overflow(L)
        );
        assert_eq!(
            err(&un(UnOp::Neg, int(i64::MIN))),
            TreadleError::overflow(L)
        );
        // §6/`.41`: these two would abort the process under a bare `/` or `%`.
        assert_eq!(
            err(&bin(BinOp::Div, int(i64::MIN), int(-1))),
            TreadleError::overflow(L)
        );
        assert_eq!(
            err(&bin(BinOp::Rem, int(i64::MIN), int(-1))),
            TreadleError::overflow(L)
        );
        assert_eq!(
            err(&bin(BinOp::Div, int(1), int(0))),
            TreadleError::divide_by_zero(L)
        );
        assert_eq!(
            err(&bin(BinOp::Rem, int(1), int(0))),
            TreadleError::modulo_by_zero(L)
        );
    }

    // ---- order, which is the part only this module can get wrong ----------

    /// §2 short-circuit, **and-form, with an observable side effect**: the
    /// callee prints, so `false and f()` proves non-evaluation by the ABSENCE
    /// of its line — not merely by the absence of a type error.
    #[test]
    fn and_short_circuits_with_an_observable_side_effect() {
        let mut env = Env::new();
        env.define_fn(printer("f", "f called", b(true)));

        let (r, printed) = ev_env(&bin(BinOp::And, b(false), call("f", vec![])), &mut env);
        assert_eq!(r.unwrap(), Value::Bool(false));
        assert!(printed.is_empty(), "rhs was evaluated: {printed:?}");

        // The control: with a true lhs the rhs IS evaluated, and its line
        // lands before anything the enclosing expression does.
        let (r, printed) = ev_env(&bin(BinOp::And, b(true), call("f", vec![])), &mut env);
        assert_eq!(r.unwrap(), Value::Bool(true));
        assert_eq!(printed, vec!["f called".to_string()]);
    }

    /// The or-form of the same assertion.
    #[test]
    fn or_short_circuits_with_an_observable_side_effect() {
        let mut env = Env::new();
        env.define_fn(printer("f", "f called", b(true)));

        let (r, printed) = ev_env(&bin(BinOp::Or, b(true), call("f", vec![])), &mut env);
        assert_eq!(r.unwrap(), Value::Bool(true));
        assert!(printed.is_empty(), "rhs was evaluated: {printed:?}");

        let (r, printed) = ev_env(&bin(BinOp::Or, b(false), call("f", vec![])), &mut env);
        assert_eq!(r.unwrap(), Value::Bool(true));
        assert_eq!(printed, vec!["f called".to_string()]);
    }

    /// §6/`.40`: a skipped `rhs` is not type-checked and cannot fail at all —
    /// not even with an error of a different variant.
    #[test]
    fn a_skipped_rhs_is_neither_typed_nor_run() {
        assert_eq!(
            ev(&bin(BinOp::And, b(false), int(1))).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            ev(&bin(BinOp::Or, b(true), lit(Value::Nil))).unwrap(),
            Value::Bool(true)
        );
        // A Value error and a Name error, both unreachable.
        assert_eq!(
            ev(&bin(BinOp::And, b(false), bin(BinOp::Div, int(1), int(0)))).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            ev(&bin(BinOp::Or, b(true), call("nope", vec![]))).unwrap(),
            Value::Bool(true)
        );
        // A chain stops at the first operand that decides.
        let chain = bin(
            BinOp::And,
            bin(BinOp::And, b(false), call("nope", vec![])),
            call("nope", vec![]),
        );
        assert_eq!(ev(&chain).unwrap(), Value::Bool(false));
    }

    /// §6/`.33`: `lhs` completely before `rhs` is touched. Both sides fail, so
    /// the error that escapes names the order (corpus `019`).
    #[test]
    fn binary_operands_evaluate_left_to_right() {
        let e = bin(
            BinOp::Add,
            bin(BinOp::Rem, int(1), int(0)),
            bin(BinOp::Div, int(1), int(0)),
        );
        assert_eq!(err(&e), TreadleError::modulo_by_zero(L));

        // And with side effects rather than errors: lhs prints first.
        let mut env = Env::new();
        env.define_fn(printer("a", "a", b(true)));
        env.define_fn(printer("z", "z", b(true)));
        let (r, printed) = ev_env(
            &bin(BinOp::And, call("a", vec![]), call("z", vec![])),
            &mut env,
        );
        assert_eq!(r.unwrap(), Value::Bool(true));
        assert_eq!(printed, vec!["a".to_string(), "z".to_string()]);
    }

    /// §6/`.33`: call arguments left to right, each one complete before the
    /// next starts. `two(shout(1), shout(2))` prints `1` then `2`.
    #[test]
    fn call_arguments_evaluate_left_to_right() {
        let mut env = Env::new();
        env.define_fn(decl(
            "shout",
            &["v"],
            vec![
                Stmt::Print {
                    args: vec![var("v")],
                    line: L,
                },
                Stmt::Return {
                    value: Some(var("v")),
                    line: L,
                },
            ],
        ));
        env.define_fn(decl(
            "two",
            &["a", "b"],
            vec![Stmt::Return {
                value: Some(bin(BinOp::Add, var("a"), var("b"))),
                line: L,
            }],
        ));

        let e = call(
            "two",
            vec![call("shout", vec![int(1)]), call("shout", vec![int(2)])],
        );
        let (r, printed) = ev_env(&e, &mut env);
        assert_eq!(r.unwrap(), Value::Int(3));
        assert_eq!(printed, vec!["1".to_string(), "2".to_string()]);
    }

    /// §6/`.35`: arguments (a) run before the name (b) and the arity (c) are
    /// checked, so a side effect in an argument survives both errors.
    #[test]
    fn arguments_run_before_the_name_and_arity_are_checked() {
        // `nope(1 / 0)` is `divide by zero`, not `undefined function`.
        assert_eq!(
            err(&call("nope", vec![bin(BinOp::Div, int(1), int(0))])),
            TreadleError::divide_by_zero(L)
        );
        assert_eq!(
            err(&call("nope", vec![])),
            TreadleError::undefined_function(L, "nope")
        );

        let mut env = Env::new();
        env.define_fn(printer("shout", "shouted", b(true)));
        env.define_fn(decl("two", &["a", "b"], vec![]));
        let (r, printed) = ev_env(&call("two", vec![call("shout", vec![])]), &mut env);
        assert_eq!(r.unwrap_err(), TreadleError::wrong_arity(L, "two", 2, 1));
        assert_eq!(printed, vec!["shouted".to_string()]);
    }

    // ---- calls -----------------------------------------------------------

    #[test]
    fn a_body_with_no_return_yields_nil() {
        let mut env = Env::new();
        env.define_fn(decl("f", &[], vec![]));
        assert_eq!(ev_env(&call("f", vec![]), &mut env).0.unwrap(), Value::Nil);
    }

    /// A variable is not callable and a function is not a value (§6/`.42`).
    #[test]
    fn the_two_namespaces_do_not_mix() {
        let mut env = Env::new();
        env.define("x", Value::Int(1));
        assert_eq!(
            ev_env(&call("x", vec![]), &mut env).0.unwrap_err(),
            TreadleError::undefined_function(L, "x")
        );
    }

    /// §6/`.36`: the counted quantity is active invocations, the top level is
    /// 0, and the check is `depth == MAX_DEPTH` at the call site — so 1000
    /// nested invocations succeed and the 1001st fails, at the CALL's line.
    /// Run on a bigger stack because the tree-walker recurses in Rust; the
    /// limit is observable and may not be tuned down to fit a test.
    #[test]
    fn recursion_stops_at_max_depth_not_at_a_stack_overflow() {
        let got = std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(|| {
                let mut env = Env::new();
                // `fn r() { return r(); }` — unbounded, so only the limit stops it.
                env.define_fn(decl(
                    "r",
                    &[],
                    vec![Stmt::Return {
                        value: Some(Expr::Call {
                            name: "r".to_string(),
                            args: vec![],
                            line: 9,
                        }),
                        line: 9,
                    }],
                ));
                let mut out = Output::new();
                // `Value` holds an `Rc` and is not `Send`, so drop the (never
                // reached) success value; `TreadleError` is plain data.
                Eval::new()
                    .eval(&call("r", vec![]), &mut env, &mut out)
                    .map(|_| ())
            })
            .expect("spawn")
            .join()
            .expect("the limit must fire as an error, never as a stack overflow");
        // Line 9 is the failing call inside the body, not the top-level call.
        assert_eq!(got.unwrap_err(), TreadleError::recursion_limit(9));
    }

    // ---- builtins --------------------------------------------------------

    #[test]
    fn len_counts_bytes_and_rejects_non_strings() {
        assert_eq!(ev(&call("len", vec![s("")])).unwrap(), Value::Int(0));
        assert_eq!(ev(&call("len", vec![s("abc")])).unwrap(), Value::Int(3));
        // 2 UTF-8 bytes, not 1 character.
        assert_eq!(ev(&call("len", vec![s("é")])).unwrap(), Value::Int(2));
        assert_eq!(ev(&call("len", vec![s("🎉")])).unwrap(), Value::Int(4));
        assert_eq!(
            err(&call("len", vec![int(5)])),
            TreadleError::unary_type_mismatch(L, "len", "Str", "Int")
        );
        assert_eq!(
            err(&call("len", vec![s("a"), s("b")])),
            TreadleError::wrong_arity(L, "len", 1, 2)
        );
    }

    #[test]
    fn str_is_the_one_display_form_and_never_fails_on_a_value() {
        for (v, want) in [
            (Value::Nil, "nil"),
            (Value::Int(-42), "-42"),
            (Value::Bool(true), "true"),
            (Value::Bool(false), "false"),
            (Value::str("abc"), "abc"),
        ] {
            assert_eq!(ev(&call("str", vec![lit(v)])).unwrap(), Value::str(want));
        }
        // The result really is a `Str`: it concatenates and `len` measures it.
        assert_eq!(
            ev(&bin(BinOp::Add, call("str", vec![lit(Value::Nil)]), s("!"))).unwrap(),
            Value::str("nil!")
        );
        assert_eq!(
            err(&call("str", vec![])),
            TreadleError::wrong_arity(L, "str", 1, 0)
        );
    }

    #[test]
    fn int_is_exactly_parse_i64() {
        for (t, n) in [
            ("42", 42),
            ("-5", -5),
            ("+1", 1),
            ("007", 7),
            ("0", 0),
            ("9223372036854775807", i64::MAX),
            ("-9223372036854775808", i64::MIN),
        ] {
            assert_eq!(ev(&call("int", vec![s(t)])).unwrap(), Value::Int(n), "{t}");
        }
        // Non-numeric, out of range and untrimmed all share `bad_int`, which
        // interpolates the string as given — hence the two spaces in `025`.
        for t in ["abc", "9223372036854775808", " 1", ""] {
            assert_eq!(
                err(&call("int", vec![s(t)])),
                TreadleError::bad_int(L, t),
                "{t:?}"
            );
        }
        // Not the identity on `Int` (§6/`.41`).
        assert_eq!(
            err(&call("int", vec![int(5)])),
            TreadleError::unary_type_mismatch(L, "int", "Str", "Int")
        );
        assert_eq!(
            err(&call("int", vec![lit(Value::Nil)])),
            TreadleError::unary_type_mismatch(L, "int", "Str", "Nil")
        );
        assert_eq!(
            err(&call("int", vec![s("1"), s("2")])),
            TreadleError::wrong_arity(L, "int", 1, 2)
        );
    }

    /// A builtin name is reserved as a FUNCTION name only, so it is an ordinary
    /// variable: `let len = 1; print len;` prints 1 (§6/`.42`).
    #[test]
    fn builtin_names_are_still_ordinary_variables() {
        let mut env = Env::new();
        for name in ["len", "str", "int"] {
            env.define(name, Value::Int(1));
            assert_eq!(ev_env(&var(name), &mut env).0.unwrap(), Value::Int(1));
            // …and calling it still reaches the builtin, not the variable.
            // `"12"` is the one argument all three accept.
            assert!(
                ev_env(&call(name, vec![s("12")]), &mut env).0.is_ok(),
                "{name} stopped being callable"
            );
        }
    }

    // ---- `let x = x + 1`, both forms (§6/`.37`) ---------------------------

    /// The order `env::define` makes the only expressible one: the initialiser
    /// is evaluated in the scope as it exists BEFORE the new binding, so with
    /// an outer `x` at 1 a shadowing `let x = x + 1;` binds 2 and leaves the
    /// outer one at 1.
    #[test]
    fn let_x_equals_x_plus_one_reads_the_scope_before_the_binding() {
        let mut env = Env::new();
        env.define("x", Value::Int(1));
        env.push_scope();

        let init = bin(BinOp::Add, var("x"), int(1));
        let v = ev_env(&init, &mut env).0.unwrap(); // FIRST
        env.define("x", v); // THEN
        assert_eq!(env.get("x", L).unwrap(), Value::Int(2));

        env.pop_scope();
        assert_eq!(env.get("x", L).unwrap(), Value::Int(1));

        // Re-declaring in the SAME scope is legal and the later `let` wins.
        let v = ev_env(&init, &mut env).0.unwrap();
        env.define("x", v);
        assert_eq!(env.get("x", L).unwrap(), Value::Int(2));
    }

    /// The second form: with no outer `x` anywhere, the initialiser's own
    /// lookup is a `Name` error at its line, before any binding exists.
    #[test]
    fn let_x_equals_x_plus_one_with_no_outer_x_is_a_name_error() {
        let mut env = Env::new();
        env.push_scope();
        let init = Expr::Binary {
            op: BinOp::Add,
            lhs: Box::new(Expr::Var {
                name: "x".to_string(),
                line: 5,
            }),
            rhs: Box::new(int(1)),
            line: 5,
        };
        assert_eq!(
            ev_env(&init, &mut env).0.unwrap_err(),
            TreadleError::undefined_name(5, "x")
        );
        assert!(env.get("x", L).is_err(), "a failed initialiser bound x");
    }

    // ---- §4: no panic, and no wording of our own --------------------------

    /// The `internal` guard on the strict path is unreachable through `eval`
    /// but must not be a panic (§4). Asserted directly, since no program can
    /// get there.
    #[test]
    fn and_or_never_reach_the_strict_operator_path() {
        for op in [BinOp::And, BinOp::Or] {
            let got = strict_binop(op, &Value::Bool(true), &Value::Bool(true), L);
            assert_eq!(
                got.unwrap_err(),
                TreadleError::internal(L, "and/or reached the strict operator path")
            );
        }
        // Reached through `eval`, the same node is a plain Bool result.
        assert_eq!(
            ev(&bin(BinOp::And, b(true), b(false))).unwrap(),
            Value::Bool(false)
        );
    }
}
