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

use crate::error::{Result, TreadleError};
use crate::front::ast::{BinOp, Expr, FnDecl, Program, Stmt, UnOp};
use crate::front::lexer::tokenize;
use crate::front::token::{Token, TokenKind};
use crate::value::Value;
use std::rc::Rc;

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

/// §6c: how deep parentheses and call arguments may nest. Bounds the *parser's*
/// own recursion, which nothing else does — parens create no AST node. Parse of
/// nested parens survives 700 and aborts by 850 on a default 8 MiB stack, so
/// 100 leaves a 7x margin and is far past anything a human writes.
pub const MAX_NEST: u32 = 100;

/// §6c: how deep an expression's AST may be. Bounds what the engines and `Drop`
/// recurse over. Measured ceilings: engines survive 15 000 and abort at 20 000
/// on 8 MiB, survive 40 000 and abort at 50 000 on the 64 MiB thread
/// `Engine::run` uses, and `Drop` alone survives 50 000. 10 000 is below the
/// smallest of those, so the limit holds whatever stack the caller provides.
pub const MAX_EXPR_DEPTH: u32 = 10_000;

/// A position in the token vector.
///
/// `tokenize` guarantees a trailing [`TokenKind::Eof`], and [`Cursor::advance`]
/// refuses to move past it, so every accessor is infallible: this type cannot
/// index out of bounds and cannot panic on a truncated program.
pub(crate) struct Cursor {
    toks: Vec<Token>,
    pos: usize,
    /// Current parenthesis / call-argument nesting, against [`MAX_NEST`].
    nest: u32,
    /// Depth of the AST the most recent expression producer returned. Carried on
    /// the cursor rather than in every signature so `parse_expr` keeps the shape
    /// `parser-stmt` imports; each producer writes it before returning.
    last_depth: u32,
}

impl Cursor {
    /// Takes the vector `front::lexer::tokenize` returned, `Eof` included.
    pub(crate) fn new(toks: Vec<Token>) -> Cursor {
        debug_assert!(
            matches!(toks.last().map(|t| &t.kind), Some(TokenKind::Eof)),
            "tokenize always ends the stream with Eof"
        );
        Cursor {
            toks,
            pos: 0,
            nest: 0,
            last_depth: 0,
        }
    }

    /// §6c. Enter a nesting level, refusing past [`MAX_NEST`]. Paired with
    /// [`Cursor::leave`] — the guard is on the way *in*, so the parser never
    /// makes the recursive call that would overflow.
    fn enter(&mut self, line: u32) -> Result<()> {
        self.nest += 1;
        if self.nest > MAX_NEST {
            return Err(TreadleError::nesting_too_deep(line, MAX_NEST));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.nest -= 1;
    }

    /// §6c. Record the depth of the expression just built, refusing past
    /// [`MAX_EXPR_DEPTH`]. Called at every node construction, so a chain is
    /// caught as it grows rather than after a 100 000-node tree exists.
    fn depth(&mut self, d: u32, line: u32) -> Result<u32> {
        if d > MAX_EXPR_DEPTH {
            return Err(TreadleError::expression_too_deep(line, MAX_EXPR_DEPTH));
        }
        self.last_depth = d;
        Ok(d)
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
    let mut depth = c.last_depth;
    while let Some(op) = binop_at(level, &c.peek().kind) {
        // The node's line is the operator's, per §6 `.46`: an error is reported
        // at the innermost node that failed, and for `a\n+ b` that is the `+`.
        let line = c.advance().line;
        let rhs = binary(c, level + 1)?;
        // §6c: each fold deepens the tree by one on the left. Checked here, as
        // the chain grows, so `1 + 1 + …` is refused at 10 000 instead of
        // building a 100 000-node tree that aborts the process when dropped.
        depth = c.depth(depth.max(c.last_depth) + 1, line)?;
        lhs = Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            line,
        };
    }
    c.last_depth = depth;
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
    // §6c: `-` and `!` are right-associative, so a prefix run recurses here as
    // well as deepening the tree. Guarding nesting bounds the recursion; the
    // depth check below bounds what the engines then walk.
    c.enter(line)?;
    let rhs = unary(c)?;
    c.leave();
    let d = c.last_depth + 1;
    c.depth(d, line)?;
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
            // §6c: a leaf is depth 1. Every other producer builds on this.
            c.depth(1, line)?;
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
                // §6c: an argument list re-enters `parse_expr`, so it nests.
                c.enter(line)?;
                let args = call_args(c)?;
                c.leave();
                // Depth of a call is one past its deepest argument, which
                // `call_args` left in `last_depth`; a call with none is a leaf.
                c.depth(c.last_depth + 1, line)?;
                Ok(Expr::Call { name, args, line })
            } else {
                c.depth(1, line)?;
                Ok(Expr::Var { name, line })
            }
        }
        TokenKind::LParen => {
            c.advance();
            // §6c: parens build **no node**, so they do not deepen the tree —
            // but they do deepen the parser's own recursion, and nothing else
            // bounds that. This is the guard that stops `((((…900…))))1`.
            c.enter(line)?;
            let inner = parse_expr(c)?;
            c.leave();
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
/// Leaves `c.last_depth` holding the **deepest** argument's depth, not the last
/// one's, so the caller can size the `Call` node correctly (§6c).
fn call_args(c: &mut Cursor) -> Result<Vec<Expr>> {
    let mut args = Vec::new();
    if c.eat(&TokenKind::RParen) {
        c.last_depth = 0;
        return Ok(args);
    }
    let mut deepest = 0;
    loop {
        args.push(parse_expr(c)?);
        deepest = deepest.max(c.last_depth);
        if !c.eat(&TokenKind::Comma) {
            break;
        }
    }
    c.expect(&TokenKind::RParen)?;
    c.last_depth = deepest;
    Ok(args)
}

// ---- statements (bead `.12`) ---------------------------------------------

/// Source to `Program`, the whole front end. Lexes and parses; every failure is
/// a `Lex` or `Parse` error, so both engines start from a `Program` that is
/// structurally valid and neither raises an error the other could miss.
pub fn parse(src: &str) -> Result<Program> {
    let mut p = Stmts {
        c: Cursor::new(tokenize(src)?),
        fns: Vec::new(),
        declared: Vec::new(),
    };
    let mut stmts = Vec::new();
    // `advance` clamps at `Eof`, so this loop terminates only because every arm
    // of `statement` consumes at least one token.
    while !p.c.at_end() {
        stmts.push(p.statement(false)?);
    }
    Ok(Program { stmts, fns: p.fns })
}

/// The statement half's state: the shared [`Cursor`] plus the hoisted function
/// list being accumulated.
struct Stmts {
    c: Cursor,
    /// Every `FnDecl` in the program, at any nesting depth, in **source order**
    /// — `ast.rs`'s hoisting contract. `Stmt::Fn` stays in `stmts` where it was
    /// written and is a run-time no-op; both engines define from here only.
    fns: Vec<Rc<FnDecl>>,
    /// Function names seen so far, recorded at the declaration's *header* rather
    /// than when it is pushed to `fns`, so `fn f() { fn f() {} }` is caught too
    /// (the enclosing declaration does not reach `fns` until its body is parsed).
    declared: Vec<String>,
}

impl Stmts {
    /// One statement. `in_fn` is whether we are inside a function body, which
    /// §2 makes part of the grammar: `return` outside one is a `Parse` error.
    fn statement(&mut self, in_fn: bool) -> Result<Stmt> {
        match self.c.peek().kind {
            TokenKind::Let => self.let_stmt(),
            TokenKind::Print => self.print_stmt(),
            TokenKind::If => self.if_stmt(in_fn),
            TokenKind::While => self.while_stmt(in_fn),
            TokenKind::Fn => self.fn_stmt(),
            TokenKind::Return => self.return_stmt(in_fn),
            TokenKind::Ident(_) => self.assign_stmt(),
            // Everything else, `=` included (reported as `unexpected '='` by
            // `Cursor::expected`). No expression statement is reachable from
            // here, which is §6 `.44`.
            _ => Err(self.c.expected("statement")),
        }
    }

    /// A braced body, the only kind §6 `.46` allows for `if`/`else`/`while`/`fn`.
    ///
    /// An **empty** body is legal — `fn f() { }`, `if c { }`, `while c { }`.
    /// Bead `.64` asked and §6 never answered; corpus case `208` requires it, an
    /// empty `els` is already the only spelling of an else-less `if`, and the
    /// natural loop below accepts it, so legal is both the cheap and the
    /// consistent reading.
    fn block(&mut self, in_fn: bool) -> Result<Vec<Stmt>> {
        self.c.expect(&TokenKind::LBrace)?;
        let mut stmts = Vec::new();
        while !self.c.eat(&TokenKind::RBrace) {
            // Checked here rather than left to `statement`, so a truncated
            // program says `expected '}', found end of input` (corpus `319`)
            // instead of naming whatever statement could have come next.
            if self.c.at_end() {
                return Err(self.c.expected(&describe(&TokenKind::RBrace)));
            }
            stmts.push(self.statement(in_fn)?);
        }
        Ok(stmts)
    }

    fn let_stmt(&mut self) -> Result<Stmt> {
        let line = self.c.advance().line;
        let (name, _) = self.c.ident()?;
        // §6 `.46`: a `let` always has an initialiser, so `let x;` is a `Parse`
        // error rather than a binding to `nil`.
        self.c.expect(&TokenKind::Eq)?;
        let init = parse_expr(&mut self.c)?;
        self.c.expect(&TokenKind::Semi)?;
        Ok(Stmt::Let { name, init, line })
    }

    /// Assignment, and the only statement that starts with a name.
    ///
    /// **This is where `f();` fails** (§6 `.44`): there is no expression
    /// statement and the frozen `Stmt` has no variant for one, so after an
    /// identifier an `=` is required and a call reports
    /// `expected '=', found '('`.
    fn assign_stmt(&mut self) -> Result<Stmt> {
        let (name, line) = self.c.ident()?;
        self.c.expect(&TokenKind::Eq)?;
        let value = parse_expr(&mut self.c)?;
        self.c.expect(&TokenKind::Semi)?;
        Ok(Stmt::Assign { name, value, line })
    }

    /// `print e (, e)* ;` — **one or more** arguments, no trailing comma
    /// (§6 `.46`). Requiring the first expression before the loop is what gives
    /// the `>= 1`.
    fn print_stmt(&mut self) -> Result<Stmt> {
        let line = self.c.advance().line;
        let mut args = vec![parse_expr(&mut self.c)?];
        while self.c.eat(&TokenKind::Comma) {
            args.push(parse_expr(&mut self.c)?);
        }
        self.c.expect(&TokenKind::Semi)?;
        Ok(Stmt::Print { args, line })
    }

    /// `if cond { .. }`, optionally `else { .. }` or `else if ..`.
    ///
    /// An `else if` chain is `els: vec![Stmt::If { .. }]` (§6 `.46`) — one nested
    /// statement, not a flattened list of arms — and because every body is
    /// braced there is no dangling-else question to answer.
    fn if_stmt(&mut self, in_fn: bool) -> Result<Stmt> {
        let line = self.c.advance().line;
        let cond = parse_expr(&mut self.c)?;
        let then = self.block(in_fn)?;
        let els = if self.c.eat(&TokenKind::Else) {
            if matches!(self.c.peek().kind, TokenKind::If) {
                vec![self.if_stmt(in_fn)?]
            } else {
                self.block(in_fn)?
            }
        } else {
            Vec::new()
        };
        Ok(Stmt::If {
            cond,
            then,
            els,
            line,
        })
    }

    fn while_stmt(&mut self, in_fn: bool) -> Result<Stmt> {
        let line = self.c.advance().line;
        let cond = parse_expr(&mut self.c)?;
        let body = self.block(in_fn)?;
        Ok(Stmt::While { cond, body, line })
    }

    /// `return;` or `return e;`, and **only inside a function**: §2 makes
    /// `return` at top level a `Parse` error, so corpus `317` sees no output at
    /// all from a program whose first line is a `print`.
    fn return_stmt(&mut self, in_fn: bool) -> Result<Stmt> {
        let line = self.c.advance().line;
        if !in_fn {
            return Err(TreadleError::return_outside_fn(line));
        }
        let value = if self.c.eat(&TokenKind::Semi) {
            None
        } else {
            let value = parse_expr(&mut self.c)?;
            self.c.expect(&TokenKind::Semi)?;
            Some(value)
        };
        Ok(Stmt::Return { value, line })
    }

    /// A function declaration, which is both a `Stmt::Fn` where it was written
    /// and an entry in `Program::fns` (§2 hoists to global; `ast.rs` pins that
    /// the statement stays in place and is a no-op at run time).
    fn fn_stmt(&mut self) -> Result<Stmt> {
        let line = self.c.advance().line;
        let (name, name_line) = self.c.ident()?;
        // §6 `.42`: the three builtins are reserved as function names, and a
        // duplicate `fn` name is a `Parse` error — both engines define from
        // `fns` and neither may pick a winner between two same-named entries.
        if matches!(name.as_str(), "len" | "str" | "int") {
            return Err(TreadleError::reserved_fn_name(name_line, &name));
        }
        if self.declared.iter().any(|n| n == &name) {
            return Err(TreadleError::duplicate_fn(name_line, &name));
        }
        self.declared.push(name.clone());
        self.c.expect(&TokenKind::LParen)?;
        let mut params = Vec::new();
        if !self.c.eat(&TokenKind::RParen) {
            loop {
                let (param, param_line) = self.c.ident()?;
                // §6d (bead `.69`): the same parameter twice is refused here, in
                // the shared front end, so neither engine needs an opinion. They
                // bind parameters by different mechanisms — scope map by name
                // versus slot by index — and both happen to let the last
                // argument win, which is agreement by coincidence rather than by
                // contract, i.e. a latent divergence.
                if params.iter().any(|p| p == &param) {
                    return Err(TreadleError::duplicate_param(param_line, &param));
                }
                params.push(param);
                if !self.c.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.c.expect(&TokenKind::RParen)?;
        }
        // The slot is taken before the body is parsed, so a nested declaration
        // (which pushes from inside `block`) lands *after* its enclosing one and
        // `fns` is in source order rather than innermost-first.
        let slot = self.fns.len();
        let body = self.block(true)?;
        let decl = Rc::new(FnDecl {
            name,
            params,
            body,
            line,
        });
        self.fns.insert(slot, Rc::clone(&decl));
        Ok(Stmt::Fn(decl))
    }
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

    // ---- statements (bead `.12`) -----------------------------------------

    fn prog(src: &str) -> Program {
        parse(src).expect("parses")
    }

    /// A whole program that must fail, yielding (variant, line, message).
    fn perr(src: &str) -> (&'static str, u32, String) {
        let e = parse(src).expect_err("must not parse");
        (e.variant(), e.line(), e.msg().to_string())
    }

    /// The statement counterpart of [`shape`]: bracketed, so nesting is
    /// unambiguous in one string (`Stmt` has no `PartialEq` by ast's decision).
    fn sshape(s: &Stmt) -> String {
        match s {
            Stmt::Let { name, init, .. } => format!("(let {name} {})", shape(init)),
            Stmt::Assign { name, value, .. } => format!("(set {name} {})", shape(value)),
            Stmt::Print { args, .. } => {
                let args: Vec<String> = args.iter().map(shape).collect();
                format!("(print [{}])", args.join(" "))
            }
            Stmt::If {
                cond, then, els, ..
            } => format!("(if {} [{}] [{}])", shape(cond), body(then), body(els)),
            Stmt::While { cond, body: b, .. } => format!("(while {} [{}])", shape(cond), body(b)),
            Stmt::Return { value: Some(e), .. } => format!("(return {})", shape(e)),
            Stmt::Return { value: None, .. } => "(return)".to_string(),
            Stmt::Fn(d) => format!(
                "(fn {} ({}) [{}])",
                d.name,
                d.params.join(" "),
                body(&d.body)
            ),
        }
    }

    fn body(stmts: &[Stmt]) -> String {
        let rendered: Vec<String> = stmts.iter().map(sshape).collect();
        rendered.join(" ")
    }

    #[test]
    fn let_assign_and_print_statements() {
        assert_eq!(body(&prog("let x = 1 + 2;").stmts), "(let x (Add 1 2))");
        assert_eq!(body(&prog("x = x + 1;").stmts), "(set x (Add x 1))");
        // `print` takes one or more arguments, in source order (§6 `.46`, `.33`)
        assert_eq!(body(&prog("print 1;").stmts), "(print [1])");
        assert_eq!(
            body(&prog(r#"print "a", x, true;"#).stmts),
            "(print [a x true])"
        );
        // several statements, in order, and each carries its own line
        let p = prog("let x = 0;\nx = 1;\nprint x;");
        assert_eq!(p.stmts.len(), 3);
        let lines: Vec<u32> = p
            .stmts
            .iter()
            .map(|s| match s {
                Stmt::Let { line, .. } | Stmt::Assign { line, .. } | Stmt::Print { line, .. } => {
                    *line
                }
                other => panic!("parsed as {other:?}"),
            })
            .collect();
        assert_eq!(lines, [1, 2, 3]);
    }

    /// `while` and `if` bodies nest, and treadle has no standalone block
    /// statement — the frozen `Stmt` has no `Block` variant — so "nested blocks"
    /// are exactly these braced bodies.
    #[test]
    fn nested_blocks_nest_to_any_depth() {
        let src = "while a < 1 { while b < 2 { if c { print 1; } } }";
        assert_eq!(
            body(&prog(src).stmts),
            "(while (Lt a 1) [(while (Lt b 2) [(if c [(print [1])] [])])])"
        );
        // §6 `.37`: a body is a scope, and the parser keeps the nesting that
        // makes shadowing expressible
        assert_eq!(
            body(&prog("let x = 1; while true { let x = 2; print x; }").stmts),
            "(let x 1) (while true [(let x 2) (print [x])])"
        );
    }

    /// An `if` with no `else` is `els: vec![]` — the only spelling of it, which
    /// is also why an empty braced body has to be legal.
    #[test]
    fn if_without_else_has_an_empty_els() {
        match &prog("if x { print 1; }").stmts[0] {
            Stmt::If { then, els, .. } => {
                assert_eq!(then.len(), 1);
                assert!(els.is_empty(), "no else means an empty els");
            }
            other => panic!("parsed as {other:?}"),
        }
        assert_eq!(
            body(&prog("if x { print 1; } else { print 2; }").stmts),
            "(if x [(print [1])] [(print [2])])"
        );
    }

    /// §6 `.46`: `else if` chains as `els: vec![Stmt::If { .. }]`, so corpus case
    /// `106`'s three-arm chain is three nested `If`s and exactly one arm runs.
    #[test]
    fn else_if_chains_as_a_nested_if() {
        let src = "if n == 1 { print 1; } else if n == 2 { print 2; } else { print 3; }";
        assert_eq!(
            body(&prog(src).stmts),
            "(if (Eq n 1) [(print [1])] [(if (Eq n 2) [(print [2])] [(print [3])])])"
        );
        // the nested arm really is a single Stmt::If, not a flattened list
        match &prog(src).stmts[0] {
            Stmt::If { els, .. } => {
                assert_eq!(els.len(), 1);
                assert!(matches!(els[0], Stmt::If { .. }));
            }
            other => panic!("parsed as {other:?}"),
        }
        // an `else if` with no final `else` bottoms out at an empty els
        assert_eq!(
            body(&prog("if a { print 1; } else if b { print 2; }").stmts),
            "(if a [(print [1])] [(if b [(print [2])] [])])"
        );
    }

    /// Bead `.64`, answered here because §6 does not: an empty braced body is
    /// **legal** in all three places one can appear. Corpus case `208`
    /// (`fn empty() { }` returning `nil`) depends on it.
    #[test]
    fn empty_braced_bodies_are_legal() {
        assert_eq!(body(&prog("fn f() { }").stmts), "(fn f () [])");
        assert_eq!(body(&prog("if c { }").stmts), "(if c [] [])");
        assert_eq!(body(&prog("while c { }").stmts), "(while c [])");
        assert_eq!(
            body(&prog("if c { } else { }").stmts),
            "(if c [] [])" // an empty else is indistinguishable from none
        );
        // a whole program may also be empty
        let p = prog("");
        assert!(p.stmts.is_empty() && p.fns.is_empty());
    }

    /// §6d (bead `.69`). Refused in the shared front end, so the question never
    /// reaches either engine. Before this, both bound the LAST argument and
    /// agreed byte-for-byte — by coincidence of a scope map and a slot table,
    /// not by contract.
    #[test]
    fn a_duplicate_parameter_name_is_a_parse_error() {
        let e = parse("fn f(a, a) { print a; }").expect_err("must be refused");
        assert!(
            matches!(&e, TreadleError::Parse { line: 1, msg } if msg == "duplicate parameter 'a'"),
            "got {e:?}"
        );
        // Three params, the repeat not adjacent, reported at the repeat's line.
        let e = parse("fn g(a,\n b,\n a) { return a; }").expect_err("must be refused");
        assert_eq!(e.line(), 3, "reported where the repeat is");
        // Distinct names are of course fine, and a param may shadow a global.
        assert!(parse("fn h(a, b, c) { return a; }").is_ok());
        assert!(parse("let a = 1;\nfn k(a) { return a; }").is_ok());
    }

    #[test]
    fn fn_declarations_take_zero_one_or_many_params() {
        assert_eq!(
            body(&prog("fn add(a, b) { return a + b; }").stmts),
            "(fn add (a b) [(return (Add a b))])"
        );
        assert_eq!(
            body(&prog("fn f() { return; }").stmts),
            "(fn f () [(return)])"
        );
        assert_eq!(
            body(&prog("fn g(x) { print x; }").stmts),
            "(fn g (x) [(print [x])])"
        );
        // the `fn` keyword's line is the declaration's line
        match &prog("\n\nfn f() { }").stmts[0] {
            Stmt::Fn(d) => assert_eq!(d.line, 3),
            other => panic!("parsed as {other:?}"),
        }
    }

    /// `ast.rs`'s hoisting contract: `fns` is the complete list at any nesting
    /// depth in source order, the `Rc` in `stmts` is the *same* allocation, and
    /// `Stmt::Fn` stays where it was written.
    #[test]
    fn fns_are_hoisted_at_every_depth_in_source_order() {
        let p = prog(
            "fn outer(n) {\n\
             fn inner() { return 1; }\n\
             return inner();\n\
             }\n\
             if false { fn dead() { return 2; } }\n\
             while false { fn looped() { return 3; } }\n\
             fn last() { return 4; }\n",
        );
        let names: Vec<&str> = p.fns.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["outer", "inner", "dead", "looped", "last"]);
        // a fn inside a branch that never runs is still callable (§2 hoisting),
        // and its Stmt::Fn is still inside that branch
        assert_eq!(
            body(&p.stmts),
            "(fn outer (n) [(fn inner () [(return 1)]) (return (call inner []))]) \
             (if false [(fn dead () [(return 2)])] []) \
             (while false [(fn looped () [(return 3)])]) \
             (fn last () [(return 4)])"
        );
        // the two references are one allocation, so an engine defining from
        // `fns` and one walking `stmts` cannot see different bodies
        match &p.stmts[0] {
            Stmt::Fn(d) => assert!(Rc::ptr_eq(d, &p.fns[0])),
            other => panic!("parsed as {other:?}"),
        }
    }

    /// §2: `return` leaves the function from any depth, and the parser accepts it
    /// anywhere inside a body — corpus `210` returns from two nested loops.
    #[test]
    fn return_is_accepted_at_any_depth_inside_a_fn() {
        let src = "fn find(n) { while a { while b { if c { return 1; } } } return -1; }";
        assert_eq!(
            body(&prog(src).stmts),
            "(fn find (n) [(while a [(while b [(if c [(return 1)] [])])]) (return (Neg 1))])"
        );
        // a return inside a nested fn belongs to that fn, not the outer one
        assert!(parse("fn a() { fn b() { return 1; } return 2; }").is_ok());
    }

    /// §2/§6, corpus `317`: `return` outside a function is a **Parse** error, so
    /// nothing runs and there is no output. The parser therefore has to track
    /// whether it is inside a body.
    #[test]
    fn return_outside_a_fn_is_a_parse_error() {
        assert_eq!(
            perr("print 1;\nreturn 2;"),
            ("Parse", 2, "return outside of a function".to_string())
        );
        // corpus 317's exact rendering
        assert_eq!(
            parse("print 1;\nreturn 2;")
                .expect_err("must not parse")
                .to_string(),
            "error: Parse at line 2: return outside of a function"
        );
        // a bare `return;` at top level, and one nested in top-level blocks:
        // being inside braces is not being inside a function
        assert_eq!(
            perr("return;"),
            ("Parse", 1, "return outside of a function".to_string())
        );
        assert_eq!(
            perr("if true {\n  return 1;\n}"),
            ("Parse", 2, "return outside of a function".to_string())
        );
        assert_eq!(
            perr("while true {\n  if x {\n    return;\n  }\n}"),
            ("Parse", 3, "return outside of a function".to_string())
        );
    }

    /// §6 `.44`: there are no expression statements. `f();` is a `Parse` error —
    /// call for effect with `print f();` or `let _ = f();`, both of which parse.
    #[test]
    fn a_call_is_not_a_statement() {
        assert_eq!(
            perr("f();"),
            ("Parse", 1, "expected '=', found '('".to_string())
        );
        assert_eq!(
            perr("1;"),
            ("Parse", 1, "expected statement, found '1'".to_string())
        );
        assert_eq!(
            perr("x + 1;"),
            ("Parse", 1, "expected '=', found '+'".to_string())
        );
        // the two spellings §6 offers instead
        assert_eq!(body(&prog("print f();").stmts), "(print [(call f [])])");
        assert_eq!(body(&prog("let _ = f();").stmts), "(let _ (call f []))");
    }

    /// §2: `=` is a statement token. Corpus `318` pins the wording, including
    /// that it is `unexpected '='` and not `expected ')', found '='`.
    #[test]
    fn assignment_is_never_an_expression_in_a_statement() {
        assert_eq!(
            perr("let x = 1;\nlet y = 2;\nx = (y = 1);"),
            ("Parse", 3, "unexpected '='".to_string())
        );
        assert_eq!(
            parse("let x = 1;\nlet y = 2;\nx = (y = 1);")
                .expect_err("must not parse")
                .to_string(),
            "error: Parse at line 3: unexpected '='"
        );
        // in a condition, and chained
        assert_eq!(
            perr("if (x = 1) { }"),
            ("Parse", 1, "unexpected '='".to_string())
        );
        assert_eq!(
            perr("while (x = 1) { }"),
            ("Parse", 1, "unexpected '='".to_string())
        );
        assert_eq!(
            perr("x = y = 1;"),
            ("Parse", 1, "unexpected '='".to_string())
        );
        assert_eq!(
            perr("let x = (y = 1);"),
            ("Parse", 1, "unexpected '='".to_string())
        );
        assert_eq!(
            perr("print (x = 1);"),
            ("Parse", 1, "unexpected '='".to_string())
        );
        // and a statement that is only an `=`
        assert_eq!(perr("= 1;"), ("Parse", 1, "unexpected '='".to_string()));
    }

    /// Every malformed statement is a `Parse` error at the line of the token that
    /// was **found** (§6 `.46`), never the line of the unmatched opener.
    #[test]
    fn statement_errors_carry_the_right_line() {
        // corpus 319: the missing `}` is reported at end of input, whose line is
        // the line the LAST TOKEN ended on — 4, not the trailing newline's 5
        assert_eq!(
            perr("let x = 0;\nwhile x < 3 {\n  x = x + 1;\nprint x;\n"),
            ("Parse", 4, "expected '}', found end of input".to_string())
        );
        // corpus 320: bodies are always braced (§6 `.46`)
        assert_eq!(
            perr("print 1;\nif true print 2;"),
            ("Parse", 2, "expected '{', found 'print'".to_string())
        );
        assert_eq!(
            perr("while x < 1 print 2;"),
            ("Parse", 1, "expected '{', found 'print'".to_string())
        );
        // §6 `.46`: a `let` always has an initialiser
        assert_eq!(
            perr("let x;"),
            ("Parse", 1, "expected '=', found ';'".to_string())
        );
        assert_eq!(
            perr("let 1 = 2;"),
            ("Parse", 1, "expected identifier, found '1'".to_string())
        );
        // §6 `.46`: `print` takes at least one argument, and no trailing comma
        assert_eq!(perr("print;"), ("Parse", 1, "unexpected ';'".to_string()));
        assert_eq!(
            perr("print 1,;"),
            ("Parse", 1, "unexpected ';'".to_string())
        );
        // missing semicolons, each at the token that was found
        assert_eq!(
            perr("let x = 1\nprint x;"),
            ("Parse", 2, "expected ';', found 'print'".to_string())
        );
        assert_eq!(
            perr("fn f() {\n  return 1\n}"),
            ("Parse", 3, "expected ';', found '}'".to_string())
        );
        // a stray closing brace, and an `else` with no `if`
        assert_eq!(
            perr("}"),
            ("Parse", 1, "expected statement, found '}'".to_string())
        );
        assert_eq!(
            perr("else { }"),
            ("Parse", 1, "expected statement, found 'else'".to_string())
        );
        // malformed parameter lists
        assert_eq!(
            perr("fn f(a,) { }"),
            ("Parse", 1, "expected identifier, found ')'".to_string())
        );
        assert_eq!(
            perr("fn f a) { }"),
            ("Parse", 1, "expected '(', found 'a'".to_string())
        );
        assert_eq!(
            perr("fn 1() { }"),
            ("Parse", 1, "expected identifier, found '1'".to_string())
        );
    }

    /// §6 `.42`: functions are one global namespace — a duplicate `fn` name is a
    /// `Parse` error at the second declaration, and `len`/`str`/`int` cannot be
    /// declared at all. `env.rs` relies on both: its `define_fn` overwrite is
    /// documented as unreachable.
    #[test]
    fn duplicate_and_reserved_fn_names_are_parse_errors() {
        assert_eq!(
            perr("fn f() { }\nfn f() { }"),
            ("Parse", 2, "duplicate function 'f'".to_string())
        );
        // at any nesting depth, in either direction, since hoisting is global
        assert_eq!(
            perr("fn f() {\n  fn f() { }\n}"),
            ("Parse", 2, "duplicate function 'f'".to_string())
        );
        assert_eq!(
            perr("fn a() { }\nif x {\n  fn a() { }\n}"),
            ("Parse", 3, "duplicate function 'a'".to_string())
        );
        for name in ["len", "str", "int"] {
            assert_eq!(
                perr(&format!("fn {name}() {{ }}")),
                (
                    "Parse",
                    1,
                    format!("cannot declare builtin function '{name}'")
                )
            );
        }
        // but they are ordinary variable names (§6 `.42`: two namespaces), and a
        // `let` may share a name with a function
        assert!(parse("let len = 1; print len;").is_ok());
        assert!(parse("fn f() { }\nlet f = 1;").is_ok());
    }

    /// A `Lex` error reaches the caller of `parse` unchanged, so a bad byte or an
    /// out-of-range literal is reported as `Lex` and not repackaged as `Parse`.
    #[test]
    fn lex_errors_pass_through_parse() {
        assert_eq!(
            perr("print 99999999999999999999;"),
            (
                "Lex",
                1,
                "integer literal out of range: 99999999999999999999".to_string()
            )
        );
        assert_eq!(
            perr("print 1;\n@"),
            ("Lex", 2, "unexpected character '@'".to_string())
        );
    }

    /// The §2 sample program, whole, as one end-to-end check that the two halves
    /// meet: every statement form and both hoisting paths in one parse.
    #[test]
    fn the_spec_sample_program_parses() {
        let p = prog(
            "let x = 1;\n\
             x = x + 1;\n\
             print x;\n\
             print \"a\", x, true;\n\
             if x > 2 { print 1; } else { print 2; }\n\
             while x < 10 { x = x + 1; }\n\
             fn add(a, b) { return a + b; }\n\
             print add(1, 2);\n\
             fn fact(n) { if n < 2 { return 1; } return n * fact(n - 1); }\n",
        );
        assert_eq!(p.stmts.len(), 9);
        let names: Vec<&str> = p.fns.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["add", "fact"]);
        assert_eq!(
            sshape(&p.stmts[8]),
            "(fn fact (n) [(if (Lt n 2) [(return 1)] []) (return (Mul n (call fact [(Sub n 1)])))])"
        );
    }

    // ---- §6c: nothing may abort the process (bead `.67`) ----
    //
    // Each of these used to be `fatal runtime error: stack overflow, aborting`,
    // which §4 forbids for any input. They are three DIFFERENT recursions and
    // two different limits: parens deepen the parser without deepening the tree,
    // a chain deepens the tree without deepening the parser.

    /// Parens build no AST node, so only the nesting limit catches this. Used to
    /// abort by 850 on a default stack.
    #[test]
    fn deeply_nested_parens_are_a_parse_error_not_an_abort() {
        for n in [MAX_NEST + 1, 900, 5_000] {
            let src = format!(
                "print {}1{};",
                "(".repeat(n as usize),
                ")".repeat(n as usize)
            );
            let e = parse(&src).expect_err("must be refused, not abort");
            assert!(
                matches!(&e, TreadleError::Parse { line: 1, msg } if msg.contains("nested deeper")),
                "n={n} gave {e:?}"
            );
        }
    }

    /// A chain is nesting 1 and tree depth N, so only the depth limit catches it.
    /// `parse` used to SUCCEED here and abort while dropping the tree.
    #[test]
    fn a_long_chain_is_a_parse_error_not_an_abort_in_drop() {
        let src = format!("print 1{};", " + 1".repeat(100_000));
        let e = parse(&src).expect_err("must be refused, not abort");
        assert!(
            matches!(&e, TreadleError::Parse { line: 1, msg } if msg.contains("deeper than")),
            "got {e:?}"
        );
    }

    /// A prefix run is right-associative, so it recurses AND deepens.
    #[test]
    fn a_long_prefix_run_is_refused() {
        let src = format!("print {}1;", "-".repeat(5_000));
        assert!(parse(&src).is_err(), "must be refused, not abort");
    }

    /// The limits must not touch anything a person would write. Both are orders
    /// of magnitude above real code, which is the point of choosing them from
    /// measured ceilings rather than taste.
    #[test]
    fn the_limits_do_not_reject_ordinary_programs() {
        assert!(parse("print ((((1 + 2)) * 3));").is_ok());
        assert!(parse("print -(-(-(1)));").is_ok());
        assert!(parse("fn f(a, b) { return a + b; }\nprint f(f(1, 2), f(3, 4));").is_ok());
        // Nesting 20 and a 500-term chain both pass comfortably.
        let deep = format!("print {}1{};", "(".repeat(20), ")".repeat(20));
        assert!(parse(&deep).is_ok());
        let chain = format!("print 1{};", " + 1".repeat(500));
        assert!(parse(&chain).is_ok());
    }

    /// Depth is per-expression, not cumulative across a program: a thousand
    /// separate statements must not add up to the cap.
    #[test]
    fn depth_does_not_accumulate_across_statements() {
        let src = "print 1 + 1 + 1 + 1;\n".repeat(1_000);
        assert!(parse(&src).is_ok(), "1000 shallow statements must parse");
    }
}
