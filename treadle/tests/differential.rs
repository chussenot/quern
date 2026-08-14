//! bead: differential-fuzzer (`bd_30-agents-2jk.6`) — §5: the **oracle**.
//!
//! Generates treadle programs from a seeded grammar, runs each one through
//! **every** engine, and asserts the [`Output`]s are equal. There is no human
//! arbiter here and nothing to argue about: if the bytecode VM and the
//! tree-walker disagree about one program, one of them is wrong, and the
//! program is the bug report.
//!
//! # Interesting, not merely valid
//!
//! The generator is **type-directed** ([`Gen::expr_of`]): it is asked for an
//! expression of a type and produces one, so most programs run to completion
//! and get *deep* before anything goes wrong. Errors are then injected
//! deliberately, against a small per-program budget ([`Gen::err_budget`]), which
//! is the whole difference between a corpus and noise. The first version of this
//! file drew leaves uniformly by type: 1600 programs, of which 1120 produced no
//! output at all because a `Type` error landed in the first expression and the
//! program never reached the second statement. It agreed on all 1600 and proved
//! almost nothing — a fuzzer whose programs die at line one is a parser test.
//!
//! Each [`Flavor`] then biases the same grammar toward one seam where the two
//! engines are *structurally* different, which is where a divergence can live:
//! short-circuiting (jumps vs. a special case), the 1000-frame limit (a heap
//! frame stack vs. the machine stack), `i64` boundaries, §6b's line-of-operand
//! rule, scoping, errors mid-output, and bounded nesting.
//!
//! # Determinism, which is the whole reason this can live in CI
//!
//! Every program comes from [`SEEDS`] — a **fixed** list of `u64`, one
//! `StdRng::seed_from_u64` each — so this test generates the *same* corpus on
//! every run, on every machine, and a failure is reproducible from the seed and
//! index printed with it. Nothing here reads the clock, the environment or
//! `/dev/urandom` to decide what to generate, and nothing iterates a `HashMap`.
//!
//! # Termination, without inventing a step limit (§6/`.43`)
//!
//! §6 forbids either engine from having a step, time or instruction budget, so
//! the fuzzer cannot lean on one to escape a generated infinite loop. Instead
//! **every generated program is structurally bounded**:
//!
//! * every `while` is emitted by [`Gen::while_stmt`] as a counter loop over a
//!   reserved `_cN` variable with a literal bound, and `_cN` is never in the
//!   pool of readable or assignable names, so no generated statement can touch
//!   the induction variable (asserted by `every_generated_while_is_counter_bounded`);
//! * recursion is bounded by the language itself — `error::MAX_DEPTH` turns
//!   runaway recursion into an `Output.error` at the 1001st active invocation,
//!   which is a *result*, not a hang;
//! * expression and block nesting are bounded by `Knobs::expr_depth` and
//!   [`STMT_DEPTH`].
//!
//! On top of that the sweep asserts a wall-clock ceiling ([`TIME_BUDGET`])
//! *after* the loop, so a pathological slowdown is a failure rather than a
//! silent 40-minute CI job. That ceiling is in the harness, outside `Output`,
//! exactly where `.43` says a clock guard belongs. It deliberately cannot
//! interrupt a single `run`: nothing generated here can hang one, and a watchdog
//! that could kill a run would need a timeout *in* `Output` to report it, which
//! is the step limit `.43` forbids.
//!
//! # Panics are failures (§4)
//!
//! "A panic on any input is a bug in both engines' book", so every `run` goes
//! through [`run_guarded`] and a panic is reported with the seed and the program
//! rather than taking the test process with it. The whole sweep also runs on a
//! [`HARNESS_STACK`]-sized thread: the shared parser is recursive with no depth
//! guard (bead `.67`), and a *stack overflow* is an abort that no `catch_unwind`
//! can turn into a report, so the generator keeps nesting far below the
//! threshold and the big stack is belt and braces. See
//! `nesting_the_generator_emits_is_safe_on_a_default_stack`.

#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use treadle::engine::Engine;
use treadle::output::{diff, Output};

/// Every engine the fuzzer compares.
///
/// Adding an engine is one line, and it is then compared against every other
/// engine pairwise — the oracle gets stronger, not longer.
fn engines() -> Vec<Box<dyn Engine>> {
    vec![
        Box::new(treadle::vm::Vm::new()),           // bd .16 / .17
        Box::new(treadle::tree::eval::Eval::new()), // bd .19 / .20
    ]
}

/// **The fixed seed list.** Changing it changes the corpus; adding to it only
/// adds programs, which is the safe direction. The values are arbitrary but
/// *written down*, because "seeded from the clock" is how a fuzzer finds a bug
/// on Tuesday and cannot show it to anyone on Wednesday.
const SEEDS: &[u64] = &[
    0x0000_0000_0000_0000,
    0x0000_0000_0000_0001,
    0x0000_0000_0000_0002,
    0x0000_0000_0000_0003,
    0x0000_0000_0000_0007,
    0x0000_0000_0000_000d,
    0x0000_0000_0000_002a,
    0x0000_0000_0000_0063,
    0x0000_0000_0000_00ff,
    0x0000_0000_0000_03e8,
    0x0000_0000_0000_1234,
    0x0000_0000_0000_7fff,
    0x0000_0000_0001_0001,
    0x0000_0000_00bc_614e,
    0x0000_0000_1234_5678,
    0x0000_0000_7fff_ffff,
    0x0000_0000_dead_beef,
    0x0000_0001_0000_0000,
    0x0000_00c0_ffee_0000,
    0x0000_5eed_0000_0001,
    0x0001_0203_0405_0607,
    0x0f0f_0f0f_0f0f_0f0f,
    0x1111_1111_1111_1111,
    0x1357_9bdf_0246_8ace,
    0x2718_2818_2845_9045,
    0x3141_5926_5358_9793,
    0x4242_4242_4242_4242,
    0x5555_5555_5555_5555,
    0x6a09_e667_f3bc_c908,
    0x7fff_ffff_ffff_ffff,
    0x8000_0000_0000_0000,
    0x9e37_79b9_7f4a_7c15,
    0xa5a5_a5a5_a5a5_a5a5,
    0xbb67_ae85_84ca_a73b,
    0xc0ff_ee00_c0ff_ee00,
    0xdead_beef_dead_beef,
    0xe621_1b4d_1b4d_0001,
    0xf00d_f00d_f00d_f00d,
    0xffff_ffff_ffff_ffff,
    0x0123_4567_89ab_cdef,
];

/// Programs generated per seed. Each is a *different* flavour (see [`FLAVORS`])
/// and a different draw, never a variant of its neighbour.
const PER_SEED: usize = 80;

/// Wall-clock ceiling for the sweep, asserted after the loop. Generous: on the
/// machine this was written on the full sweep takes about five seconds.
const TIME_BUDGET: Duration = Duration::from_secs(300);

/// Stack for the thread the whole sweep runs on. The tree-walker already gives
/// *itself* one this size for the 1000-frame limit; the shared **parser** is
/// recursive with no guard, and that recursion happens on whatever thread calls
/// `run`, which is this one.
const HARNESS_STACK: usize = 64 << 20;

/// Statement-nesting cap: how deep `if`/`while` bodies may nest.
const STMT_DEPTH: u32 = 3;

// ---------------------------------------------------------------------------
// What to aim at
// ---------------------------------------------------------------------------

/// The seams. A uniform grammar spends nearly every program on arithmetic both
/// engines get right; these bias the same generator at the places the two
/// implementations are *structurally* different.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavor {
    /// The unbiased grammar.
    General,
    /// `false and f()` where `f` prints: the VM compiles these as jumps, the
    /// tree-walker special-cases them before evaluating `rhs` (§2, `.40`).
    ShortCircuit,
    /// 1000 active invocations succeed, the 1001st fails at the call's line
    /// (§4, `.36`). The VM's frame stack is on the heap and the tree-walker's is
    /// the machine stack, so this is the single most likely divergence.
    Recursion,
    /// `i64` boundaries: overflow on `+ - *` and unary `-`, `i64::MIN / -1`,
    /// `%` by zero and by negatives (§6/`.41`).
    Boundary,
    /// Operators split across source lines, the only way to observe §6b's "the
    /// line of the failing operand, the enclosing node's line for a literal".
    LineSplit,
    /// Shadowing, re-declaration, `let x = x + 1`, assignment reaching an outer
    /// binding (§2, `.37`).
    Scope,
    /// Errors mid-output, so the partial-output rule is exercised: prior print
    /// lines are kept, and a `print` with a failing argument emits no line.
    Errors,
    /// Deeply nested expressions — bounded well below the parser's limit (bead
    /// `.67`), because an overflow there aborts rather than reports.
    Nested,
}

const FLAVORS: &[Flavor] = &[
    Flavor::General,
    Flavor::ShortCircuit,
    Flavor::Recursion,
    Flavor::Boundary,
    Flavor::LineSplit,
    Flavor::Scope,
    Flavor::Errors,
    Flavor::Nested,
];

/// Generator biases. One struct rather than one generator per flavour: the
/// *grammar* is shared, only the odds move, so a construct added once is
/// reachable from every flavour.
#[derive(Clone, Copy, Debug)]
struct Knobs {
    /// Deliberate error injections allowed in one program. Small on purpose —
    /// see the module docs: a program that errors in its first expression tests
    /// almost nothing.
    errs: u32,
    /// Chance, in percent, of a newline between a binary operator's lhs and the
    /// operator.
    p_line_split: u32,
    /// Chance an int leaf comes from the `i64`-boundary pool.
    p_boundary: u32,
    /// Chance an expression node is a call.
    p_call: u32,
    /// Chance a `Bool` expression is an `and`/`or`.
    p_logical: u32,
    /// Chance a statement is a `print`.
    p_print: u32,
    /// Maximum expression nesting.
    expr_depth: u32,
    /// Top-level statement count.
    stmts: u32,
    /// If set, drive the recursion limit from this depth.
    rec_depth: Option<i64>,
}

impl Flavor {
    fn knobs(self, rng: &mut StdRng) -> Knobs {
        let mut k = Knobs {
            errs: 1,
            p_line_split: 8,
            p_boundary: 7,
            p_call: 22,
            p_logical: 25,
            p_print: 40,
            expr_depth: 3,
            stmts: 7,
            rec_depth: None,
        };
        match self {
            Flavor::General => {}
            Flavor::ShortCircuit => {
                k.p_logical = 85;
                k.p_call = 40;
                k.errs = 2;
            }
            Flavor::Recursion => {
                // Straddle the limit: 1000 active invocations succeed, 1001 fails.
                k.rec_depth = Some(rng.gen_range(996..=1004));
                k.stmts = 4;
                k.errs = 0;
            }
            Flavor::Boundary => {
                // No injected errors: the *values* do the work here, and an
                // injected `Type` error would stop the program before the
                // overflow it was generated for.
                k.errs = 0;
                k.p_boundary = 80;
                k.p_logical = 4;
                k.expr_depth = 4;
            }
            Flavor::LineSplit => {
                k.p_line_split = 70;
                k.p_logical = 60;
                k.errs = 2;
            }
            Flavor::Scope => {
                k.p_print = 45;
                k.p_call = 8;
                k.stmts = 12;
                k.expr_depth = 2;
            }
            Flavor::Errors => {
                k.errs = 3;
                k.p_print = 55;
                k.p_boundary = 30;
                k.stmts = 8;
            }
            Flavor::Nested => {
                // Bounded on purpose: see the module docs on bead `.67`.
                k.expr_depth = rng.gen_range(6..=9);
                k.stmts = 3;
            }
        }
        k
    }
}

// ---------------------------------------------------------------------------
// The generator
// ---------------------------------------------------------------------------

/// The four §2 value types, as the generator's request. `Any` means "whatever
/// you like", which is what a `print` argument and a `let` initialiser want.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ty {
    Int,
    Bool,
    Str,
    Nil,
    Any,
}

const CONCRETE: &[Ty] = &[Ty::Int, Ty::Bool, Ty::Str, Ty::Nil];

/// What a deliberate error injection does. Budgeted by [`Knobs::errs`] so a
/// program gets a *few* errors in interesting places instead of dying at line 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Inject {
    /// An operand of a clashing type: the `Type` errors of §6a.
    WrongType,
    /// A name nothing ever declared: a `Name` error.
    UnknownName,
    /// A call with the wrong number of arguments: a `Type` error (§3).
    WrongArity,
    /// A call to `boom`, which divides by zero inside a function body.
    Boom,
}

/// Variable names the generator declares and assigns. Deliberately few and
/// reused, so shadowing and re-declaration happen by construction rather than by
/// luck.
const NAMES: &[&str] = &["a", "b", "c", "d"];

/// Int leaves. `9223372036854775807` is `i64::MAX`; `i64::MIN` is not a literal
/// (§6/`.41` makes it a `Lex` error) and is reached through `mn` from the
/// preamble instead.
const INTS: &[&str] = &[
    "0",
    "1",
    "2",
    "3",
    "7",
    "10",
    "64",
    "200",
    "999",
    "1000",
    "1001",
    "4611686018427387904",
    "9223372036854775806",
    "9223372036854775807",
];

/// Leaves whose whole job is to break arithmetic: `i64::MIN`/`MAX` from the
/// preamble, zero, and negatives, so `/` and `%` see both signs and both
/// overflow shapes (`mn / (0 - 1)`, `mn % (0 - 1)`, `mx + 1`, `mn - 1`).
const BOUNDARY_INTS: &[&str] = &[
    "mn",
    "mx",
    "0",
    "(0 - 1)",
    "(0 - 3)",
    "1",
    "2",
    "9223372036854775807",
    "(mn + 1)",
    "(mx - 1)",
];

/// String leaves: empty, `int`-parseable, `int`-unparseable, out of `i64` range,
/// and three whose bytes would hide a rendering difference (tab, embedded
/// newline, trailing space) if anything compared line by line.
const STRS: &[&str] = &[
    r#""""#,
    r#""a""#,
    r#""ab""#,
    r#""-1""#,
    r#""7""#,
    r#""0""#,
    r#""9223372036854775808""#,
    // §6/`.41`: `int(s)` is exactly `s.parse::<i64>()`, which accepts a leading
    // `+` and rejects surrounding space. Two engines that each wrote their own
    // "is this numeric" check would part company on precisely these.
    r#""+5""#,
    r#"" 7 ""#,
    r#""7x""#,
    r#""-""#,
    r#"" x ""#,
    r#""a\tb""#,
    r#""a\nb""#,
    r#""error: nope""#,
    r#""\\""#,
    // Multibyte, because `len` is **bytes** (§2) and an engine that reached for
    // `chars().count()` would disagree here and nowhere else.
    r#""é""#,
    r#""日本語""#,
];

/// Every non-logical binary operator over `Int` (§2 rungs 5 and 6). `and`/`or`,
/// `==`/`!=` and the ordering operators are generated by their own methods,
/// because each has its own operand rule.
const INT_OPS: &[&str] = &["+", "-", "*", "/", "%"];
const ORDER_OPS: &[&str] = &["<", ">", "<=", ">="];

/// The three builtins (§2), which are also the three names §6/`.42` reserves as
/// function names but **not** as variable names.
const BUILTINS: &[&str] = &["len", "str", "int"];

/// The preamble's recursive drivers, all of which count down to a base case and
/// therefore terminate — either by returning or by hitting `MAX_DEPTH`.
const RECURSIVE: &[&str] = &["rec", "dp", "mu", "mv", "ml"];

/// Fixed preamble, in every program. `mn` is `i64::MIN` built by arithmetic
/// because the literal is a `Lex` error; `se` is the side-effect function the
/// short-circuit flavour needs — it *prints*, so skipping it is observable;
/// `boom` fails inside a call; `rec` and `dp` drive the depth limit, `dp`
/// printing on the way down so the depth error arrives mid-output.
///
/// `mu`/`mv` reach the same 1000-invocation limit by **mutual** recursion, which
/// self-recursion does not exercise: the failing call is in a different function
/// from the one that started, so the `Value` error's line comes from the other
/// declaration. `ml` carries two locals per frame, so the frames the limit
/// counts are not all one slot wide.
///
/// `nested` is declared **inside a block** on purpose: §2 hoists a function
/// declared at any scope to global, so every program calls a function whose
/// declaration was never at top level. The parser is shared, but *resolving* it
/// is each engine's own code.
const PREAMBLE: &str = "\
let mx = 9223372036854775807;
let mn = 0 - 9223372036854775807 - 1;
fn se(t) { print \"se\", t; return t; }
fn boom(n) { return n / 0; }
fn rec(n) { if n < 1 { return 0; } return rec(n - 1); }
fn dp(n) { if n % 200 == 0 { print n; } if n < 1 { return 0; } return dp(n - 1); }
fn two(p, q) { return p + q; }
fn mu(n) { if n < 1 { return 0; } return mv(n - 1); }
fn mv(n) { if n < 1 { return 1; } return mu(n - 1); }
fn ml(n) { let u = n * 2; let v = u + 1; if n < 1 { return v; } return ml(n - 1); }
if true { fn nested(v) { return v; } }
";

/// Declared **after** the generated body and called from inside it, so every
/// program exercises §2 hoisting — a call to a function declared later.
const EPILOGUE: &str = "fn late() { print \"late\"; return 7; }\n";

struct Gen<'r> {
    rng: &'r mut StdRng,
    k: Knobs,
    /// Innermost scope last, each a list of `(name, type)`. Only these names are
    /// read or assigned, so the reserved loop counters can never be clobbered.
    /// A `Vec`, not a map: nothing here may depend on hash order (§2
    /// Determinism), and the innermost-first walk is exactly §2 shadowing.
    scopes: Vec<Vec<(String, Ty)>>,
    /// Remaining deliberate error injections.
    err_budget: u32,
    counters: u32,
    src: String,
}

impl<'r> Gen<'r> {
    fn new(rng: &'r mut StdRng, k: Knobs) -> Gen<'r> {
        let err_budget = if rng.gen_range(0..100) < 35 {
            0
        } else {
            k.errs
        };
        Gen {
            rng,
            k,
            // The preamble's globals.
            scopes: vec![vec![
                ("mx".to_string(), Ty::Int),
                ("mn".to_string(), Ty::Int),
            ]],
            // Drawn per program, not fixed at `k.errs`: there are dozens of
            // injection opportunities in one program, so a non-zero budget is
            // spent with near certainty and *every* program would carry a
            // deliberate error. A third of them get none, and those are the ones
            // that run to the end and compare a long `Output` rather than a
            // short one and an error message.
            err_budget,
            counters: 0,
            src: String::new(),
        }
    }

    fn chance(&mut self, percent: u32) -> bool {
        self.rng.gen_range(0..100) < percent
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.rng.gen_range(0..xs.len())]
    }

    /// Spend one unit of the error budget on `kind`, if the dice and the budget
    /// agree. `WrongType` is likelier than the rest because it is the injection
    /// that reaches the most operators.
    fn inject(&mut self, kind: Inject) -> bool {
        if self.err_budget == 0 {
            return false;
        }
        let p = match kind {
            Inject::WrongType => 22,
            Inject::UnknownName => 6,
            Inject::WrongArity => 10,
            Inject::Boom => 10,
        };
        if self.chance(p) {
            self.err_budget -= 1;
            return true;
        }
        false
    }

    fn line(&mut self, indent: usize, text: &str) {
        for _ in 0..indent {
            self.src.push_str("    ");
        }
        self.src.push_str(text);
        self.src.push('\n');
    }

    fn declare(&mut self, name: &str, ty: Ty) {
        self.scopes
            .last_mut()
            .expect("a scope")
            .push((name.to_string(), ty));
    }

    fn some_ty(&mut self) -> Ty {
        *self.pick(CONCRETE)
    }

    /// A concrete type that is **not** `ty` — the operand of a deliberate
    /// `Type` error.
    fn other_ty(&mut self, ty: Ty) -> Ty {
        let mut t = self.some_ty();
        while t == ty {
            t = self.some_ty();
        }
        t
    }

    /// The live binding of each name, innermost scope first — §2 shadowing, so a
    /// re-declared name is seen at its newest type.
    fn live(&self, ty: Ty) -> Vec<String> {
        let mut seen: Vec<&str> = Vec::new();
        let mut out = Vec::new();
        for scope in self.scopes.iter().rev() {
            for (name, t) in scope.iter().rev() {
                if seen.contains(&name.as_str()) {
                    continue;
                }
                seen.push(name);
                if ty == Ty::Any || *t == ty {
                    out.push(name.clone());
                }
            }
        }
        out
    }

    fn int_leaf(&mut self) -> String {
        if self.chance(self.k.p_boundary) {
            self.pick(BOUNDARY_INTS).to_string()
        } else {
            self.pick(INTS).to_string()
        }
    }

    fn literal_of(&mut self, ty: Ty) -> String {
        match ty {
            Ty::Int => self.int_leaf(),
            Ty::Bool => if self.chance(50) { "true" } else { "false" }.to_string(),
            Ty::Str => self.pick(STRS).to_string(),
            Ty::Nil => "nil".to_string(),
            Ty::Any => {
                let t = self.some_ty();
                self.literal_of(t)
            }
        }
    }

    /// The two functions that return their argument unchanged. `nested` is the
    /// one declared inside a block, so choosing it exercises §2 hoisting rather
    /// than just a call.
    fn identity_fn(&mut self) -> &'static str {
        if self.chance(50) {
            "se"
        } else {
            "nested"
        }
    }

    /// A leaf of type `ty`: a live variable of that type about half the time, a
    /// literal otherwise, and an unresolvable name when an injection says so.
    fn leaf_of(&mut self, ty: Ty) -> String {
        if self.inject(Inject::UnknownName) {
            // §6/`.42`: functions and variables are **separate** namespaces, so
            // a function name used as a value is an *undefined variable*, not a
            // function. Half the injections take that route, because "which
            // namespace does a bare `se` resolve in" is a question each engine
            // answers in its own code.
            return if self.chance(50) {
                "zz".to_string()
            } else {
                (*self.pick(&["se", "rec", "two", "nested", "late"])).to_string()
            };
        }
        let live = self.live(ty);
        if !live.is_empty() && self.chance(45) {
            let i = self.rng.gen_range(0..live.len());
            return live[i].clone();
        }
        self.literal_of(ty)
    }

    /// The argument of `rec`/`dp`: usually a plain count so the recursion is
    /// actually driven, occasionally an expression so its type errors are too.
    fn depth_arg(&mut self, d: u32) -> String {
        if self.chance(85) {
            self.rng.gen_range(0..=1002).to_string()
        } else {
            self.expr_of(Ty::Int, d)
        }
    }

    /// A call whose result has type `ty`.
    fn call_of(&mut self, ty: Ty, d: u32) -> String {
        let d1 = d.saturating_sub(1);
        if self.inject(Inject::UnknownName) {
            let a = self.expr_of(Ty::Any, d1);
            return format!("nope({a})");
        }
        if self.inject(Inject::WrongArity) {
            // Both directions, on a user function and on a builtin (§3: wrong
            // arity is a `Type` error, raised at the call, not at compile time).
            let a = self.expr_of(Ty::Any, d1);
            return match self.rng.gen_range(0..4) {
                0 => "se()".to_string(),
                1 => format!("se({a}, {a})"),
                2 => format!("two({a})"),
                _ => format!("len({a}, {a})"),
            };
        }
        if self.inject(Inject::Boom) {
            let a = self.expr_of(Ty::Int, d1);
            return format!("boom({a})");
        }
        match ty {
            // `se`/`nested` return their argument, so they are the identity at
            // every type and the only calls that fit `Bool` and `Str` without a
            // conversion.
            Ty::Bool | Ty::Nil => {
                let f = self.identity_fn();
                let a = self.expr_of(ty, d1);
                format!("{f}({a})")
            }
            Ty::Str => {
                if self.chance(55) {
                    let a = self.expr_of(Ty::Any, d1);
                    format!("str({a})")
                } else {
                    let f = self.identity_fn();
                    let a = self.expr_of(Ty::Str, d1);
                    format!("{f}({a})")
                }
            }
            Ty::Int => match self.rng.gen_range(0..10) {
                0..=1 => {
                    let f = self.identity_fn();
                    let a = self.expr_of(Ty::Int, d1);
                    format!("{f}({a})")
                }
                2..=3 => {
                    // All four recursive drivers: silent, printing, mutual, and
                    // one with locals in every frame.
                    let f = *self.pick(RECURSIVE);
                    let n = self.depth_arg(d1);
                    format!("{f}({n})")
                }
                4 => {
                    let n = self.depth_arg(d1);
                    format!("dp({n})")
                }
                5..=6 => {
                    let a = self.expr_of(Ty::Int, d1);
                    let b = self.expr_of(Ty::Int, d1);
                    format!("two({a}, {b})")
                }
                7..=8 => {
                    let a = self.expr_of(Ty::Str, d1);
                    format!("len({a})")
                }
                _ => {
                    // `int` of a non-numeric string is a `Value` error (§2).
                    let a = self.expr_of(Ty::Str, d1);
                    format!("int({a})")
                }
            },
            Ty::Any => match self.rng.gen_range(0..6) {
                0 => "late()".to_string(),
                // `g`'s return type is whatever its generated body returns —
                // often `Nil`, since a function with no `return` returns `Nil`.
                1..=2 => {
                    let a = self.expr_of(Ty::Any, d1);
                    let b = self.expr_of(Ty::Any, d1);
                    format!("g({a}, {b})")
                }
                _ => {
                    let t = self.some_ty();
                    self.call_of(t, d)
                }
            },
        }
    }

    /// `lhs op rhs`, with §6b's newline between the lhs and the operator at
    /// `p_line_split`. That newline is the *only* way to observe which line an
    /// `as_bool` failure reports, and therefore the only way a divergence in the
    /// `line_of` fallback becomes visible.
    fn join(&mut self, lhs: &str, op: &str, rhs: &str) -> String {
        if self.chance(self.k.p_line_split) {
            format!("{lhs}\n{op} {rhs}")
        } else {
            format!("{lhs} {op} {rhs}")
        }
    }

    fn expr_of(&mut self, ty: Ty, d: u32) -> String {
        // The one place a deliberate type error is introduced: ask for the wrong
        // type here and every operator, builtin and Bool gate downstream sees a
        // clashing operand without any of them needing a special case.
        let mut ty = ty;
        if ty != Ty::Any && self.inject(Inject::WrongType) {
            ty = self.other_ty(ty);
        }
        if ty == Ty::Any {
            ty = self.some_ty();
        }
        if d == 0 {
            return self.leaf_of(ty);
        }
        if self.chance(self.k.p_call) {
            return self.call_of(ty, d);
        }
        match ty {
            Ty::Nil => "nil".to_string(),
            Ty::Int => match self.rng.gen_range(0..10) {
                0..=2 => self.leaf_of(Ty::Int),
                3 => {
                    let e = self.expr_of(Ty::Int, d - 1);
                    format!("({e})")
                }
                4 => {
                    // `-mn` is the `i64::MIN` overflow of §6/`.41`; `-` on a
                    // non-Int is a `Type` error when the injection lands here.
                    let e = self.expr_of(Ty::Int, d - 1);
                    format!("-({e})")
                }
                _ => {
                    let op = *self.pick(INT_OPS);
                    let lhs = self.expr_of(Ty::Int, d - 1);
                    let rhs = self.expr_of(Ty::Int, d - 1);
                    self.join(&lhs, op, &rhs)
                }
            },
            Ty::Bool => {
                if self.chance(self.k.p_logical) {
                    return self.logical(d);
                }
                match self.rng.gen_range(0..10) {
                    0..=2 => self.leaf_of(Ty::Bool),
                    3 => {
                        let e = self.expr_of(Ty::Bool, d - 1);
                        format!("!({e})")
                    }
                    4 => {
                        let e = self.expr_of(Ty::Bool, d - 1);
                        format!("({e})")
                    }
                    5..=7 => {
                        // §6/`.40`: `==`/`!=` across two different types is a
                        // `Type` error, not `false`, and `nil == nil` is `true`.
                        let t = self.some_ty();
                        let op = if self.chance(50) { "==" } else { "!=" };
                        let lhs = self.expr_of(t, d - 1);
                        let rhs = self.expr_of(t, d - 1);
                        self.join(&lhs, op, &rhs)
                    }
                    _ => {
                        // Ordering is Int/Int or Str/Str only; every other
                        // pairing is a `Type` error (§6/`.40`).
                        let t = if self.chance(70) { Ty::Int } else { Ty::Str };
                        let op = *self.pick(ORDER_OPS);
                        let lhs = self.expr_of(t, d - 1);
                        let rhs = self.expr_of(t, d - 1);
                        self.join(&lhs, op, &rhs)
                    }
                }
            }
            Ty::Str => match self.rng.gen_range(0..8) {
                0..=3 => self.leaf_of(Ty::Str),
                4..=5 => {
                    // `+` on two Strs is concatenation (§2 rung 5).
                    let lhs = self.expr_of(Ty::Str, d - 1);
                    let rhs = self.expr_of(Ty::Str, d - 1);
                    self.join(&lhs, "+", &rhs)
                }
                _ => {
                    let e = self.expr_of(Ty::Any, d - 1);
                    format!("str({e})")
                }
            },
            Ty::Any => unreachable!("Any was replaced with a concrete type above"),
        }
    }

    /// `and`/`or`, where the two engines are structurally different: the VM
    /// compiles a jump over `rhs`, the tree-walker special-cases the node before
    /// evaluating it.
    fn logical(&mut self, d: u32) -> String {
        let op = if self.chance(50) { "and" } else { "or" };
        // A literal `false and`/`true or` guarantees the short circuit is taken,
        // and a *printing* call on the right makes taking it observable — the
        // `se` line appears in `Output.lines` iff `rhs` was evaluated.
        let lhs = if self.chance(45) {
            match op {
                "and" => "false".to_string(),
                _ => "true".to_string(),
            }
        } else {
            self.expr_of(Ty::Bool, d - 1)
        };
        let rhs = if self.chance(55) {
            let inner = if self.chance(50) { "true" } else { "false" };
            format!("se({inner})")
        } else {
            self.expr_of(Ty::Bool, d - 1)
        };
        // §6b is only observable when the operator is split across lines *and*
        // an operand fails `as_bool`, so when the budget allows, aim the two at
        // each other rather than hoping they coincide. `leaf_of` returns either
        // a literal (which has no line of its own — the enclosing-node fallback
        // of bead `.pqj`) or a variable (which has one), so both halves of the
        // rule get generated.
        if self.err_budget > 0 && self.chance(60) {
            self.err_budget -= 1;
            let t = self.other_ty(Ty::Bool);
            let bad = self.leaf_of(t);
            let (lhs, rhs) = if self.chance(50) {
                (bad, rhs)
            } else {
                // A bad *right* operand is only reached if the left does not
                // short-circuit, which is §6/`.40`'s "the right is type-checked
                // only if it is evaluated".
                (lhs, bad)
            };
            return if self.chance(80) {
                format!("{lhs}\n{op} {rhs}")
            } else {
                format!("{lhs} {op} {rhs}")
            };
        }
        self.join(&lhs, op, &rhs)
    }

    /// A `while` that cannot run away: a reserved counter, a literal bound, and
    /// an increment the generator appends itself. `_cN` is never declared into
    /// `scopes`, so no generated statement can read or assign it (§6/`.43`).
    fn while_stmt(&mut self, indent: usize, d: u32, in_fn: bool) {
        let c = format!("_c{}", self.counters);
        self.counters += 1;
        let bound = self.rng.gen_range(1..=3);
        self.line(indent, &format!("let {c} = 0;"));
        self.line(indent, &format!("while {c} < {bound} {{"));
        self.block(indent + 1, d, in_fn);
        self.line(indent + 1, &format!("{c} = {c} + 1;"));
        self.line(indent, "}");
    }

    /// A braced body, which is a scope (§2). Pushing and popping here is what
    /// makes an inner `let` shadow and an inner assignment reach outward.
    fn block(&mut self, indent: usize, d: u32, in_fn: bool) {
        self.scopes.push(Vec::new());
        let n = self.rng.gen_range(1..=3);
        for _ in 0..n {
            self.stmt(indent, d, in_fn);
        }
        self.scopes.pop();
    }

    fn stmt(&mut self, indent: usize, d: u32, in_fn: bool) {
        if self.chance(self.k.p_print) {
            let n = self.rng.gen_range(1..=3);
            let mut args = Vec::new();
            for _ in 0..n {
                args.push(self.expr_of(Ty::Any, self.k.expr_depth));
            }
            let args = args.join(", ");
            self.line(indent, &format!("print {args};"));
            return;
        }
        // At `d == 0` the `if`/`while` arms are closed off, so the tail is flat.
        let kinds = if d == 0 { 3 } else { 6 };
        match self.rng.gen_range(0..kinds) {
            0 => {
                // §6/`.37`: the initialiser is evaluated in the scope as it
                // exists *before* the binding, so `let a = a + 1;` with an outer
                // `a` at 1 binds a new `a` at 2. Re-declaring in the same scope
                // is legal and the later `let` wins — both happen here, because
                // the name comes from a pool of four.
                // §6/`.42`: `len`, `str` and `int` are reserved as *function*
                // names but **not** as variable names, so `let str = 5;` is
                // legal and `str(1)` still works afterwards. Rare, and a
                // genuinely nasty edge for anything that resolves calls through
                // the variable environment.
                let name = if self.chance(4) {
                    (*self.pick(BUILTINS)).to_string()
                } else {
                    self.pick(NAMES).to_string()
                };
                let ints = self.live(Ty::Int);
                if ints.contains(&name) && self.chance(50) {
                    self.line(indent, &format!("let {name} = {name} + 1;"));
                    self.declare(&name, Ty::Int);
                } else {
                    let ty = self.some_ty();
                    let init = self.expr_of(ty, self.k.expr_depth);
                    self.line(indent, &format!("let {name} = {init};"));
                    self.declare(&name, ty);
                }
            }
            1 => {
                // Assignment walks outward to the nearest binding (§2), so this
                // reaches an *outer* one whenever the inner scope has not
                // declared the name — and is a `Name` error when nothing has.
                if self.inject(Inject::UnknownName) {
                    let value = self.expr_of(Ty::Any, self.k.expr_depth);
                    self.line(indent, &format!("zz = {value};"));
                    return;
                }
                // Only a name that is actually bound: an assignment to an
                // undeclared name is a `Name` error and a perfectly good
                // program, but drawing the target from the whole pool made it
                // *most* programs, which killed them before they printed
                // anything. It is reachable deliberately, above.
                //
                // `p`/`q` are in the pool so that **assigning to a parameter**
                // is generated: a VM writes a frame slot and a tree-walker
                // writes an environment entry, and neither may let the write
                // escape to a global or to the caller. `mx`/`mn` are excluded so
                // the `i64::MIN` the boundary leaves depend on stays intact.
                let bound: Vec<String> = self
                    .live(Ty::Any)
                    .into_iter()
                    .filter(|n| NAMES.contains(&n.as_str()) || n == "p" || n == "q")
                    .collect();
                if bound.is_empty() {
                    let ty = self.some_ty();
                    let name = self.pick(NAMES).to_string();
                    let init = self.expr_of(ty, self.k.expr_depth);
                    self.line(indent, &format!("let {name} = {init};"));
                    self.declare(&name, ty);
                    return;
                }
                let i = self.rng.gen_range(0..bound.len());
                let name = bound[i].clone();
                // A variable's type may change under assignment — no static
                // types in treadle — so record the new one and keep the
                // generator's model honest.
                let ty = self.some_ty();
                let value = self.expr_of(ty, self.k.expr_depth);
                self.line(indent, &format!("{name} = {value};"));
                if let Some(slot) = self
                    .scopes
                    .iter_mut()
                    .rev()
                    .flat_map(|s| s.iter_mut().rev())
                    .find(|(n, _)| *n == name)
                {
                    slot.1 = ty;
                }
            }
            2 if in_fn => {
                if self.chance(25) {
                    // A bare `return;` and falling off the end both yield `Nil`.
                    self.line(indent, "return;");
                } else {
                    let value = self.expr_of(Ty::Any, self.k.expr_depth);
                    self.line(indent, &format!("return {value};"));
                }
            }
            2 => {
                // §6/`.44`: there are no expression statements, so calling for
                // effect goes through `let _ = f();`.
                let value = self.call_of(Ty::Any, self.k.expr_depth);
                self.line(indent, &format!("let _ = {value};"));
                self.declare("_", Ty::Any);
            }
            3 => {
                // §6/`.40`: the condition must be `Bool` — there is no
                // truthiness — so a non-Bool here is a `Type` error at the
                // condition's line.
                let cond = self.expr_of(Ty::Bool, self.k.expr_depth.min(2));
                self.line(indent, &format!("if {cond} {{"));
                self.block(indent + 1, d - 1, in_fn);
                if self.chance(55) {
                    self.line(indent, "} else {");
                    self.block(indent + 1, d - 1, in_fn);
                }
                self.line(indent, "}");
            }
            _ => self.while_stmt(indent, d - 1, in_fn),
        }
    }
}

/// One program: the preamble, a generated function `g`, the generated top-level
/// body, then `late`. Hoisting, a call to a function declared later, and a user
/// function with a generated body are therefore in every program, and the
/// variety is in the body.
fn generate(rng: &mut StdRng, flavor: Flavor) -> String {
    let k = flavor.knobs(rng);

    // `g`'s body first, in its own generator: a function body is a scope whose
    // parent is the **global** scope, never the caller's (§2), so it sees the
    // preamble globals and its own params and nothing of the body below. Its
    // loop counters start high so they cannot collide with the outer
    // generator's, which would let an inner `let _c0` shadow an outer loop's.
    let g_body = {
        let mut body = Gen::new(rng, k);
        body.declare("p", Ty::Int);
        body.declare("q", Ty::Int);
        body.counters = 1000;
        let n = body.rng.gen_range(1..=3);
        for _ in 0..n {
            body.stmt(1, STMT_DEPTH, true);
        }
        body.src
    };

    let mut g = Gen::new(rng, k);
    if let Some(depth) = k.rec_depth {
        // `rec` recurses silently; `dp` prints on the way down, so the depth
        // error arrives *after* output and the partial-output rule is in play;
        // `mu`/`mv` get there mutually and `ml` with locals in every frame.
        let f = *g.pick(RECURSIVE);
        g.line(0, &format!("print {f}({depth});"));
    }
    for _ in 0..k.stmts {
        g.stmt(0, STMT_DEPTH, false);
    }

    let mut src = String::from(PREAMBLE);
    src.push_str("fn g(p, q) {\n");
    src.push_str(&g_body);
    src.push_str("}\n");
    src.push_str(&g.src);
    src.push_str(EPILOGUE);
    src
}

// ---------------------------------------------------------------------------
// Running, comparing, shrinking
// ---------------------------------------------------------------------------

/// `Engine::run` is infallible by contract (§3), so a panic is a **finding**,
/// not something to propagate. Returns the panic message, so a report can name
/// it next to the program and the seed.
fn run_guarded(engine: &mut dyn Engine, src: &str) -> Result<Output, String> {
    catch_unwind(AssertUnwindSafe(|| engine.run(src))).map_err(|p| {
        p.downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string())
    })
}

/// `Err(report)` if any two engines disagree, or if any engine panicked.
///
/// On agreement the `Output` is returned: every engine produced it, so it *is*
/// the program's meaning, and the caller can account for what the corpus
/// exercised without paying for another run.
fn compare(engines: &mut [Box<dyn Engine>], src: &str) -> Result<Output, String> {
    let mut outs: Vec<(&'static str, Output)> = Vec::with_capacity(engines.len());
    for e in engines.iter_mut() {
        let name = e.name();
        match run_guarded(e.as_mut(), src) {
            Ok(out) => outs.push((name, out)),
            Err(msg) => return Err(format!("engine {name} PANICKED: {msg}")),
        }
    }
    for (i, (ln, lo)) in outs.iter().enumerate() {
        for (rn, ro) in outs.iter().skip(i + 1) {
            if lo != ro {
                return Err(diff(ln, lo, rn, ro));
            }
        }
    }
    Ok(outs.pop().expect("at least one engine").1)
}

/// Delete one line at a time for as long as the disagreement survives.
///
/// Line granularity needs no grammar and is enough on generated source: a
/// deletion that unbalances the braces turns the program into a `Parse` error
/// both engines report identically, so the candidate simply stops disagreeing
/// and is rejected. Runs to a fixpoint, so the result is minimal with respect to
/// single-line deletion — which on a 40-line generated program is the difference
/// between a report someone can act on and one they cannot.
fn shrink(engines: &mut [Box<dyn Engine>], src: &str) -> String {
    let mut best: Vec<String> = src.lines().map(str::to_string).collect();
    let mut improved = true;
    while improved {
        improved = false;
        let mut i = 0;
        while i < best.len() {
            let mut cand = best.clone();
            cand.remove(i);
            let text = format!("{}\n", cand.join("\n"));
            if compare(engines, &text).is_err() {
                best = cand;
                improved = true;
            } else {
                i += 1;
            }
        }
    }
    format!("{}\n", best.join("\n"))
}

/// Run `f` on a thread with a stack big enough for the recursive parser.
fn on_big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(HARNESS_STACK)
        .spawn(f)
        .expect("spawn the fuzz thread")
        .join()
        .expect("the fuzz thread panicked")
}

/// What the sweep saw, so a green run can *state* its coverage instead of
/// claiming it. Asserted at the end of the sweep: a generator that quietly
/// stopped producing interesting programs would otherwise pass forever, and
/// "1600 programs, zero divergences" would come to mean 1600 parse errors.
#[derive(Debug, Default)]
struct Stats {
    programs: usize,
    /// Ran to completion with no error at all.
    clean: usize,
    /// Printed at least one line.
    with_lines: usize,
    /// Printed at least one line **and then** failed — §6/`.33`'s partial output.
    lines_then_error: usize,
    lex: usize,
    parse: usize,
    type_err: usize,
    name: usize,
    value: usize,
    depth_limit: usize,
    overflow: usize,
    div_zero: usize,
    /// `se` was called, i.e. an `and`/`or` right operand was actually evaluated.
    side_effect_seen: usize,
    /// Total print lines across the corpus.
    total_lines: usize,
}

impl Stats {
    fn record(&mut self, out: &Output) {
        self.programs += 1;
        self.total_lines += out.lines.len();
        if !out.lines.is_empty() {
            self.with_lines += 1;
        }
        if out.lines.iter().any(|l| l.starts_with("se\t")) {
            self.side_effect_seen += 1;
        }
        match &out.error {
            None => self.clean += 1,
            Some(e) => {
                if !out.lines.is_empty() {
                    self.lines_then_error += 1;
                }
                // Classifying the *rendered* error keeps this file out of the
                // business of knowing `TreadleError`'s shape.
                let msg = e.to_string();
                if msg.contains("Lex at") {
                    self.lex += 1;
                } else if msg.contains("Parse at") {
                    self.parse += 1;
                } else if msg.contains("Type at") {
                    self.type_err += 1;
                } else if msg.contains("Name at") {
                    self.name += 1;
                } else if msg.contains("Value at") {
                    self.value += 1;
                }
                if msg.contains("recursion limit") {
                    self.depth_limit += 1;
                }
                if msg.contains("overflow") {
                    self.overflow += 1;
                }
                if msg.contains("by zero") {
                    self.div_zero += 1;
                }
            }
        }
    }
}

/// The sweep. Returns the stats and every divergence found, already shrunk.
fn sweep() -> (Stats, Vec<String>) {
    let mut engines = engines();
    assert!(
        engines.len() >= 2,
        "a differential fuzzer over fewer than two engines is not an oracle"
    );
    let mut stats = Stats::default();
    let mut findings: Vec<String> = Vec::new();
    let started = Instant::now();

    for &seed in SEEDS {
        let mut rng = StdRng::seed_from_u64(seed);
        for i in 0..PER_SEED {
            let flavor = FLAVORS[i % FLAVORS.len()];
            let src = generate(&mut rng, flavor);
            match compare(&mut engines, &src) {
                Ok(out) => stats.record(&out),
                Err(report) => {
                    let minimal = shrink(&mut engines, &src);
                    let mut f = String::new();
                    let _ = writeln!(
                        f,
                        "\n=== DIVERGENCE (seed 0x{seed:016x}, program {i}, flavor {flavor:?}) ==="
                    );
                    let _ = writeln!(f, "{report}");
                    let _ = writeln!(
                        f,
                        "--- minimal program ({} lines) ---\n{minimal}",
                        minimal.lines().count()
                    );
                    let _ = writeln!(
                        f,
                        "--- original program ({} lines) ---\n{src}",
                        src.lines().count()
                    );
                    findings.push(f);
                }
            }
        }
    }

    let elapsed = started.elapsed();
    println!(
        "fuzzed {} programs from {} seeds x {} flavours in {:?}\n  {:?}",
        stats.programs,
        SEEDS.len(),
        FLAVORS.len(),
        elapsed,
        stats
    );
    assert!(
        elapsed < TIME_BUDGET,
        "the sweep took {elapsed:?}, over the {TIME_BUDGET:?} harness budget — \
         something generated is pathologically slow (§6/.43 forbids a step limit \
         inside the engines, so the guard is out here)"
    );
    (stats, findings)
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// **The oracle.** Every generated program, every engine, byte-equal `Output`s,
/// and no panics.
///
/// The coverage floors are asserted in the *same* test rather than a second one:
/// a green sweep is only worth something if the corpus was interesting, and
/// running the sweep twice to say so doubles the suite for nothing. Every floor
/// is far below what the generator actually produces; they exist to fail loudly
/// if a change quietly narrows the grammar.
#[test]
fn every_engine_agrees_on_every_generated_program() {
    let (s, findings) = on_big_stack(sweep);
    assert!(
        findings.is_empty(),
        "{} divergence(s) found:\n{}",
        findings.len(),
        findings.join("\n")
    );

    assert!(s.programs >= 1000, "{s:?}");
    // Almost every program must be syntactically valid, or the fuzzer is
    // testing the parser's error path and nothing else.
    assert!(
        s.lex + s.parse < s.programs / 50,
        "too many programs never reached execution: {s:?}"
    );
    // ... and must get far enough to *do* something. Every floor below is a
    // regression guard set well under what the corpus measured when it was
    // written (the numbers in the comments), not a target to squeeze past.
    assert!(s.clean > 100, "{s:?}"); // 158
    assert!(s.with_lines > s.programs / 2, "{s:?}"); // 1076
    assert!(s.total_lines > 5_000, "{s:?}"); // 8098
                                             // §6/`.33`: output kept, then an error.
    assert!(s.lines_then_error > 500, "{s:?}"); // 919
                                                // Every error class a *program* (rather than a broken engine) can cause.
    assert!(s.type_err > 200, "{s:?}"); // 477
    assert!(s.name > 20, "{s:?}"); // 117
    assert!(s.value > 200, "{s:?}"); // 848
                                     // The three seams most likely to diverge.
    assert!(
        s.depth_limit > 50,
        "the recursion limit was never hit: {s:?}"
    ); // 121
    assert!(s.overflow > 100, "no i64 boundary was crossed: {s:?}"); // 508
    assert!(s.div_zero > 50, "no division or modulo by zero: {s:?}"); // 120
                                                                      // Short-circuiting is only observable through a side effect, so a corpus
                                                                      // where `se` never ran cannot have tested it either way.
    assert!(s.side_effect_seen > 300, "{s:?}"); // 802
}

/// §3: "nothing observable may carry over between two `run` calls". The sweep
/// reuses one instance of each engine for speed, which would hide a leak *and*
/// could invent a divergence no single program reproduces — so check the reuse
/// directly, against fresh instances.
#[test]
fn nothing_carries_over_between_runs() {
    on_big_stack(|| {
        let mut reused = engines();
        for &seed in SEEDS.iter().take(6) {
            let mut rng = StdRng::seed_from_u64(seed);
            for i in 0..FLAVORS.len() * 2 {
                let src = generate(&mut rng, FLAVORS[i % FLAVORS.len()]);
                for (idx, fresh) in engines().iter_mut().enumerate() {
                    let a = reused[idx].run(&src);
                    let b = fresh.run(&src);
                    assert!(
                        a == b,
                        "{} differs between a reused and a fresh instance\n{}\n{src}",
                        fresh.name(),
                        diff("reused", &a, "fresh", &b)
                    );
                }
            }
        }
    });
}

/// The nesting the generator emits must stay far from the recursive parser's
/// stack limit — an overflow there is an *abort*, which no report survives (bead
/// `.67`). This runs the deepest thing `Flavor::Nested` can produce on a
/// default-sized test thread on purpose: if it survives here, the 64 MiB sweep
/// thread has room to spare.
#[test]
fn nesting_the_generator_emits_is_safe_on_a_default_stack() {
    let mut engines = engines();
    for &seed in SEEDS.iter().take(8) {
        let mut rng = StdRng::seed_from_u64(seed);
        for _ in 0..8 {
            let src = generate(&mut rng, Flavor::Nested);
            assert!(compare(&mut engines, &src).is_ok(), "{src}");
        }
    }
}

/// The shrinker, against a planted disagreement: two fakes that differ only on a
/// program containing `boom`. It must peel every other line away.
#[test]
fn the_shrinker_reduces_a_planted_divergence() {
    struct Fake(&'static str, bool);
    impl Engine for Fake {
        fn name(&self) -> &'static str {
            self.0
        }
        fn run(&mut self, src: &str) -> Output {
            let mut out = Output::new();
            if src.contains("boom") && self.1 {
                out.push_line("different");
            }
            out
        }
    }
    let mut fakes: Vec<Box<dyn Engine>> =
        vec![Box::new(Fake("l", false)), Box::new(Fake("r", true))];
    let src = "let a = 1;\nprint a;\nlet b = boom(2);\nprint b;\nprint 3;\n";
    assert_eq!(shrink(&mut fakes, src), "let b = boom(2);\n");

    // And it leaves an agreeing program alone rather than deleting it to
    // nothing. Nothing here calls `shrink` on an agreeing pair, but a shrinker
    // that "minimises" a non-failure is how a report loses its repro.
    let agree = "print 1;\n";
    let mut same: Vec<Box<dyn Engine>> =
        vec![Box::new(Fake("l", false)), Box::new(Fake("r", false))];
    assert_eq!(shrink(&mut same, agree), agree);
}

/// A panic must be reported, not propagated — the guard the §4 assertion rests
/// on. Checked against a fake that panics, because no real engine may.
#[test]
fn a_panicking_engine_is_reported_rather_than_taking_the_suite_down() {
    struct Bomb;
    impl Engine for Bomb {
        fn name(&self) -> &'static str {
            "bomb"
        }
        fn run(&mut self, _src: &str) -> Output {
            panic!("boom in the engine")
        }
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let got = run_guarded(&mut Bomb, "print 1;");
    let mut one: Vec<Box<dyn Engine>> = vec![Box::new(Bomb)];
    let report = compare(&mut one, "print 1;");
    std::panic::set_hook(prev);

    assert_eq!(got.err().as_deref(), Some("boom in the engine"));
    let report = report.expect_err("a panicking engine must be a failure");
    assert!(report.contains("PANICKED"), "{report}");
    assert!(report.contains("bomb"), "{report}");
}

/// Bead `.43`: nothing in the language bounds a `while`, so the *generator* is
/// where the bound has to come from, and this is the assertion that it is
/// actually there. A generated infinite loop would hang the suite with no
/// output at all.
#[test]
fn every_generated_while_is_counter_bounded() {
    for &seed in SEEDS {
        let mut rng = StdRng::seed_from_u64(seed);
        for &flavor in FLAVORS {
            let src = generate(&mut rng, flavor);
            for (n, text) in src.lines().enumerate() {
                let Some(rest) = text.trim().strip_prefix("while ") else {
                    continue;
                };
                // Every loop is `while _cN < <literal> {`.
                let (var, bound) = rest
                    .split_once(" < ")
                    .unwrap_or_else(|| panic!("unbounded while at line {} of\n{src}", n + 1));
                assert!(var.starts_with("_c"), "loop over {var}:\n{src}");
                let bound = bound.trim_end_matches(" {");
                assert!(bound.parse::<u32>().is_ok(), "bound {bound}:\n{src}");
                // A counter appears on the left of `=` exactly twice: the
                // `let _cN = 0;` and the increment `while_stmt` appends itself.
                // Nothing generated can reach the name, because it is never
                // declared into `Gen::scopes`.
                assert_eq!(
                    src.matches(&format!("{var} = ")).count(),
                    2,
                    "the counter {var} is written somewhere else too:\n{src}"
                );
                assert_eq!(src.matches(&format!("let {var} = 0;")).count(), 1, "{src}");
            }
        }
    }
}

/// Same seed in, same program out — the property the fixed seed list buys.
/// Without it, a `HashMap` iteration or an `Instant` sneaking into the generator
/// would make every failure unreproducible and nobody would notice until the
/// first one mattered.
#[test]
fn generation_is_reproducible_from_the_seed() {
    for &seed in SEEDS.iter().take(8) {
        for &flavor in FLAVORS {
            let a = generate(&mut StdRng::seed_from_u64(seed), flavor);
            let b = generate(&mut StdRng::seed_from_u64(seed), flavor);
            assert_eq!(a, b, "seed 0x{seed:016x} flavor {flavor:?}");
        }
    }
}
