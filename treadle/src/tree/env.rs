//! bead: treadle-env — scope chain, shadowing, globals
//!
//! The variable environment of the tree-walking engine, plus the *separate*
//! function namespace (§6, bead `.42`).
//!
//! # The shape, and why it is this shape
//!
//! ```text
//!   globals            <- top-level `let`s AND the hoisted functions
//!     ^
//!     |  (the only parent link a function body has)
//!     |
//!   locals[frame_base .. ]   <- the current invocation's scopes, innermost last
//!   locals[.. frame_base]    <- the CALLERS' scopes: present, but INVISIBLE
//! ```
//!
//! §2 has no closures, so **a function body's scope parent is the global scope,
//! never the caller's**. That is enforced structurally here rather than
//! remembered at each lookup: [`Env::push_frame`] records the current top of
//! `locals` as `frame_base`, and every walk stops there before falling through
//! to `globals`. A caller's locals are therefore unreachable by construction,
//! which is the single most expensive thing to get wrong — every recursive
//! program would diverge from the VM.
//!
//! §6 (`.37`): the top-level scope **is** the global scope, so a top-level
//! `let` is a global that every function body sees. A `let` in a *block* at top
//! level is not, and a function body cannot create a global.
//!
//! # Memory model (§0 asks for it in a comment)
//!
//! Values are cloned on read; `Value::Str` is an `Rc<String>`, so a clone never
//! copies string bytes. `FnDecl`s are shared as `Rc`. No GC, none needed.
//!
//! # Determinism
//!
//! `HashMap` is used for scopes, but nothing here ever *iterates* one — only
//! `get`/`insert` by name — so §2's "no hash-order iteration in any output
//! path" holds.

use std::collections::HashMap;
use std::rc::Rc;

use crate::error::{Result, TreadleError};
use crate::front::ast::FnDecl;
use crate::value::Value;

/// The saved frame boundary handed back by [`Env::push_frame`]. Pass it to
/// [`Env::pop_frame`] to leave the invocation. Opaque so a caller cannot invent
/// one.
#[must_use = "a pushed frame must be popped with Env::pop_frame"]
#[derive(Debug)]
pub struct Frame(usize);

/// The variable scope chain plus the function namespace.
#[derive(Debug, Default)]
pub struct Env {
    /// Global scope: top-level `let`s. Also the parent of every function body.
    globals: HashMap<String, Value>,
    /// Block and function-body scopes, innermost last. Entries below
    /// `frame_base` belong to callers and are not visible.
    locals: Vec<HashMap<String, Value>>,
    /// Index into `locals`: the first scope belonging to the current
    /// invocation. 0 at top level.
    frame_base: usize,
    /// The function namespace — separate from variables (§6 `.42`), so
    /// `let f = 1; fn f() {...}` is legal, `print f;` prints 1 and `f()` calls
    /// the function.
    fns: HashMap<String, Rc<FnDecl>>,
}

impl Env {
    pub fn new() -> Env {
        Env::default()
    }

    // ---- variables -------------------------------------------------------

    /// `let name = value;` — bind in the **current** scope, shadowing any outer
    /// binding of the same name for the rest of that scope.
    ///
    /// This takes an already-evaluated [`Value`], not an initialiser
    /// expression, and that is deliberate: §6 (`.37`) pins the initialiser as
    /// being evaluated in the scope **as it exists before** the new binding is
    /// created. Because `define` cannot evaluate anything, the wrong order is
    /// not expressible — the caller must `eval(init)?` first and pass the
    /// result. With an outer `x == 1`, `let x = x + 1;` therefore binds a new
    /// `x == 2` and leaves the outer one untouched; with no outer `x` at all,
    /// the `get` inside the initialiser is a `Name` error before `define` is
    /// ever reached.
    ///
    /// Re-declaring a name already bound in the same scope is **legal** (§6
    /// `.37`) and the later `let` wins for the rest of that scope: this is a
    /// plain overwrite.
    pub fn define(&mut self, name: &str, value: Value) {
        // Inside an invocation, `push_frame` always pushed a scope, so
        // `locals.len() > frame_base` — a function body can never fall through
        // to `globals` and create a global (§6 `.37`).
        debug_assert!(self.frame_base == 0 || self.locals.len() > self.frame_base);
        let scope = if self.locals.len() > self.frame_base {
            self.locals.last_mut().expect("len > frame_base >= 0")
        } else {
            &mut self.globals
        };
        scope.insert(name.to_string(), value);
    }

    /// Read a variable: walk **outward** from the innermost visible scope, then
    /// the globals. A `Name` error if nothing binds it.
    ///
    /// A function or builtin name used where a value is expected resolves here,
    /// in the variable namespace only, finds nothing, and so is
    /// `undefined variable '<name>'` (§6 `.42` D).
    pub fn get(&self, name: &str, line: u32) -> Result<Value> {
        self.visible()
            .find_map(|scope| scope.get(name))
            .cloned()
            .ok_or_else(|| TreadleError::undefined_name(line, name))
    }

    /// `name = value;` — walk **outward** to the nearest existing binding and
    /// overwrite it. A `Name` error if there is none: assignment never creates
    /// a binding (§2). From a function body the walk reaches the globals but
    /// never the caller, so `fn f() { g = 1; }` with no global `g` is a
    /// `Name` error on every call (§6 `.37`).
    pub fn assign(&mut self, name: &str, value: Value, line: u32) -> Result<()> {
        let base = self.frame_base;
        let slot = self.locals[base..]
            .iter_mut()
            .rev()
            .chain(std::iter::once(&mut self.globals))
            .find_map(|scope| scope.get_mut(name));
        match slot {
            Some(slot) => {
                *slot = value;
                Ok(())
            }
            None => Err(TreadleError::assign_unbound(line, name)),
        }
    }

    /// Enter a block `{ ... }`.
    pub fn push_scope(&mut self) {
        self.locals.push(HashMap::new());
    }

    /// Leave a block, discarding its bindings — so an outer binding shadowed
    /// inside it is visible again with its original value.
    pub fn pop_scope(&mut self) {
        debug_assert!(
            self.locals.len() > self.frame_base,
            "pop_scope would leave the current invocation's frame"
        );
        self.locals.pop();
    }

    /// Enter a function body: push its scope and hide the caller's locals. The
    /// new scope's only parent is the global scope (§2, no closures).
    ///
    /// Define the parameters with [`Env::define`] after this, then execute the
    /// body. Depth counting is the caller's job (§6 `.36`).
    pub fn push_frame(&mut self) -> Frame {
        let saved = Frame(self.frame_base);
        self.frame_base = self.locals.len();
        self.locals.push(HashMap::new());
        saved
    }

    /// Leave a function body, discarding every scope it pushed (a `return` out
    /// of nested blocks does not pop them one by one) and restoring the
    /// caller's view.
    pub fn pop_frame(&mut self, frame: Frame) {
        self.locals.truncate(self.frame_base);
        self.frame_base = frame.0;
    }

    /// Innermost-first iterator over the scopes a lookup may see: the current
    /// invocation's scopes, then the globals. Never a caller's scope.
    fn visible(&self) -> impl Iterator<Item = &HashMap<String, Value>> {
        self.locals[self.frame_base..]
            .iter()
            .rev()
            .chain(std::iter::once(&self.globals))
    }

    // ---- functions -------------------------------------------------------

    /// Define a hoisted function. Call once per entry of `Program::fns` before
    /// executing any statement, and treat `Stmt::Fn` as a no-op (see
    /// `ast.rs`).
    ///
    /// Duplicate names are a `Parse` error in the shared front end (§6 `.42`
    /// B), so `fns` never contains two entries with one name and this overwrite
    /// is unreachable. The three builtins are reserved function names, also in
    /// the parser, so they cannot be shadowed here either — which is why this
    /// table does not know about them at all: `eval` checks builtins first.
    pub fn define_fn(&mut self, decl: Rc<FnDecl>) {
        self.fns.insert(decl.name.clone(), decl);
    }

    /// Resolve a call in the **function** namespace. A variable is not
    /// callable, so a `let`-bound name reaching here is
    /// `undefined function '<name>'` (§6 `.42` D).
    pub fn get_fn(&self, name: &str, line: u32) -> Result<Rc<FnDecl>> {
        self.fns
            .get(name)
            .cloned()
            .ok_or_else(|| TreadleError::undefined_function(line, name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: u32 = 1;

    fn decl(name: &str) -> Rc<FnDecl> {
        Rc::new(FnDecl {
            name: name.to_string(),
            params: vec![],
            body: vec![],
            line: L,
        })
    }

    fn int(n: i64) -> Value {
        Value::Int(n)
    }

    /// §6 `.37`: the top-level scope IS the global scope, and a `let` in a
    /// block at top level is not global.
    #[test]
    fn top_level_let_is_global_but_a_block_let_is_not() {
        let mut e = Env::new();
        e.define("g", int(1));
        e.push_scope();
        e.define("b", int(2));
        assert_eq!(e.get("g", L).unwrap(), int(1));
        assert_eq!(e.get("b", L).unwrap(), int(2));
        e.pop_scope();
        assert_eq!(e.get("g", L).unwrap(), int(1));
        assert!(e.get("b", L).is_err());

        // `g` really landed in the globals: a function body sees it.
        let f = e.push_frame();
        assert_eq!(e.get("g", L).unwrap(), int(1));
        e.pop_frame(f);
    }

    /// §2: `let` shadows an outer binding for the rest of the scope, and the
    /// outer value is back after `pop_scope`.
    #[test]
    fn nested_block_shadowing_restores_the_outer_value() {
        let mut e = Env::new();
        e.define("x", int(1));
        e.push_scope();
        e.define("x", int(2));
        assert_eq!(e.get("x", L).unwrap(), int(2));
        e.push_scope();
        e.define("x", int(3));
        assert_eq!(e.get("x", L).unwrap(), int(3));
        e.pop_scope();
        assert_eq!(e.get("x", L).unwrap(), int(2));
        e.pop_scope();
        assert_eq!(e.get("x", L).unwrap(), int(1));
    }

    /// §2: assignment walks outward to the nearest existing binding and writes
    /// THAT one — it does not create a local.
    #[test]
    fn assignment_reaches_an_outer_binding() {
        let mut e = Env::new();
        e.define("x", int(1));
        e.push_scope();
        e.assign("x", int(9), L).unwrap();
        assert_eq!(e.get("x", L).unwrap(), int(9));
        e.pop_scope();
        assert_eq!(e.get("x", L).unwrap(), int(9));
    }

    /// Assignment writes the NEAREST binding, so a shadow absorbs it and the
    /// outer one is untouched.
    #[test]
    fn assignment_writes_the_nearest_binding_only() {
        let mut e = Env::new();
        e.define("x", int(1));
        e.push_scope();
        e.define("x", int(2));
        e.assign("x", int(9), L).unwrap();
        assert_eq!(e.get("x", L).unwrap(), int(9));
        e.pop_scope();
        assert_eq!(e.get("x", L).unwrap(), int(1));
    }

    /// §2: assignment with no binding anywhere is a runtime `Name` error, with
    /// its own wording, distinct from a failed read.
    #[test]
    fn assignment_to_an_unbound_name_errors() {
        let mut e = Env::new();
        assert_eq!(
            e.assign("y", int(1), 7).unwrap_err(),
            TreadleError::assign_unbound(7, "y")
        );
        assert_eq!(
            e.get("y", 7).unwrap_err(),
            TreadleError::undefined_name(7, "y")
        );
        // Failing to assign did not bind anything.
        assert!(e.get("y", L).is_err());
    }

    /// §2/§6 `.37`: a function body's parent is the GLOBAL scope, never the
    /// caller's. The caller here holds a local `secret` and a *block* local
    /// `blocky`; neither is visible inside the body, while the global is.
    ///
    /// This is the case that only passes if the chain stops at the frame: a
    /// naive `Vec<HashMap>` walked to the bottom would find both.
    #[test]
    fn a_function_body_cannot_see_caller_locals() {
        let mut e = Env::new();
        e.define("g", int(1)); // global
        let caller = e.push_frame(); // the caller is itself a function
        e.define("secret", int(2)); // its parameter/local
        e.push_scope();
        e.define("blocky", int(3)); // a local in a block inside it
        assert_eq!(e.get("secret", L).unwrap(), int(2));

        let callee = e.push_frame();
        assert_eq!(e.get("g", L).unwrap(), int(1));
        assert!(e.get("secret", L).is_err());
        assert!(e.get("blocky", L).is_err());
        // Nor can it ASSIGN to one: the outward walk stops at the frame too.
        assert!(e.assign("secret", int(0), L).is_err());
        // It can assign a global, which is the only outward reach it has.
        e.assign("g", int(4), L).unwrap();
        e.pop_frame(callee);

        // The caller's view is intact, and it sees the global write.
        assert_eq!(e.get("secret", L).unwrap(), int(2));
        assert_eq!(e.get("blocky", L).unwrap(), int(3));
        assert_eq!(e.get("g", L).unwrap(), int(4));
        e.pop_scope();
        e.pop_frame(caller);
        assert_eq!(e.get("g", L).unwrap(), int(4));
    }

    /// §6 `.37`: a function body cannot create a global — its `let` binds in
    /// the function scope and is gone when the call returns.
    #[test]
    fn a_function_body_cannot_create_a_global() {
        let mut e = Env::new();
        let f = e.push_frame();
        e.define("local", int(1));
        assert_eq!(e.get("local", L).unwrap(), int(1));
        e.pop_frame(f);
        assert!(e.get("local", L).is_err());
    }

    /// A `return` out of nested blocks does not unwind scope by scope, so
    /// `pop_frame` must discard every scope the body pushed.
    #[test]
    fn pop_frame_discards_nested_block_scopes() {
        let mut e = Env::new();
        e.define("g", int(1));
        let f = e.push_frame();
        e.push_scope();
        e.push_scope();
        e.define("deep", int(2));
        e.pop_frame(f); // as if a `return` fired from the inner block
        assert!(e.get("deep", L).is_err());
        assert_eq!(e.get("g", L).unwrap(), int(1));
        // Back at top level, so a `let` is global again.
        e.define("g2", int(3));
        let g = e.push_frame();
        assert_eq!(e.get("g2", L).unwrap(), int(3));
        e.pop_frame(g);
    }

    /// §6 `.37` A: `let x = x + 1` with an outer `x` reads the OUTER `x` and
    /// leaves it untouched — because the caller evaluates the initialiser
    /// before calling `define`, which is the only order this API allows.
    #[test]
    fn let_x_equals_x_plus_one_reads_the_outer_x() {
        let mut e = Env::new();
        e.define("x", int(1));
        e.push_scope();
        let init = e.get("x", L).unwrap().add(&int(1), L).unwrap(); // evaluated FIRST
        e.define("x", init);
        assert_eq!(e.get("x", L).unwrap(), int(2));
        e.pop_scope();
        assert_eq!(e.get("x", L).unwrap(), int(1));
    }

    /// §6 `.37` A, second form: with no outer `x` anywhere it is a runtime
    /// `Name` error at that line, raised by the initialiser's `get`.
    #[test]
    fn let_x_equals_x_plus_one_with_no_outer_x_is_a_name_error() {
        let mut e = Env::new();
        e.push_scope();
        assert_eq!(
            e.get("x", 5).unwrap_err(),
            TreadleError::undefined_name(5, "x")
        );
    }

    /// §6 `.37` B: re-declaring in the SAME scope is legal and the later `let`
    /// wins. `let x = 1; let x = x + 1; print x;` prints 2.
    #[test]
    fn re_declaration_in_the_same_scope_is_legal_and_the_later_let_wins() {
        let mut e = Env::new();
        e.define("x", int(1));
        let init = e.get("x", L).unwrap().add(&int(1), L).unwrap();
        e.define("x", init);
        assert_eq!(e.get("x", L).unwrap(), int(2));

        // And the same inside a block, where it must not leak outward.
        e.push_scope();
        e.define("y", int(1));
        e.define("y", int(2));
        assert_eq!(e.get("y", L).unwrap(), int(2));
        e.pop_scope();
        assert!(e.get("y", L).is_err());
    }

    /// §6 `.42` A/D: functions and variables are two namespaces.
    /// `let f = 1; fn f() {...}` is legal, `f` as a value is the variable, and
    /// a name that exists only as a variable is not callable.
    #[test]
    fn functions_and_variables_are_separate_namespaces() {
        let mut e = Env::new();
        e.define("f", int(1));
        e.define_fn(decl("f"));
        assert_eq!(e.get("f", L).unwrap(), int(1)); // the variable
        assert_eq!(e.get_fn("f", L).unwrap().name, "f"); // the function

        // A function-only name used as a value: `undefined variable`.
        e.define_fn(decl("only_fn"));
        assert_eq!(
            e.get("only_fn", 3).unwrap_err(),
            TreadleError::undefined_name(3, "only_fn")
        );
        // A variable-only name called: `undefined function`.
        e.define("only_var", int(1));
        assert_eq!(
            e.get_fn("only_var", 4).unwrap_err(),
            TreadleError::undefined_function(4, "only_var")
        );
        // Assignment resolves in the variable namespace only, so a function
        // name is not an assignable binding.
        assert!(e.assign("only_fn", int(2), L).is_err());
    }

    /// §6 `.42` C: `len`/`str`/`int` are reserved as FUNCTION names (a `Parse`
    /// error, so they never reach `define_fn`) but not as variable names — this
    /// table holds no builtins, and `let len = 1;` is an ordinary binding.
    #[test]
    fn builtin_names_are_ordinary_variables_and_absent_from_the_fn_table() {
        let mut e = Env::new();
        e.define("len", int(1));
        assert_eq!(e.get("len", L).unwrap(), int(1));
        for name in ["len", "str", "int"] {
            assert_eq!(
                e.get_fn(name, 2).unwrap_err(),
                TreadleError::undefined_function(2, name)
            );
        }
    }

    /// Functions are hoisted to global (§2), so a body defined at any depth is
    /// callable from anywhere, including from inside another invocation.
    #[test]
    fn the_function_namespace_is_global() {
        let mut e = Env::new();
        e.define_fn(decl("f"));
        let outer = e.push_frame();
        assert!(e.get_fn("f", L).is_ok());
        let inner = e.push_frame();
        assert!(e.get_fn("f", L).is_ok());
        e.pop_frame(inner);
        e.pop_frame(outer);
        assert!(Rc::ptr_eq(
            &e.get_fn("f", L).unwrap(),
            &e.get_fn("f", L).unwrap()
        ));
    }

    /// Recursion: each invocation gets its own scope for the same parameter
    /// name, and the deeper one does not disturb the shallower.
    #[test]
    fn each_invocation_has_its_own_parameters() {
        let mut e = Env::new();
        let a = e.push_frame();
        e.define("n", int(3));
        let b = e.push_frame();
        e.define("n", int(2));
        assert_eq!(e.get("n", L).unwrap(), int(2));
        e.pop_frame(b);
        assert_eq!(e.get("n", L).unwrap(), int(3));
        e.pop_frame(a);
    }
}
