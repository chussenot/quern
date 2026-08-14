//! bead: treadle-error — FROZEN: TreadleError taxonomy + message constructors. §3
//!
//! §4 makes the **message text** a contract, not just the variant: both engines
//! must produce the same variant, the same line and the same wording for the
//! same program, or the differential fuzzer reports a divergence that is really
//! just two spellings of the same error. So every message in the language is
//! built by a constructor in this module, and **no call site formats an error
//! string by hand**. If you need a message that has no constructor here, add one
//! here rather than a `format!` at the call site — that is the whole mechanism.
//!
//! The wording of the arithmetic, comparison and truth errors is shared with
//! `value.rs`, which is the single place either engine does arithmetic. The
//! `value_rs_agrees_with_our_constructors` test at the bottom of this file
//! asserts the two agree **byte for byte**, so the drift §4 forbids fails a test
//! here rather than surfacing as a mystery fuzzer failure later.
//!
//! `Display` is the form the `.tr` conformance corpus compares as text (§5):
//!
//! ```text
//! error: Value at line 3: divide by zero
//! ```

use std::fmt;

/// The error taxonomy. **FROZEN, §3** — the shape below is verbatim from the
/// spec; the derives are additive and do not change it.
///
/// `PartialEq` is load-bearing rather than convenience: `Output` carries an
/// `Option<TreadleError>` and the fuzzer's whole oracle is comparing two
/// `Output`s for equality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreadleError {
    /// Bad byte, bad escape, unterminated string.
    Lex { line: u32, msg: String },
    /// Structurally invalid program, including `return` outside a function (§2).
    Parse { line: u32, msg: String },
    /// Wrong operand type, wrong arity.
    Type { line: u32, msg: String },
    /// Unknown variable or function.
    Name { line: u32, msg: String },
    /// Divide by zero, bad `int()`, overflow, recursion limit.
    Value { line: u32, msg: String },
}

/// **FROZEN, §3.**
pub type Result<T> = std::result::Result<T, TreadleError>;

/// The recursion limit, §6 (bead `.36`): the one place the literal `1000` lives.
///
/// Both engines import this rather than holding their own copy, so neither can
/// count to a different number and neither can spell the message differently.
/// The counted quantity is **active invocations**, not stack frames: the
/// top-level program is depth 0, the check is `depth == MAX_DEPTH` at the *call
/// site* after arguments, name resolution and arity, so 1000 nested invocations
/// succeed and the 1001st fails. Builtins do not consume depth.
pub const MAX_DEPTH: usize = 1000;

impl TreadleError {
    // ---- accessors -------------------------------------------------------

    /// The variant name as it appears in the `Display` form: `Lex`, `Parse`,
    /// `Type`, `Name`, `Value`.
    pub fn variant(&self) -> &'static str {
        match self {
            TreadleError::Lex { .. } => "Lex",
            TreadleError::Parse { .. } => "Parse",
            TreadleError::Type { .. } => "Type",
            TreadleError::Name { .. } => "Name",
            TreadleError::Value { .. } => "Value",
        }
    }

    /// The line the error is reported on. Part of the observable output, so the
    /// two engines must agree on it (§3).
    pub fn line(&self) -> u32 {
        match self {
            TreadleError::Lex { line, .. }
            | TreadleError::Parse { line, .. }
            | TreadleError::Type { line, .. }
            | TreadleError::Name { line, .. }
            | TreadleError::Value { line, .. } => *line,
        }
    }

    /// The message body, without the `error: <Variant> at line <n>: ` prefix.
    pub fn msg(&self) -> &str {
        match self {
            TreadleError::Lex { msg, .. }
            | TreadleError::Parse { msg, .. }
            | TreadleError::Type { msg, .. }
            | TreadleError::Name { msg, .. }
            | TreadleError::Value { msg, .. } => msg,
        }
    }

    // ---- Lex -------------------------------------------------------------

    /// A string literal with no closing quote.
    pub fn unterminated_string(line: u32) -> TreadleError {
        TreadleError::Lex {
            line,
            msg: "unterminated string".to_string(),
        }
    }

    /// A backslash escape the lexer does not recognise, e.g. `\q`.
    ///
    /// `ch` is the raw character that followed the backslash, so it may itself
    /// be a newline or a tab — a trailing backslash before a line break reaches
    /// us as `'\n'`. It is escaped rather than interpolated raw, because §5
    /// compares an error as **one line** of text: a literal newline in the
    /// message would split a corpus expectation across two lines and no `.tr`
    /// case could express it. `\q` stays `\q`; a real newline prints as `\n`.
    pub fn unknown_escape(line: u32, ch: char) -> TreadleError {
        let shown = ch.escape_default().to_string();
        // `escape_default` already prefixes the ones it rewrites (`\n`, `\t`,
        // `\\`, `\"`); a plain char comes back bare and needs the backslash.
        let shown = if shown.starts_with('\\') {
            shown
        } else {
            format!("\\{shown}")
        };
        TreadleError::Lex {
            line,
            msg: format!("unknown escape {shown}"),
        }
    }

    /// A byte that cannot begin any token.
    pub fn unexpected_char(line: u32, ch: char) -> TreadleError {
        TreadleError::Lex {
            line,
            msg: format!("unexpected character '{ch}'"),
        }
    }

    /// An integer literal in the **source** that does not fit in an `i64`.
    ///
    /// This is the lexer's case and it is a `Lex` error. It is deliberately NOT
    /// [`TreadleError::bad_int`]: that one is the runtime `int()` builtin
    /// failing on a string (§3 files `bad int()` under `Value`). The two look
    /// similar and are different stages — a program containing `99999999999999999999`
    /// never runs at all, whereas `int("abc")` fails partway through a run that
    /// has already printed output.
    pub fn int_literal_out_of_range(line: u32, text: &str) -> TreadleError {
        TreadleError::Lex {
            line,
            msg: format!("integer literal out of range: {text}"),
        }
    }

    // ---- Parse -----------------------------------------------------------

    /// The parser wanted `want` and found `found`. Both should be described in
    /// the same vocabulary the source uses, e.g. `expected(line, "';'", "'}'")`.
    pub fn expected(line: u32, want: &str, found: &str) -> TreadleError {
        TreadleError::Parse {
            line,
            msg: format!("expected {want}, found {found}"),
        }
    }

    /// A token that cannot appear where it was found. Covers `x = (y = 1)`,
    /// since `=` is a statement token and not an expression operator (§2).
    pub fn unexpected_token(line: u32, found: &str) -> TreadleError {
        TreadleError::Parse {
            line,
            msg: format!("unexpected {found}"),
        }
    }

    /// §2: `return` outside a function is a **parse** error, not a runtime one.
    pub fn return_outside_fn(line: u32) -> TreadleError {
        TreadleError::Parse {
            line,
            msg: "return outside of a function".to_string(),
        }
    }

    // ---- Type ------------------------------------------------------------

    /// A binary operator applied to the wrong operand types. `want` names what
    /// the operator accepts and `lhs`/`rhs` are `Value::type_name()` results:
    ///
    /// ```text
    /// + expects Int or Str operands, got Int and Bool
    /// < expects two Int or two Str operands, got Bool and Bool
    /// ```
    pub fn type_mismatch(line: u32, op: &str, want: &str, lhs: &str, rhs: &str) -> TreadleError {
        TreadleError::Type {
            line,
            msg: format!("{op} expects {want} operands, got {lhs} and {rhs}"),
        }
    }

    /// A prefix operator applied to the wrong operand type:
    /// `- expects a Int operand, got Bool`.
    pub fn unary_type_mismatch(line: u32, op: &str, want: &str, got: &str) -> TreadleError {
        TreadleError::Type {
            line,
            msg: format!("{op} expects a {want} operand, got {got}"),
        }
    }

    /// `==` / `!=` across two different types. §2 makes this an error rather
    /// than `false`, so it needs its own wording:
    /// `== expects two values of the same type, got Int and Str`.
    pub fn eq_type_mismatch(line: u32, lhs: &str, rhs: &str) -> TreadleError {
        TreadleError::Type {
            line,
            msg: format!("== expects two values of the same type, got {lhs} and {rhs}"),
        }
    }

    /// A non-`Bool` used as a condition by `if`, `while`, `and`, `or` or `!`.
    /// There is no truthiness in this language (§2), so this is an error and not
    /// a coercion: `expected Bool, got Int`.
    pub fn not_bool(line: u32, got: &str) -> TreadleError {
        TreadleError::Type {
            line,
            msg: format!("expected Bool, got {got}"),
        }
    }

    /// Wrong argument count (§2, a runtime error; §3 files arity under `Type`).
    pub fn wrong_arity(line: u32, name: &str, expected: usize, actual: usize) -> TreadleError {
        TreadleError::Type {
            line,
            msg: format!(
                "{name} expects {expected} {}, got {actual}",
                plural_args(expected)
            ),
        }
    }

    // ---- Name ------------------------------------------------------------

    /// A variable that was never bound.
    pub fn undefined_name(line: u32, name: &str) -> TreadleError {
        TreadleError::Name {
            line,
            msg: format!("undefined variable '{name}'"),
        }
    }

    /// A call to a name that is not one of the three builtins and not a
    /// declared function. Functions are hoisted (§2), so this really is unknown.
    pub fn undefined_function(line: u32, name: &str) -> TreadleError {
        TreadleError::Name {
            line,
            msg: format!("undefined function '{name}'"),
        }
    }

    /// §2: assignment without `let` walks outward to the nearest binding and is
    /// an error if there is none. Distinct wording from [`Self::undefined_name`]
    /// so the corpus can tell a bad read from a bad write.
    pub fn assign_unbound(line: u32, name: &str) -> TreadleError {
        TreadleError::Name {
            line,
            msg: format!("assignment to undefined variable '{name}'"),
        }
    }

    // ---- Value -----------------------------------------------------------
    //
    // The three below are the wording `value.rs` already ships; they are
    // adopted here verbatim rather than respelled. See the agreement test.

    /// `i64` overflow on `+ - *` or unary `-` (§4: a `Value` error, never a
    /// wrap and never a panic).
    pub fn overflow(line: u32) -> TreadleError {
        TreadleError::Value {
            line,
            msg: "integer overflow".to_string(),
        }
    }

    pub fn divide_by_zero(line: u32) -> TreadleError {
        TreadleError::Value {
            line,
            msg: "divide by zero".to_string(),
        }
    }

    pub fn modulo_by_zero(line: u32) -> TreadleError {
        TreadleError::Value {
            line,
            msg: "modulo by zero".to_string(),
        }
    }

    /// The `int()` builtin on a string that is not a valid `i64` (§2).
    pub fn bad_int(line: u32, text: &str) -> TreadleError {
        TreadleError::Value {
            line,
            msg: format!("not a valid integer: {text}"),
        }
    }

    /// Deep recursion, §4 and §6 (`.36`): a `Value` error **naming the limit**,
    /// never a stack overflow — a stack overflow aborts the process, and §4
    /// makes that a bug rather than an error.
    ///
    /// Takes no limit argument on purpose: the number comes from [`MAX_DEPTH`],
    /// so one engine cannot report a different one. `line` is the line of the
    /// failing **call** expression.
    pub fn recursion_limit(line: u32) -> TreadleError {
        TreadleError::Value {
            line,
            msg: format!("recursion limit of {MAX_DEPTH} frames exceeded"),
        }
    }

    // ---- not a language error --------------------------------------------

    /// An invariant **inside an engine** broke — a malformed chunk, a stack
    /// underflow, a bad slot (§6, `.45`).
    ///
    /// This is NOT a language error and is documented as unreachable: no treadle
    /// program can cause one. If it ever appears in an `Output`, the engine that
    /// produced it has a bug and the fuzzer's divergence report is **correct**.
    /// It exists so that neither engine reaches for a `format!` when it finds a
    /// broken invariant — the VM can hit cases the tree-walker cannot, and
    /// `internal error:` makes that unmistakable in a divergence dump and
    /// greppable in CI.
    pub fn internal(line: u32, what: &str) -> TreadleError {
        TreadleError::Type {
            line,
            msg: format!("internal error: {what}"),
        }
    }
}

/// `1` takes `argument`, everything else takes `arguments`. Kept here so both
/// engines pluralise identically — the point of §4 is that neither one decides.
fn plural_args(n: usize) -> &'static str {
    if n == 1 {
        "argument"
    } else {
        "arguments"
    }
}

/// The corpus form (§5): `error: <Variant> at line <n>: <msg>`. The `.tr` cases
/// compare this as text, so it is byte-exact and must not gain or lose a
/// character.
impl fmt::Display for TreadleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "error: {} at line {}: {}",
            self.variant(),
            self.line(),
            self.msg()
        )
    }
}

impl std::error::Error for TreadleError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    const L: u32 = 7;

    /// The exact line printed in §5 of the spec. If this test changes, every
    /// `.tr` expectation in the corpus changes with it.
    #[test]
    fn display_is_the_corpus_form_verbatim() {
        assert_eq!(
            TreadleError::divide_by_zero(3).to_string(),
            "error: Value at line 3: divide by zero"
        );
    }

    #[test]
    fn display_names_every_variant() {
        assert_eq!(
            TreadleError::unterminated_string(1).to_string(),
            "error: Lex at line 1: unterminated string"
        );
        assert_eq!(
            TreadleError::return_outside_fn(2).to_string(),
            "error: Parse at line 2: return outside of a function"
        );
        assert_eq!(
            TreadleError::not_bool(3, "Int").to_string(),
            "error: Type at line 3: expected Bool, got Int"
        );
        assert_eq!(
            TreadleError::undefined_name(4, "x").to_string(),
            "error: Name at line 4: undefined variable 'x'"
        );
        assert_eq!(
            TreadleError::overflow(5).to_string(),
            "error: Value at line 5: integer overflow"
        );
    }

    /// **This is the §4 test.** `value.rs` builds `TreadleError` as struct
    /// literals for the arithmetic, comparison and truth paths. If its wording
    /// and this module's constructors ever diverge, the two engines can report
    /// the same failure with two different strings and the fuzzer will blame
    /// whichever it ran second. Pin them together instead.
    #[test]
    fn value_rs_agrees_with_our_constructors() {
        assert_eq!(
            Value::Int(1).div(&Value::Int(0), L).unwrap_err(),
            TreadleError::divide_by_zero(L)
        );
        assert_eq!(
            Value::Int(1).rem(&Value::Int(0), L).unwrap_err(),
            TreadleError::modulo_by_zero(L)
        );
        assert_eq!(
            Value::Int(i64::MAX).add(&Value::Int(1), L).unwrap_err(),
            TreadleError::overflow(L)
        );
        assert_eq!(
            Value::Int(i64::MIN).neg(L).unwrap_err(),
            TreadleError::overflow(L)
        );
        // §6 (`.41`): the three i64::MIN edges are Value *overflow*, not panics
        // — a bare `/` or `%` would abort the process here, which §4 forbids.
        assert_eq!(
            Value::Int(i64::MIN).div(&Value::Int(-1), L).unwrap_err(),
            TreadleError::overflow(L)
        );
        assert_eq!(
            Value::Int(i64::MIN).rem(&Value::Int(-1), L).unwrap_err(),
            TreadleError::overflow(L)
        );
        // And a zero divisor outranks overflow, so i64::MIN / 0 is not "integer
        // overflow" — the two engines must agree on which check comes first.
        assert_eq!(
            Value::Int(i64::MIN).div(&Value::Int(0), L).unwrap_err(),
            TreadleError::divide_by_zero(L)
        );
        assert_eq!(
            Value::Int(i64::MIN).rem(&Value::Int(0), L).unwrap_err(),
            TreadleError::modulo_by_zero(L)
        );
        assert_eq!(
            Value::Int(1).add(&Value::Bool(true), L).unwrap_err(),
            TreadleError::type_mismatch(L, "+", "Int or Str", "Int", "Bool")
        );
        assert_eq!(
            Value::Nil.sub(&Value::Int(1), L).unwrap_err(),
            TreadleError::type_mismatch(L, "-", "Int", "Nil", "Int")
        );
        assert_eq!(
            Value::Bool(true)
                .cmp_value(&Value::Bool(false), "<", L)
                .unwrap_err(),
            TreadleError::type_mismatch(L, "<", "two Int or two Str", "Bool", "Bool")
        );
        assert_eq!(
            Value::Bool(true).neg(L).unwrap_err(),
            TreadleError::unary_type_mismatch(L, "-", "Int", "Bool")
        );
        assert_eq!(
            Value::Int(1).eq_value(&Value::str("1"), L).unwrap_err(),
            TreadleError::eq_type_mismatch(L, "Int", "Str")
        );
        assert_eq!(
            Value::Int(0).as_bool(L).unwrap_err(),
            TreadleError::not_bool(L, "Int")
        );
    }

    #[test]
    fn arity_pluralises_on_one() {
        assert_eq!(
            TreadleError::wrong_arity(L, "add", 1, 2).msg(),
            "add expects 1 argument, got 2"
        );
        assert_eq!(
            TreadleError::wrong_arity(L, "add", 2, 0).msg(),
            "add expects 2 arguments, got 0"
        );
        assert_eq!(
            TreadleError::wrong_arity(L, "f", 0, 3).msg(),
            "f expects 0 arguments, got 3"
        );
    }

    /// §5 compares an error as one line of text, so no message may contain a
    /// raw control character. The lexer calls `unknown_escape` with whatever
    /// followed the backslash, including a literal newline.
    #[test]
    fn no_error_message_can_contain_a_raw_newline() {
        for ch in ['\n', '\r', '\t', '\\', '"', '\0'] {
            let e = TreadleError::unknown_escape(L, ch);
            assert!(
                !e.msg().contains('\n') && !e.msg().contains('\r') && !e.msg().contains('\t'),
                "raw control char leaked into message for {ch:?}: {:?}",
                e.msg()
            );
            assert_eq!(e.to_string().lines().count(), 1);
        }
        assert_eq!(
            TreadleError::unknown_escape(2, '\n').to_string(),
            "error: Lex at line 2: unknown escape \\n"
        );
        assert_eq!(
            TreadleError::unknown_escape(2, '\t').msg(),
            "unknown escape \\t"
        );
    }

    #[test]
    fn accessors_match_the_constructed_variant() {
        let e = TreadleError::recursion_limit(42);
        assert_eq!(e.variant(), "Value");
        assert_eq!(e.line(), 42);
        assert_eq!(e.msg(), "recursion limit of 1000 frames exceeded");
        assert_eq!(
            e.to_string(),
            "error: Value at line 42: recursion limit of 1000 frames exceeded"
        );
    }

    /// §6 (`.36`): the literal 1000 lives in exactly one place, and the message
    /// is built from it — so an engine counting to a different number cannot
    /// also report a consistent message.
    #[test]
    fn recursion_message_is_built_from_max_depth() {
        assert_eq!(MAX_DEPTH, 1000);
        assert!(TreadleError::recursion_limit(1)
            .msg()
            .contains(&MAX_DEPTH.to_string()));
    }

    /// §6 (`.45`): a broken engine invariant is a `Type` error reading
    /// `internal error: <what>`, and no treadle program can produce one.
    #[test]
    fn internal_is_a_type_error_and_greppable() {
        let e = TreadleError::internal(4, "stack underflow");
        assert_eq!(e.variant(), "Type");
        assert_eq!(
            e.to_string(),
            "error: Type at line 4: internal error: stack underflow"
        );
    }

    #[test]
    fn lex_and_name_wording() {
        assert_eq!(
            TreadleError::unknown_escape(L, 'q').msg(),
            "unknown escape \\q"
        );
        assert_eq!(
            TreadleError::int_literal_out_of_range(L, "99999999999999999999").msg(),
            "integer literal out of range: 99999999999999999999"
        );
        assert_eq!(
            TreadleError::unexpected_char(L, '@').msg(),
            "unexpected character '@'"
        );
        assert_eq!(
            TreadleError::bad_int(L, "12x").msg(),
            "not a valid integer: 12x"
        );
        assert_eq!(
            TreadleError::undefined_function(L, "foo").msg(),
            "undefined function 'foo'"
        );
        assert_eq!(
            TreadleError::assign_unbound(L, "y").msg(),
            "assignment to undefined variable 'y'"
        );
        assert_eq!(
            TreadleError::expected(L, "';'", "'}'").msg(),
            "expected ';', found '}'"
        );
        assert_eq!(
            TreadleError::unexpected_token(L, "'='").msg(),
            "unexpected '='"
        );
    }
}
