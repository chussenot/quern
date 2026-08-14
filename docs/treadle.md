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

`=` is **not** in that table, and deliberately so: it is a statement token, not
an expression operator. It appears only in `let x = e;` and `x = e;`. There is
no assignment-as-expression, so `x = (y = 1)` and `if (x = 1) { }` are **parse**
errors. Pinned here because an engine that accepted assignment as an expression
would diverge from one that did not, and the fuzzer would report it as a bug in
whichever engine you happened to blame.

**Every binary operator is LEFT-associative**, and parenthesised grouping
exists at the tightest rung alongside literals and variables. So `1 - 2 - 3`
is `-4`, not `2`, and `12 / 3 / 2` is `2`, not `8`. Pinned because the table
above fixes only the rungs: a ladder written as a loop comes out
left-associative and one written by recursing on the rhs comes out
right-associative, and both are natural readings of "the ladder reads
top-to-bottom" — the two engines would have agreed only by luck (bead `.54`).
Rust's own associativity, consistent with the Rust-semantics rule used for
`/` and `%`.

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

// The two operator enums, listed in §2 precedence order, loosest first, so the
// parser's ladder reads top-to-bottom against `BinOp`. Rust's own spelling
// throughout: `Rem` not `Mod`, `Ne` not `Neq`, `Le`/`Ge` not `Lte`/`Gte`.
pub enum UnOp { Neg, Not }
pub enum BinOp { Or, And, Eq, Ne, Lt, Gt, Le, Ge, Add, Sub, Mul, Div, Rem }
```

`Or` and `And` are `BinOp` variants even though both engines must special-case
them **before** evaluating `rhs` (§2 short-circuit): keeping them here means the
parser needs no separate node and the frozen `Expr` needs no sixth variant.
There is no `UnOp::Pos` — `+x` is not in the grammar.

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

**How an `Output` is compared to an `--- expect` section.** By a byte comparison
of a canonical rendering — never by splitting either side into lines. The
rendering is `Display for Output`: every print line newline-**terminated** (not
separated), then the error's display form, also newline-terminated. `terminated,
not separated` is load-bearing: it is what makes `lines == []` render as `""` and
`lines == [""]` render as `"\n"`.

The source section is every byte after the line exactly `--- source` up to the
**first** line exactly `--- expect` (byte-exact line equality, no trimming); the
expect section is every remaining byte of the file, verbatim; the assertion is
`expect_bytes == output.to_string()`. That settles trailing newlines, an
expected-empty output, a printed empty line, a printed string containing `\n`,
and a printed line that begins with `error: ` — no line is ever *classified*, so
none of them needs a rule. A program whose source contains a line that is exactly
`--- expect` is not expressible; `print "--- expect";` is, because that line is
not byte-equal to the delimiter. See bead `.34`, which also excludes `*.tr` from
the whitespace-fixing pre-commit hooks that would otherwise rewrite an assertion.

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

## 6b. Which line an `as_bool` failure reports (bead `.uzm`)

§6/`.40` routes `if`/`while` conditions **and** `and`/`or` operands through one
`Value::as_bool`, and never said which line the error carries. It matters only
when an operator is split across source lines:

```
print 1
and true;
```

**Normative: the error reports the line of the FAILING OPERAND**, not the line
of the `Binary` node. So the example above reports line 1, where the `1` is.

**With one necessary fallback (bead `.pqj`).** The frozen `Expr::Lit(Value)`
carries **no line**, so a literal operand has no line of its own and the rule
above is unimplementable for `true and 1`. In that case — and only that case —
report the line of the **enclosing node**. Both engines must use the same
fallback, so express it as a single `line_of(&Expr) -> u32` helper that returns
the node's own line where it has one and the enclosing line where it does not,
rather than scattering the decision. `vm/compiler.rs` already implements it that
way; `tree/eval.rs` must match. Found by `compiler-expr`, which filed it instead
of reading `src/tree/` to see what the other engine had done — the fallback is
observable whenever an operator is split across lines, so guessing would have
produced exactly the unattributable divergence this run exists to prevent.

Chosen because both engines can reach it without agreeing on anything else: the
tree-walker has the operand's own `Expr` node and its `line`; the VM's compiler
emits `AsBool` immediately after the operand's code and so has that same line to
put in the line table. Pinning the `Binary` node's line instead would require
both engines to agree on how the parser assigns an operator node's line, which is
a second convention to keep in step for no gain.

`tree/eval.rs` currently uses the `Binary` node's line and must change — that is
why `eval-expr` filed this rather than reading `src/vm/` to see what group A did,
which is exactly the behaviour the isolation rule is for.

## 6a. Error wording: the landed `value.rs` is the spec (bead `.55`)

Bead `.39` proposed a table of operator and builtin error messages. It never
landed. `value.rs` did, and is frozen, and `error.rs` adopted its exact strings
and added a test that calls the real `Value::div/rem/add/sub/neg/cmp_value/
eq_value/as_bool` and asserts each error equals the matching constructor. So:

**Where `.39`'s proposal and the merged `value.rs` disagree, `value.rs` wins.**
The wording is `+ expects Int or Str operands, got Int and Bool`,
`- expects a Int operand, got Bool`, `expected Bool, got Int`,
`== expects two values of the same type, got Int and Str`,
`< expects two Int or two Str operands, got Bool and Int`. Corpus authors
assert those. `.39`'s table is superseded, not pending.

One consequence, accepted deliberately: because §6/`.40` routes both the
`if`/`while` condition and the `and`/`or` operands through one `as_bool`, they
produce the **same** message and no corpus case can tell them apart. That is
fine. §4 requires only that the two *engines* agree, and they do; distinguishing
them would need two constructors plus a rule about which applies where, which
is new drift surface for no observable benefit.

## 6. Pinned edges

Every line below is a case §2–§5 left open where the two engines would each have
followed their own most natural implementation to a **different** answer. They
are normative. Each cites the bead carrying the full argument and the exact fix;
read that bead before disagreeing, and file a new one rather than editing here.

**Order and observability** (`.33`). Argument evaluation is **left to right**
everywhere: `print` arguments, call arguments, and the two operands of a binary
operator (`lhs` completely, side effects included, before `rhs` is touched).
`and`/`or` may skip `rhs` entirely; that is short-circuiting, not reordering. A
`print` appends **exactly one** element to `Output.lines`, and only once every
argument evaluated successfully — so `print "a", 1/0;` is
`{lines: [], error: …divide by zero}` and there is **no partial line**. The CLI
renders a finished `Output`; it must not stream.

**Truthiness, and there is none** (`.40`). The condition of `if`/`while` must be
`Bool`; any other type is a `Type` error at the condition's line. Both engines
route it through one `Value::as_bool(line)`.

**Short-circuit typing** (`.40`). The left operand of `and`/`or` must be `Bool`.
The right is type-checked **only if it is evaluated**: `false and 1` is `false`,
`true or nil` is `true`, `true and 1` is a `Type` error.

**Equality and ordering** (`.40`). `==`/`!=` across two different types is a
`Type` error, not `false`. `nil == nil` is `true`. `Str` orders
byte-lexicographically (Rust's `Ord for str`); every pairing under `< > <= >=`
other than Int/Int and Str/Str is a `Type` error.

**Integers** (`.41`). An integer literal is ASCII digits converted with
`i64::from_str`; out of range is a `Lex` error, so `i64::MIN` is reachable only
through arithmetic. `-i64::MIN`, `i64::MIN / -1` and `i64::MIN % -1` are `Value`
overflow errors — use `checked_*`, never bare `/` or `%`. `int(s)` is exactly
`s.parse::<i64>()`; `int` of a non-`Str` is a `Type` error.

**Operator messages** (`.39`). `value.rs` owns the semantics **and** the message
of every operator, builtin and Bool gate, taking the line in and returning a
built `TreadleError` out. Neither engine may construct an error from a literal
string; a message not on that list is a spec gap, not a `format!`.

**`let`** (`.37`). The initialiser is evaluated in the scope **as it exists
before** the new binding is created, so with an outer `x` at 1, `let x = x + 1;`
binds a new `x` at 2 — a compiler must emit the initialiser before declaring the
slot. Re-declaring in the same scope is legal and the later `let` wins.
Top-level scope **is** global scope; a function body cannot create a global.

**Statements** (`.44`, `.46`). There are no expression statements: `f();` is a
`Parse` error — call for effect with `print f();` or `let _ = f();`. `print`
takes one or more arguments. Bodies of `if`/`else`/`while` are always braced, so
there is no dangling else; `else if` chains as `els: vec![Stmt::If{..}]`. `let`
always has an initialiser.

**Calls** (`.35`). The compiler raises **no** errors: `compile` is infallible for
any `Program` the front end produced, and every `Type`/`Name`/`Value` error is
raised when execution reaches it, so `if false { nope(); }` runs clean. A `Call`
proceeds in exactly this order — (a) evaluate arguments left to right, (b)
resolve the name, (c) check arity, (d) check depth, (e) enter. `(a)` precedes
`(b)` because bytecode forces it, so `nope(1/0)` is `divide by zero`.

**Functions** (`.42`). Functions and variables are **separate namespaces**. A
duplicate `fn` name is a `Parse` error; `len`, `str` and `int` are reserved as
function names (a `Parse` error to declare) but not as variable names. A function
name used as a value is `undefined variable`.

**Recursion depth** (`.36`). The counted quantity is **active invocations**, not
frames: the top-level program is depth 0. The check is `depth == 1000` at the
call site, as step (d) above — after arguments, name and arity, before the callee
exists. So 1000 nested invocations succeed and the 1001st fails, with a `Value`
error at the **call's** line, from one constructor, against one
`error::MAX_DEPTH`. Builtins do not consume depth. The tree-walker counts
deliberately and, if 1000 overflows the test stack, runs on a bigger thread — the
limit is observable and cannot be tuned by one engine.

**Error lines** (`.46`). An error's line is the line of the innermost AST node
that failed, never the enclosing statement's, so the VM's line table needs an
entry per **fallible instruction**.

**Engine-only errors** (`.45`). `error::internal(line, what)` exists for a broken
engine invariant and is documented as unreachable: no treadle program can cause
one. The VM has no program-size limit observable in `Output` — operands are
`u32`/`usize`, never `u8`.

**Termination** (`.43`). treadle has no step, time or instruction limit, and
neither engine may invent one: "one step" is not the same quantity in a VM and a
tree-walker, so a step budget would *be* the divergence. The fuzzer's generator
emits only counter-bounded `while` loops, and guards wall clock in the harness,
outside `Output`.
