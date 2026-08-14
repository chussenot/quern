# treadle — one language, two engines

`treadle` is a small imperative language implemented **twice**: once as a
compiler to bytecode plus a stack VM, once as a tree-walking interpreter.
Both back ends sit on one shared front end and must produce **byte-identical
output for every program**. That equality is the point of the project: a
`differential-fuzzer` generates programs, runs both, and any divergence is a
real bug found mechanically, with no human arbiter and nothing to argue about.

Run 5 (`quern`) graded a fleet against a corpus the fleet itself wrote, so an
implementer's belief about what was asserted could be wrong and stay invisible
for an hour. This spec exists to remove that class of error: the oracle is
another implementation, not a human's list of expectations.

**Non-goals, stated so nobody builds them:** no closures, no first-class
functions, no objects, no modules, no imports, no I/O beyond `print`, no
floats, no garbage collector (values are cloned or reference-counted; say
which in a comment), no concurrency, no standard library beyond the builtins
listed in §2.

## 1. The two engines, and the line between them

```
            shared front end (one implementation, group S)
   source ──► lexer ──► parser ──► Ast          §3 frozen
                                    │
                    ┌───────────────┴───────────────┐
                    ▼                               ▼
        group A: compile(&Ast) -> Chunk    group B: eval(&Ast) -> Output
                 vm::run(&Chunk)                    (tree-walking)
                    │                               │
                    └──────────► Output ◄───────────┘
                          must be byte-identical
```

**Group A and group B may not read each other's source.** Both read the spec
and the shared front end. This is not ceremony: two implementations that copy
each other's reasoning agree on their shared mistakes, and the oracle is worth
nothing. If you need to know what the other engine does about some edge, the
answer is in this document or it is a spec gap to file — not in their file.

`Output` is the frozen observable in §3. Everything a program can do is in it,
so "identical output" is a total statement about behaviour.

## 2. The language

```
// line comment
let x = 1;                  // declaration, always initialised
x = x + 1;                  // assignment to an existing binding
print x;                    // the only output
print "a", x, true;         // several values, tab-separated, one line

if x > 2 { print 1; } else { print 2; }
while x < 10 { x = x + 1; }

fn add(a, b) { return a + b; }
print add(1, 2);
fn fact(n) { if n < 2 { return 1; } return n * fact(n - 1); }
```

**Values.** `Int` (i64), `Bool`, `Str`, `Nil`. No floats. No implicit
conversion anywhere.

**Operators**, tightest binding last:

| Precedence | Operators | Operands | Result |
|---|---|---|---|
| 1 (loosest) | `or` | Bool | Bool |
| 2 | `and` | Bool | Bool |
| 3 | `==` `!=` | any two of the same type | Bool |
| 4 | `<` `>` `<=` `>=` | Int, Str | Bool |
| 5 | `+` `-` | Int; `+` also Str (concat) | Int / Str |
| 6 | `*` `/` `%` | Int | Int |
| 7 (tightest) | `-` `!` prefix | Int / Bool | Int / Bool |

`and` and `or` **short-circuit**: the right operand is not evaluated when the
left decides the answer. This is observable through `print` inside a call, so
both engines must do it, and the fuzzer will find it if one does not.

`/` and `%` truncate toward zero (Rust's semantics), so `-7 / 2 == -3` and
`-7 % 2 == -1`. Division or modulo by zero is a runtime error (§4).

**Builtins**, and there are only three: `len(Str) -> Int` (bytes),
`str(any) -> Str` (the §3 display form), `int(Str) -> Int` (a runtime error if
the string is not a valid i64).

**Scoping.** A block `{ ... }` is a scope. `let` binds in the current scope and
**shadows** an outer binding of the same name for the rest of that scope.
Assignment without `let` walks outward to the nearest existing binding, and is
a runtime error if there is none. A function body is a scope whose parent is
the **global** scope, never the caller's — there are no closures, so a function
sees globals and its own parameters and locals, and nothing else.

**Functions.** Declared at any scope but hoisted to global: a program may call
a function declared later in the file. Recursion works. Wrong argument count is
a runtime error. A function with no `return` returns `Nil`. `return` outside a
function is a **parse** error.

**Determinism.** No hash-order iteration in any output path. The same program
produces the same `Output` on every run of either engine, on any platform.

## 3. Frozen contracts

These five items are **frozen**. Both engines and the fuzzer code against
them, so a change breaks work in flight: message the dependents, do not edit
quietly.

`value.rs`:

```rust
pub enum Value { Nil, Int(i64), Bool(bool), Str(Rc<String>) }
pub enum Type { Nil, Int, Bool, Str }
```

`Display for Value` is the one display form, used by `print`, by `str()` and
by every error message that names a value:
`Nil` → `nil`; `Int` → the decimal digits; `Bool` → `true` / `false`;
`Str` → its bytes, **unquoted**.

`error.rs` — the taxonomy is frozen because both engines must produce the
**same** error for the same program:

```rust
pub enum TreadleError {
    Lex { line: u32, msg: String },
    Parse { line: u32, msg: String },
    Type { line: u32, msg: String },      // wrong operand type, wrong arity
    Name { line: u32, msg: String },      // unknown variable or function
    Value { line: u32, msg: String },     // divide by zero, bad int()
}
pub type Result<T> = std::result::Result<T, TreadleError>;
```

`ast.rs` — the boundary between the shared front end and both back ends:

```rust
pub enum Expr {
    Lit(Value),
    Var { name: String, line: u32 },
    Unary { op: UnOp, rhs: Box<Expr>, line: u32 },
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr>, line: u32 },
    Call { name: String, args: Vec<Expr>, line: u32 },
}
pub enum Stmt {
    Let { name: String, init: Expr, line: u32 },
    Assign { name: String, value: Expr, line: u32 },
    Print { args: Vec<Expr>, line: u32 },
    If { cond: Expr, then: Vec<Stmt>, els: Vec<Stmt>, line: u32 },
    While { cond: Expr, body: Vec<Stmt>, line: u32 },
    Return { value: Option<Expr>, line: u32 },
    Fn(Rc<FnDecl>),
}
pub struct FnDecl { pub name: String, pub params: Vec<String>, pub body: Vec<Stmt>, pub line: u32 }
pub struct Program { pub stmts: Vec<Stmt>, pub fns: Vec<Rc<FnDecl>> }
```

Every node carries a `line`, because an error's line number is part of the
observable output and the two engines must agree on it.

`output.rs` — **the observable**, and the whole basis of the comparison:

```rust
pub struct Output { pub lines: Vec<String>, pub error: Option<TreadleError> }
```

A program's `Output` is every `print` line in order, plus at most one error
that stopped it. A program that fails at line 9 has the output of lines 1..8
and then the error — so an engine that buffers output and drops it on error
diverges immediately, deliberately.

`engine.rs`:

```rust
pub trait Engine { fn name(&self) -> &'static str; fn run(&mut self, src: &str) -> Output; }
```

Both engines implement it. The fuzzer and the conformance runner take
`&mut dyn Engine`, so neither knows which it is driving.

## 4. Errors are values

Every failure is an `Output.error`, never a panic and never a process abort.
A panic on any input — including deliberately pathological input — is a bug in
both engines' book.

**Both engines must produce the same error variant, the same line, and the
same message** for the same program. The message text is therefore a contract:
build every message through the constructors in `error.rs` rather than
formatting strings at the call site, so two engines cannot drift on wording.

Guaranteed-terminating error cases: integer overflow on `+ - *` (an `Value`
error, not a wrap and not a panic), divide or modulo by zero, `int()` on a
non-numeric string, unknown name, wrong arity, and wrong operand types.

**Recursion depth.** Deep recursion must produce a `Value` error naming the
limit, not a stack overflow — the VM has a heap-allocated frame stack and the
tree-walker does not, so this is the single most likely place for the two to
diverge. The limit is **1000 frames** in both, and it is part of the spec
precisely because the fuzzer will generate programs that hit it.

## 5. Grading

**Conformance corpus** — `tests/conform/*.tr`, written **before** the engines
(this ordering is a fix for a run-5 finding). One file per case:

```
# comment describing the case
--- source
let x = 2;
print x * 3;
--- expect
6
```

An expected error is written as its display form on its own line after the
output lines:

```
--- expect
1
error: Value at line 3: divide by zero
```

`tests/conform.rs` runs **every case against both engines** and fails naming
the engine, the file, the expected and the actual. A case that passes on one
engine and fails on the other is the most valuable failure the suite can
produce; report it as such rather than as a single failure.

**Differential fuzzer** — `tests/differential.rs`. Generates programs from a
seeded grammar (`rand` with a **fixed seed list**, so a failure is
reproducible and CI is deterministic), runs both engines, and asserts the
`Output`s are equal. On divergence it must print the program, both outputs and
the seed. It also asserts neither engine panicked. Shrinking a failing program
to a minimal reproduction is worth building if time allows; say so either way.

**House rules.** `cargo clippy --all-targets -- -D warnings` clean,
`cargo fmt --check` clean, `#![forbid(unsafe_code)]`, zero dependencies beyond
`rand` (dev-only, for the fuzzer's seeded generator). Anything else needs a
comment arguing for it.
