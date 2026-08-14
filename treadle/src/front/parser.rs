//! bead: treadle-parser — Tokens -> Program, precedence per §2
//!
//! This file is shared by two beads. **The expression half is here** (bead
//! `.11`): the [`Cursor`] over the token vector and the §2 precedence ladder
//! down to primaries. The statement half (bead `.12`) builds `Stmt`/`FnDecl`/
//! `Program` on top of the same [`Cursor`] — there is exactly **one** cursor
//! type in this crate, deliberately, so the two halves cannot disagree about
//! where the token stream is.
//!
//! Three rules that shaped the code below, all §6:
//!
//! - **`=` is a statement token, not an expression operator** (§2). Nothing in
//!   the ladder consumes [`TokenKind::Eq`], so `x = (y = 1)` and
//!   `if (x = 1) { }` are `Parse` errors and no `Expr` can be an assignment.
//! - **Left to right, always** (`.33`). Call arguments are pushed in source
//!   order and a `Binary` node's `lhs` is the earlier text, because both
//!   engines walk this AST in the order it is built.
//! - **`i64::MIN` is not a literal** (`.41`). `-9223372036854775808` is two
//!   tokens and the lexer rejects the magnitude, so we do **not** fold
//!   `Minus` + `Int` here; see the note on [`unary`].
//!
//! Every message is a constructor from `error.rs` (§4): the parser never
//! formats an error string, it only names the token it found.

// The statement half (bead `.12`) is the in-crate consumer of everything below,
// and it has not landed yet, so on the lib target alone every item here is
// unreachable and `dead_code` fires under `-D warnings`. Delete this attribute
// with that bead — nothing else in the file depends on it.
#![allow(dead_code)]

use crate::error::{Result, TreadleError};
use crate::front::ast::{BinOp, Expr, UnOp};
use crate::front::token::{Token, TokenKind};
use crate::value::Value;

/// How a token is named inside a `Parse` message. `error.rs` wants the
/// source-level vocabulary already quoted (`expected(line, "';'", "'}'")`), but
/// `Eof` reads as prose — "expected ';', found end of input" — so it is the one
/// kind that is not quoted. One place, so the two parser halves cannot drift.
pub(crate) fn describe(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Eof => kind.to_string(),
        _ => format!("'{kind}'"),
    }
}

/// A position in the token vector.
///
/// `tokenize` guarantees a trailing [`TokenKind::Eof`], and [`Cursor::advance`]
/// refuses to move past it, so every accessor is infallible: this type cannot
/// index out of bounds and cannot panic on a truncated program.
pub(crate) struct Cursor {
    toks: Vec<Token>,
    pos: usize,
}

impl Cursor {
    /// Takes the vector `front::lexer::tokenize` returned, `Eof` included.
    pub(crate) fn new(toks: Vec<Token>) -> Cursor {
        debug_assert!(
            matches!(toks.last().map(|t| &t.kind), Some(TokenKind::Eof)),
            "tokenize always ends the stream with Eof"
        );
        Cursor { toks, pos: 0 }
    }

    /// The token about to be consumed. At the end of input this is the `Eof`
    /// token, whose `line` is the line the last real token ended on, so an
    /// error at end of input has a line number like any other.
    pub(crate) fn peek(&self) -> &Token {
        // `pos` is clamped by `advance`, so the index is always in range.
        &self.toks[self.pos]
    }

    /// Consumes and returns the current token. Named `advance`, not `next`:
    /// `next` would trip `clippy::should_implement_trait` on a type that is not
    /// an `Iterator`.
    pub(crate) fn advance(&mut self) -> Token {
        let tok = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        tok
    }

    /// True once the stream is exhausted — i.e. sitting on `Eof`.
    pub(crate) fn at_end(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }

    /// Consumes the current token if it is `kind`, reporting whether it did.
    pub(crate) fn eat(&mut self, kind: &TokenKind) -> bool {
        if &self.peek().kind == kind {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Consumes `kind` or fails with `expected <kind>, found <what is there>`.
    pub(crate) fn expect(&mut self, kind: &TokenKind) -> Result<Token> {
        if &self.peek().kind == kind {
            Ok(self.advance())
        } else {
            Err(self.expected(&describe(kind)))
        }
    }

    /// Consumes an identifier, yielding its name and line. Used for variable
    /// names, parameter names and `fn` names, so it lives with the cursor
    /// rather than in either half.
    pub(crate) fn ident(&mut self) -> Result<(String, u32)> {
        if let TokenKind::Ident(_) = self.peek().kind {
            let tok = self.advance();
            match tok.kind {
                TokenKind::Ident(name) => Ok((name, tok.line)),
                _ => unreachable!("guarded by the peek above"),
            }
        } else {
            Err(self.expected("identifier"))
        }
    }

    /// A `Parse` error at the current token: "expected `want`, found X". `want`
    /// is passed in the source's own vocabulary and already quoted when it is a
    /// literal token (`"';'"`), bare when it is a category (`"identifier"`).
    ///
    /// **`=` is reported as `unexpected '='` whatever was wanted**, and that is
    /// a contract, not a convenience: corpus case `318` pins `x = (y = 1);` to
    /// `error: Parse at line 3: unexpected '='`, and `error.rs`'s own doc on
    /// `unexpected_token` names that program. Since `=` is a statement token
    /// (§2) it is never valid anywhere the parser can ask for something, so the
    /// rule belongs here — one branch — rather than at each `expect` call in
    /// either half.
    pub(crate) fn expected(&self, want: &str) -> TreadleError {
        let tok = self.peek();
        if tok.kind == TokenKind::Eq {
            return TreadleError::unexpected_token(tok.line, &describe(&tok.kind));
        }
        TreadleError::expected(tok.line, want, &describe(&tok.kind))
    }
}

/// The §2 operator table as a level -> `BinOp` lookup, loosest level first.
///
/// Levels 1..=[`MAX_BIN_LEVEL`] are exactly the table's rows 1..6; row 7
/// (prefix `-` `!`) is [`unary`] and binds tighter than all of them.
fn binop_at(level: u32, kind: &TokenKind) -> Option<BinOp> {
    Some(match (level, kind) {
        (1, TokenKind::Or) => BinOp::Or,
        (2, TokenKind::And) => BinOp::And,
        (3, TokenKind::EqEq) => BinOp::Eq,
        (3, TokenKind::BangEq) => BinOp::Ne,
        (4, TokenKind::Lt) => BinOp::Lt,
        (4, TokenKind::Gt) => BinOp::Gt,
        (4, TokenKind::LtEq) => BinOp::Le,
        (4, TokenKind::GtEq) => BinOp::Ge,
        (5, TokenKind::Plus) => BinOp::Add,
        (5, TokenKind::Minus) => BinOp::Sub,
        (6, TokenKind::Star) => BinOp::Mul,
        (6, TokenKind::Slash) => BinOp::Div,
        (6, TokenKind::Percent) => BinOp::Rem,
        _ => return None,
    })
}

/// The tightest binary row of the §2 table (`* / %`).
const MAX_BIN_LEVEL: u32 = 6;

/// Parses one expression. **The entry point for the statement half**: every
/// `let` initialiser, assignment value, `print` argument, condition and
/// `return` value is one of these.
///
/// Leaves the cursor on the first token that is not part of the expression, so
/// the caller checks for its own `;`, `{` or `,`.
pub(crate) fn parse_expr(c: &mut Cursor) -> Result<Expr> {
    binary(c, 1)
}

/// One rung of the ladder: parse the next-tighter level, then fold as many
/// same-level operators as follow. Folding in a loop (rather than recursing on
/// the right) is what makes every binary operator **left**-associative, so
/// `1 - 2 - 3` is `(1 - 2) - 3`.
fn binary(c: &mut Cursor, level: u32) -> Result<Expr> {
    if level > MAX_BIN_LEVEL {
        return unary(c);
    }
    let mut lhs = binary(c, level + 1)?;
    while let Some(op) = binop_at(level, &c.peek().kind) {
        // The node's line is the operator's, per §6 `.46`: an error is reported
        // at the innermost node that failed, and for `a\n+ b` that is the `+`.
        let line = c.advance().line;
        let rhs = binary(c, level + 1)?;
        lhs = Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            line,
        };
    }
    Ok(lhs)
}

/// Row 7: prefix `-` and `!`, right-associative, so `--x` and `!!b` chain.
///
/// **No `Minus`+`Int` folding**, and that is a decision, not an omission. §6
/// `.41` is normative: an integer literal is ASCII digits through
/// `i64::from_str` and out of range is a `Lex` error, so `i64::MIN` is
/// reachable "only through arithmetic". Folding here would make
/// `-9223372036854775808` a literal and contradict it — and `-i64::MIN` is
/// itself a `Value` overflow error, so the fold would have to invent an
/// asymmetry the spec does not have. `-1` is therefore always
/// `Unary { Neg, Lit(1) }`.
fn unary(c: &mut Cursor) -> Result<Expr> {
    let op = match c.peek().kind {
        TokenKind::Minus => UnOp::Neg,
        TokenKind::Bang => UnOp::Not,
        _ => return primary(c),
    };
    let line = c.advance().line;
    let rhs = unary(c)?;
    Ok(Expr::Unary {
        op,
        rhs: Box::new(rhs),
        line,
    })
}

/// A literal, a variable, a call, or a parenthesised expression.
///
/// Parentheses produce **no node** — they only group, and the frozen `Expr` has
/// no variant for them — so `(1 + 2) * 3` and a hypothetical `Group` node are
/// the same tree.
fn primary(c: &mut Cursor) -> Result<Expr> {
    let line = c.peek().line;
    match c.peek().kind {
        TokenKind::Int(_) | TokenKind::Str(_) | TokenKind::Bool(_) | TokenKind::Nil => {
            Ok(Expr::Lit(match c.advance().kind {
                TokenKind::Int(n) => Value::Int(n),
                TokenKind::Str(s) => Value::str(s),
                TokenKind::Bool(b) => Value::Bool(b),
                _ => Value::Nil,
            }))
        }
        TokenKind::Ident(_) => {
            let (name, line) = c.ident()?;
            // A call target is always a bare identifier: §2 has no first-class
            // functions, so there is no callee expression to parse.
            if c.eat(&TokenKind::LParen) {
                Ok(Expr::Call {
                    name,
                    args: call_args(c)?,
                    line,
                })
            } else {
                Ok(Expr::Var { name, line })
            }
        }
        TokenKind::LParen => {
            c.advance();
            let inner = parse_expr(c)?;
            c.expect(&TokenKind::RParen)?;
            Ok(inner)
        }
        // Everything else, `=` and `end of input` included. This arm is the
        // whole of "there is no assignment expression": the ladder never asks
        // for `Eq`, so `(y = 1)` fails at the `)` we expected and a bare `= 1`
        // fails here.
        _ => Err(TreadleError::unexpected_token(
            line,
            &describe(&c.peek().kind),
        )),
    }
}

/// The argument list of a call, `(` already consumed. Arguments are pushed in
/// **source order** (§6 `.33`: call arguments evaluate left to right, and both
/// engines walk this `Vec` in order).
///
/// A trailing comma is not accepted (§6 `.46`): after a `,` an expression is
/// required, so `f(1,)` fails on the `)` with "unexpected ')'".
fn call_args(c: &mut Cursor) -> Result<Vec<Expr>> {
    let mut args = Vec::new();
    if c.eat(&TokenKind::RParen) {
        return Ok(args);
    }
    loop {
        args.push(parse_expr(c)?);
        if !c.eat(&TokenKind::Comma) {
            break;
        }
    }
    c.expect(&TokenKind::RParen)?;
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::front::lexer::tokenize;

    /// Parse one expression from source and require the whole source to be it.
    fn expr(src: &str) -> Expr {
        let mut c = Cursor::new(tokenize(src).expect("lexes"));
        let e = parse_expr(&mut c).expect("parses");
        assert!(c.at_end(), "{src}: trailing tokens after the expression");
        e
    }

    /// Parse an expression that must fail, yielding (variant, line, message).
    fn err(src: &str) -> (&'static str, u32, String) {
        let mut c = Cursor::new(tokenize(src).expect("lexes"));
        let e = parse_expr(&mut c).expect_err("must not parse");
        (e.variant(), e.line(), e.msg().to_string())
    }

    /// A parenthesis-free rendering, so a test can assert the *shape* of a tree
    /// in one string: every `Binary`/`Unary`/`Call` node is bracketed, so the
    /// nesting is unambiguous without comparing nodes (`Expr` has no `PartialEq`
    /// and `ast` asked that nobody add one locally).
    fn shape(e: &Expr) -> String {
        match e {
            Expr::Lit(v) => format!("{v}"),
            Expr::Var { name, .. } => name.clone(),
            Expr::Unary { op, rhs, .. } => format!("({op:?} {})", shape(rhs)),
            Expr::Binary { op, lhs, rhs, .. } => {
                format!("({op:?} {} {})", shape(lhs), shape(rhs))
            }
            Expr::Call { name, args, .. } => {
                let args: Vec<String> = args.iter().map(shape).collect();
                format!("(call {name} [{}])", args.join(" "))
            }
        }
    }

    fn line_of(e: &Expr) -> u32 {
        match e {
            Expr::Lit(_) => 0,
            Expr::Var { line, .. }
            | Expr::Unary { line, .. }
            | Expr::Binary { line, .. }
            | Expr::Call { line, .. } => *line,
        }
    }

    /// The whole §2 table in one expression, loosest to tightest. Written with
    /// no parentheses, so the tree is entirely the ladder's doing.
    #[test]
    fn precedence_ladder_binds_tightest_last() {
        assert_eq!(
            shape(&expr("a or b and c == d < e + f * g")),
            "(Or a (And b (Eq c (Lt d (Add e (Mul f g))))))"
        );
        // and the other operator of each row, same shape
        assert_eq!(
            shape(&expr("a or b and c != d >= e - f % g")),
            "(Or a (And b (Ne c (Ge d (Sub e (Rem f g))))))"
        );
    }

    /// The case that changes ANSWER, not just shape, if the ladder is wrong:
    /// `2 + 3 * 4` is 14 under §2 and 20 under a flat left fold, and
    /// `1 - 2 - 3` is -4 left-associative and 2 right-associative.
    #[test]
    fn precedence_and_associativity_change_the_answer() {
        assert_eq!(shape(&expr("2 + 3 * 4")), "(Add 2 (Mul 3 4))");
        // §2 (25486ce) pins LEFT-associativity within a rung, and these are the
        // two programs it names: `1 - 2 - 3` is -4 left and 2 right, and
        // `12 / 3 / 2` is 2 left and 8 right. A ladder that recursed on the rhs
        // would pass every single-operator test above and fail these.
        assert_eq!(shape(&expr("1 - 2 - 3")), "(Sub (Sub 1 2) 3)");
        assert_eq!(shape(&expr("12 / 3 / 2")), "(Div (Div 12 3) 2)");
        assert_eq!(shape(&expr("8 / 4 / 2")), "(Div (Div 8 4) 2)");
        assert_eq!(shape(&expr("1 == 2 == 3")), "(Eq (Eq 1 2) 3)");
        // comparison is looser than arithmetic: `1 + 1 == 2` is `(1+1) == 2`,
        // which is true, not `1 + (1 == 2)`, which is a Type error.
        assert_eq!(shape(&expr("1 + 1 == 2")), "(Eq (Add 1 1) 2)");
        // `and` is tighter than `or`: `a or b and c` never gates `c` on `a`.
        assert_eq!(shape(&expr("a or b and c")), "(Or a (And b c))");
        assert_eq!(shape(&expr("a and b or c")), "(Or (And a b) c)");
    }

    /// Parentheses override the ladder and produce no node of their own.
    #[test]
    fn parens_override_precedence() {
        assert_eq!(shape(&expr("(2 + 3) * 4")), "(Mul (Add 2 3) 4)");
        assert_eq!(shape(&expr("(a or b) and c")), "(And (Or a b) c)");
        assert_eq!(shape(&expr("-(a + b)")), "(Neg (Add a b))");
        // redundant parens are invisible in the tree
        assert_eq!(shape(&expr("((((7))))")), "7");
        assert_eq!(shape(&expr("(a) + (b)")), "(Add a b)");
    }

    /// Prefix operators are row 7 — tighter than `*` — and chain to the right.
    #[test]
    fn unary_chains_and_binds_tighter_than_any_binary() {
        assert_eq!(shape(&expr("-a * b")), "(Mul (Neg a) b)");
        assert_eq!(shape(&expr("!a == b")), "(Eq (Not a) b)");
        assert_eq!(shape(&expr("---a")), "(Neg (Neg (Neg a)))");
        assert_eq!(shape(&expr("!!!b")), "(Not (Not (Not b)))");
        assert_eq!(shape(&expr("-!-x")), "(Neg (Not (Neg x)))");
        // `-` is both prefix (row 7) and binary (row 5), and `1 - -2` needs
        // both readings of it in one expression.
        assert_eq!(shape(&expr("1 - -2")), "(Sub 1 (Neg 2))");
        assert_eq!(shape(&expr("-1 - 2")), "(Sub (Neg 1) 2)");
    }

    /// All 13 `BinOp`s are produced, by the token that spells each one.
    #[test]
    fn every_binop_is_reachable_from_source() {
        let table = [
            ("a or b", BinOp::Or),
            ("a and b", BinOp::And),
            ("a == b", BinOp::Eq),
            ("a != b", BinOp::Ne),
            ("a < b", BinOp::Lt),
            ("a > b", BinOp::Gt),
            ("a <= b", BinOp::Le),
            ("a >= b", BinOp::Ge),
            ("a + b", BinOp::Add),
            ("a - b", BinOp::Sub),
            ("a * b", BinOp::Mul),
            ("a / b", BinOp::Div),
            ("a % b", BinOp::Rem),
        ];
        assert_eq!(table.len(), 13, "§2 has 13 binary operators");
        for (src, want) in table {
            match expr(src) {
                Expr::Binary { op, .. } => assert_eq!(op, want, "{src}"),
                other => panic!("{src} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn literals_and_variables() {
        assert_eq!(shape(&expr("0")), "0");
        assert_eq!(shape(&expr("9223372036854775807")), "9223372036854775807");
        assert_eq!(shape(&expr("true")), "true");
        assert_eq!(shape(&expr("false")), "false");
        assert_eq!(shape(&expr("nil")), "nil");
        // Str arrives decoded and unquoted, and `Display for Value` is unquoted
        assert_eq!(shape(&expr(r#""a\tb""#)), "a\tb");
        // case-sensitive keywords: `Let` is a variable, not a keyword
        assert_eq!(shape(&expr("Let")), "Let");
    }

    #[test]
    fn calls_take_zero_one_or_many_args() {
        assert_eq!(shape(&expr("f()")), "(call f [])");
        assert_eq!(shape(&expr("f(1)")), "(call f [1])");
        assert_eq!(shape(&expr("f(1, 2, 3)")), "(call f [1 2 3])");
        // arguments are full expressions, and nest
        assert_eq!(
            shape(&expr("f(1 + 2, g(h()), -x)")),
            "(call f [(Add 1 2) (call g [(call h [])]) (Neg x)])"
        );
        // the three builtins are ordinary calls to the parser
        assert_eq!(
            shape(&expr("len(str(int(s)))")),
            "(call len [(call str [(call int [s])])])"
        );
    }

    /// §6 `.33`: arguments are stored in source order, because both engines
    /// evaluate the `Vec` left to right and that order is observable.
    #[test]
    fn call_args_are_in_source_order() {
        match expr("f(a, b, c)") {
            Expr::Call { args, .. } => {
                let names: Vec<String> = args.iter().map(shape).collect();
                assert_eq!(names, ["a", "b", "c"]);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    /// §2/§6: `=` is a statement token. No expression may be an assignment, in
    /// any position, and each of these is a `Parse` error.
    ///
    /// The wording is corpus case `318`'s, byte for byte: `unexpected '='`, not
    /// `expected ')', found '='`, even though the ladder stops at `y` and it is
    /// the `)` that is missing.
    #[test]
    fn assignment_is_not_an_expression() {
        // the `(y = 1)` of `x = (y = 1)`
        assert_eq!(err("(y = 1)"), ("Parse", 1, "unexpected '='".to_string()));
        // the condition of `if (x = 1) { }`, which is the same expression
        assert_eq!(err("(x = 1)"), ("Parse", 1, "unexpected '='".to_string()));
        // case 318's own program, whose `=` is on line 3 — the statement half
        // parses `x =` and hands us `(y = 1)`
        let mut c = Cursor::new(tokenize("let x = 1;\nlet y = 2;\nx = (y = 1);").expect("lexes"));
        while !matches!(c.peek().kind, TokenKind::LParen) {
            c.advance();
        }
        let e = parse_expr(&mut c).expect_err("must not parse");
        assert_eq!(e.to_string(), "error: Parse at line 3: unexpected '='");
        // `=` in primary position
        assert_eq!(err("= 1"), ("Parse", 1, "unexpected '='".to_string()));
        // and a bare `x = 1` leaves the `=` unconsumed rather than folding it:
        // whatever the statement half wanted next, it is not an operator here
        let mut c = Cursor::new(tokenize("x = 1").expect("lexes"));
        assert_eq!(shape(&parse_expr(&mut c).expect("parses `x`")), "x");
        assert_eq!(c.peek().kind, TokenKind::Eq);
        assert!(!c.at_end());
    }

    /// Every malformed expression is a `Parse` error at the right line, and
    /// none of them panics or reads past `Eof`.
    #[test]
    fn error_cases_carry_the_right_line() {
        // truncated after an operator: the error is at `Eof`, which the lexer
        // puts on the line the last token ended on (line 2, not 3)
        assert_eq!(
            err("1 +\n2 *\n"),
            ("Parse", 2, "unexpected end of input".to_string())
        );
        // unclosed parenthesis, reported at end of input
        assert_eq!(
            err("(1 + 2"),
            ("Parse", 1, "expected ')', found end of input".to_string())
        );
        // a closing paren that never opened, in argument position
        assert_eq!(err("f(,)"), ("Parse", 1, "unexpected ','".to_string()));
        // no trailing comma in an argument list (§6 `.46`)
        assert_eq!(err("f(1,)"), ("Parse", 1, "unexpected ')'".to_string()));
        // a keyword where a primary belongs, on line 3
        assert_eq!(
            err("1 +\n2 +\nwhile"),
            ("Parse", 3, "unexpected 'while'".to_string())
        );
        // missing operand between two operators
        assert_eq!(err("1 * / 2"), ("Parse", 1, "unexpected '/'".to_string()));
        // empty input is `Eof` on line 1
        assert_eq!(err(""), ("Parse", 1, "unexpected end of input".to_string()));
        // a call whose argument list is never closed
        assert_eq!(
            err("f(1, 2"),
            ("Parse", 1, "expected ')', found end of input".to_string())
        );
    }

    /// Each node's line is its operator's or head token's, not the first line
    /// of the expression — §6 `.46` reports an error at the innermost node.
    #[test]
    fn nodes_carry_their_own_operator_line() {
        // the `+` is on line 2, its `lhs` variable on line 1
        let e = expr("a\n+ b");
        assert_eq!(line_of(&e), 2);
        match &e {
            Expr::Binary { lhs, rhs, .. } => {
                assert_eq!(line_of(lhs), 1);
                assert_eq!(line_of(rhs), 2);
            }
            other => panic!("parsed as {other:?}"),
        }
        // a unary takes the operator's line, a call its name's
        assert_eq!(line_of(&expr("\n\n-x")), 3);
        assert_eq!(line_of(&expr("\nf(\n1)")), 2);
        assert_eq!(line_of(&expr("\n\n\nx")), 4);
    }

    /// §6 `.41`: `-9223372036854775808` is NOT a literal. The lexer rejects the
    /// magnitude, so folding `Minus` + `Int` here would contradict the spec —
    /// this test is the decision, written down.
    #[test]
    fn i64_min_is_not_a_literal_and_is_not_folded() {
        // the negation of the smallest magnitude the lexer accepts stays two
        // nodes, so the engines raise the `Value` overflow §6 pins
        assert_eq!(
            shape(&expr("-9223372036854775807")),
            "(Neg 9223372036854775807)"
        );
        // and `i64::MIN` itself never reaches the parser at all
        let e = tokenize("-9223372036854775808").expect_err("lexer rejects it");
        assert_eq!(e.variant(), "Lex");
    }

    /// The cursor is the one shared piece of state, so its contract is tested
    /// directly: `advance` never walks off the end, and `peek` after `Eof` is
    /// still `Eof`.
    #[test]
    fn cursor_cannot_run_past_eof() {
        let mut c = Cursor::new(tokenize("x;").expect("lexes"));
        assert!(!c.at_end());
        assert_eq!(c.advance().kind, TokenKind::Ident("x".to_string()));
        assert!(c.eat(&TokenKind::Semi));
        assert!(c.at_end());
        for _ in 0..5 {
            assert_eq!(c.advance().kind, TokenKind::Eof);
            assert!(c.at_end());
        }
        // eat/expect at the end fail rather than moving
        assert!(!c.eat(&TokenKind::Semi));
        assert_eq!(
            c.expect(&TokenKind::Semi).expect_err("no `;` at Eof").msg(),
            "expected ';', found end of input"
        );
    }

    /// `ident` is the cursor helper the statement half uses for `let`, `fn` and
    /// assignment targets.
    #[test]
    fn ident_consumes_a_name_or_reports_one_was_wanted() {
        let mut c = Cursor::new(tokenize("x\nlet").expect("lexes"));
        assert_eq!(c.ident().expect("an identifier"), ("x".to_string(), 1));
        let e = c.ident().expect_err("`let` is not an identifier");
        assert_eq!(e.variant(), "Parse");
        assert_eq!(e.line(), 2);
        assert_eq!(e.msg(), "expected identifier, found 'let'");
    }
}
