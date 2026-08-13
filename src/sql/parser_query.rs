//! bead: quern-parser-query — SELECT, JOIN, GROUP BY, ORDER BY, LIMIT
//!
//! Two things live here, and the second is why `parser_ddl.rs` and
//! `parser_dml.rs` import this module:
//!
//! * [`Cursor`] — the shared token cursor. Peek/eat/expect over a `&[Token]`,
//!   with `Err(QuernError::Parse)` messages that name what was expected and
//!   what was actually there. It never indexes past the end, so a truncated
//!   statement is an ordinary error rather than a panic (§1: errors are values).
//! * [`parse_expr`] — the ONE expression parser in quern. §1's precedence,
//!   loosest first: `OR` < `AND` < `NOT` < `= <> < >` < `+ -` < `* /` <
//!   unary `-` < primary. `NOT` and unary `-` are prefix; `(expr)` overrides.
//!
//! Parsers produce `Expr::Column { table, name }` and never `ColumnRef` —
//! resolving a name to an index is `plan::logical`'s job. Aggregates parse to
//! `Expr::Agg` in the SELECT list only; they are rejected in `WHERE`, `ON`,
//! `GROUP BY` and `ORDER BY`, because quern has no `HAVING` and `exec::eval`
//! must never meet a bare `Agg`.

use crate::sql::ast::{
    AggExpr, AggFunc, BinOp, Expr, Join, SelectItem, SelectStmt, Statement, UnOp,
};
use crate::sql::token::Token;
use crate::types::{QuernError, Result, Value};

// --- the shared cursor ------------------------------------------------------

/// A position in a token slice. Cloning is cheap and deliberate: a parser can
/// save a cursor, try something, and go back to the copy.
#[derive(Debug, Clone)]
pub(crate) struct Cursor<'t> {
    toks: &'t [Token],
    pos: usize,
}

impl<'t> Cursor<'t> {
    pub(crate) fn new(toks: &'t [Token]) -> Cursor<'t> {
        Cursor { toks, pos: 0 }
    }

    /// The next token without consuming it; `None` at end of input.
    pub(crate) fn peek(&self) -> Option<&'t Token> {
        self.toks.get(self.pos)
    }

    /// Consumes and returns the next token; `None` at end of input.
    pub(crate) fn advance(&mut self) -> Option<&'t Token> {
        let t = self.toks.get(self.pos)?;
        self.pos += 1;
        Some(t)
    }

    pub(crate) fn at_end(&self) -> bool {
        self.pos >= self.toks.len()
    }

    /// Consumes `t` if it is next. Not an error either way — this is the
    /// "is the optional clause there" test.
    pub(crate) fn eat(&mut self, t: &Token) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consumes `t` or fails naming it.
    pub(crate) fn expect(&mut self, t: &Token) -> Result<()> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(self.expected(&format!("{t:?}")))
        }
    }

    /// Consumes an identifier. `what` describes it for the error message
    /// ("a table name after FROM").
    pub(crate) fn ident(&mut self, what: &str) -> Result<String> {
        match self.peek() {
            Some(Token::Ident(s)) => {
                self.pos += 1;
                Ok(s.clone())
            }
            _ => Err(self.expected(what)),
        }
    }

    /// The one error shape: what was expected, and what is there instead.
    pub(crate) fn expected(&self, what: &str) -> QuernError {
        match self.peek() {
            Some(t) => QuernError::Parse(format!("expected {what}, found {t:?}")),
            None => QuernError::Parse(format!("expected {what}, found end of input")),
        }
    }
}

// --- expressions ------------------------------------------------------------

/// Parse one expression at the cursor. See the module docs for precedence.
pub(crate) fn parse_expr(c: &mut Cursor) -> Result<Expr> {
    or_expr(c)
}

fn or_expr(c: &mut Cursor) -> Result<Expr> {
    let mut left = and_expr(c)?;
    while c.eat(&Token::Or) {
        left = Expr::bin(left, BinOp::Or, and_expr(c)?);
    }
    Ok(left)
}

fn and_expr(c: &mut Cursor) -> Result<Expr> {
    let mut left = not_expr(c)?;
    while c.eat(&Token::And) {
        left = Expr::bin(left, BinOp::And, not_expr(c)?);
    }
    Ok(left)
}

fn not_expr(c: &mut Cursor) -> Result<Expr> {
    if c.eat(&Token::Not) {
        Ok(Expr::un(UnOp::Not, not_expr(c)?))
    } else {
        cmp_expr(c)
    }
}

fn cmp_expr(c: &mut Cursor) -> Result<Expr> {
    let mut left = additive(c)?;
    loop {
        let op = match c.peek() {
            Some(Token::Eq) => BinOp::Eq,
            Some(Token::NotEq) => BinOp::Ne,
            Some(Token::Lt) => BinOp::Lt,
            Some(Token::Gt) => BinOp::Gt,
            _ => return Ok(left),
        };
        c.advance();
        left = Expr::bin(left, op, additive(c)?);
    }
}

fn additive(c: &mut Cursor) -> Result<Expr> {
    let mut left = multiplicative(c)?;
    loop {
        let op = match c.peek() {
            Some(Token::Plus) => BinOp::Add,
            Some(Token::Minus) => BinOp::Sub,
            _ => return Ok(left),
        };
        c.advance();
        left = Expr::bin(left, op, multiplicative(c)?);
    }
}

fn multiplicative(c: &mut Cursor) -> Result<Expr> {
    let mut left = unary(c)?;
    loop {
        let op = match c.peek() {
            Some(Token::Star) => BinOp::Mul,
            Some(Token::Slash) => BinOp::Div,
            _ => return Ok(left),
        };
        c.advance();
        left = Expr::bin(left, op, unary(c)?);
    }
}

fn unary(c: &mut Cursor) -> Result<Expr> {
    if c.eat(&Token::Minus) {
        Ok(Expr::un(UnOp::Neg, unary(c)?))
    } else {
        primary(c)
    }
}

fn primary(c: &mut Cursor) -> Result<Expr> {
    let expr = match c.peek() {
        Some(Token::Int(n)) => Expr::Literal(Value::Int(*n)),
        Some(Token::Text(s)) => Expr::Literal(Value::Text(s.clone())),
        Some(Token::Bool(b)) => Expr::Literal(Value::Bool(*b)),
        Some(Token::Null) => Expr::Literal(Value::Null),
        Some(Token::Ident(name)) => {
            let name = name.clone();
            c.advance();
            // `t.a` — the qualifier the planner resolves against the join's
            // left-then-right index space.
            return if c.eat(&Token::Dot) {
                let col = c.ident("a column name after `.`")?;
                Ok(Expr::qcol(name, col))
            } else {
                Ok(Expr::col(name))
            };
        }
        Some(Token::LParen) => {
            c.advance();
            let inner = parse_expr(c)?;
            c.expect(&Token::RParen)?;
            return Ok(inner);
        }
        Some(Token::Count) => return agg_call(c, AggFunc::Count),
        Some(Token::Sum) => return agg_call(c, AggFunc::Sum),
        Some(Token::Min) => return agg_call(c, AggFunc::Min),
        Some(Token::Max) => return agg_call(c, AggFunc::Max),
        Some(Token::Avg) => return agg_call(c, AggFunc::Avg),
        _ => return Err(c.expected("an expression")),
    };
    c.advance();
    Ok(expr)
}

/// `FUNC(expr)`, plus the one no-argument form: `COUNT(*)`.
fn agg_call(c: &mut Cursor, func: AggFunc) -> Result<Expr> {
    c.advance(); // the function keyword
    c.expect(&Token::LParen)?;
    let agg = if c.peek() == Some(&Token::Star) {
        if func != AggFunc::Count {
            return Err(QuernError::Parse(format!(
                "expected an expression in {}(..); only COUNT(*) takes a star",
                func.name()
            )));
        }
        c.advance();
        AggExpr::count_star()
    } else {
        let arg = parse_expr(c)?;
        if arg.contains_agg() {
            return Err(QuernError::Parse(format!(
                "aggregate nested inside {}(..)",
                func.name()
            )));
        }
        AggExpr::of(func, arg)
    };
    c.expect(&Token::RParen)?;
    Ok(Expr::agg(agg))
}

/// An expression in a clause that cannot contain an aggregate: `WHERE`, `ON`,
/// `GROUP BY`, `ORDER BY`. quern has no `HAVING`, and lowering only lifts
/// aggregates out of the SELECT list.
fn clause_expr(c: &mut Cursor, clause: &str) -> Result<Expr> {
    let e = parse_expr(c)?;
    if e.contains_agg() {
        return Err(QuernError::Parse(format!(
            "aggregate not allowed in {clause} (quern has no HAVING)"
        )));
    }
    Ok(e)
}

// --- SELECT -----------------------------------------------------------------

/// `SELECT ..` as a whole statement; a trailing `;` is allowed and anything
/// after it is an error.
pub fn parse_select(tokens: &[Token]) -> Result<Statement> {
    let mut c = Cursor::new(tokens);
    let stmt = select(&mut c)?;
    c.eat(&Token::Semicolon);
    if !c.at_end() {
        return Err(c.expected("end of statement"));
    }
    Ok(Statement::Select(stmt))
}

/// `SELECT ..` at the cursor, for a caller that dispatches on the first token.
pub(crate) fn select(c: &mut Cursor) -> Result<SelectStmt> {
    c.expect(&Token::Select)?;
    let projection = projection(c)?;
    c.expect(&Token::From)?;
    let from = c.ident("a table name after FROM")?;

    let join = if c.eat(&Token::Join) {
        let table = c.ident("a table name after JOIN")?;
        c.expect(&Token::On)?;
        Some(Join {
            table,
            on: clause_expr(c, "ON")?,
        })
    } else {
        None
    };
    if c.peek() == Some(&Token::Join) {
        return Err(QuernError::Parse(
            "at most one JOIN is supported".to_string(),
        ));
    }

    let predicate = if c.eat(&Token::Where) {
        Some(clause_expr(c, "WHERE")?)
    } else {
        None
    };

    let group_by = if c.eat(&Token::Group) {
        c.expect(&Token::By)?;
        expr_list(c, "GROUP BY")?
    } else {
        Vec::new()
    };

    let order_by = if c.eat(&Token::Order) {
        c.expect(&Token::By)?;
        order_keys(c)?
    } else {
        Vec::new()
    };

    let limit = if c.eat(&Token::Limit) {
        Some(limit_count(c)?)
    } else {
        None
    };

    Ok(SelectStmt {
        projection,
        from,
        join,
        predicate,
        group_by,
        order_by,
        limit,
    })
}

fn projection(c: &mut Cursor) -> Result<Vec<SelectItem>> {
    let mut items = Vec::new();
    loop {
        items.push(if c.eat(&Token::Star) {
            SelectItem::Star
        } else {
            let expr = parse_expr(c)?;
            if c.eat(&Token::As) {
                let alias = c.ident("an alias after AS")?;
                // An aliased aggregate carries the name twice: the SELECT item
                // holds it for Project, AggExpr::alias for the Aggregate node.
                match expr {
                    Expr::Agg(a) => {
                        SelectItem::aliased(Expr::agg((*a).with_alias(alias.clone())), alias)
                    }
                    e => SelectItem::aliased(e, alias),
                }
            } else {
                SelectItem::expr(expr)
            }
        });
        if !c.eat(&Token::Comma) {
            return Ok(items);
        }
    }
}

fn expr_list(c: &mut Cursor, clause: &str) -> Result<Vec<Expr>> {
    let mut out = Vec::new();
    loop {
        out.push(clause_expr(c, clause)?);
        if !c.eat(&Token::Comma) {
            return Ok(out);
        }
    }
}

/// `expr [ASC|DESC]` list; the `bool` is descending, ASC is the default.
fn order_keys(c: &mut Cursor) -> Result<Vec<(Expr, bool)>> {
    let mut keys = Vec::new();
    loop {
        let key = clause_expr(c, "ORDER BY")?;
        let desc = if c.eat(&Token::Desc) {
            true
        } else {
            c.eat(&Token::Asc);
            false
        };
        keys.push((key, desc));
        if !c.eat(&Token::Comma) {
            return Ok(keys);
        }
    }
}

fn limit_count(c: &mut Cursor) -> Result<usize> {
    match c.peek() {
        // The lexer never produces a negative Int (`-1` is Minus then Int), so
        // the guard is belt-and-braces against a hand-built token vector.
        Some(&Token::Int(n)) if n >= 0 => {
            c.advance();
            Ok(n as usize)
        }
        _ => Err(c.expected("a non-negative integer after LIMIT")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn expr(sql: &str) -> Expr {
        let toks = tokenize(sql).expect("lexes");
        let mut c = Cursor::new(&toks);
        let e = parse_expr(&mut c).expect("parses");
        assert!(c.at_end(), "{sql}: trailing tokens after expression");
        e
    }

    fn sel(sql: &str) -> SelectStmt {
        match parse_select(&tokenize(sql).expect("lexes")).expect("parses") {
            Statement::Select(s) => s,
            other => panic!("expected Select, got {other:?}"),
        }
    }

    fn err(sql: &str) -> String {
        let toks = tokenize(sql).expect("lexes");
        match parse_select(&toks) {
            Err(QuernError::Parse(m)) => m,
            other => panic!("{sql}: expected a parse error, got {other:?}"),
        }
    }

    #[test]
    fn precedence_ladder_binds_loosest_to_tightest() {
        // OR < AND < NOT < comparison, so NOT takes only `a = 1`.
        assert_eq!(
            expr("NOT a = 1 AND b > 2 OR c").to_string(),
            "((NOT (a = 1) AND (b > 2)) OR c)"
        );
        // comparison < additive < multiplicative < unary minus.
        assert_eq!(
            expr("1 + 2 * 3 - -4 / 2 > a").to_string(),
            "(((1 + (2 * 3)) - (-4 / 2)) > a)"
        );
        // Same-precedence operators are left-associative.
        assert_eq!(expr("1 - 2 - 3").to_string(), "((1 - 2) - 3)");
        assert_eq!(
            expr("NOT NOT c").to_string(),
            "NOT NOT c",
            "NOT is prefix and stacks"
        );
    }

    #[test]
    fn parentheses_override_precedence() {
        assert_eq!(expr("(a + 1) * 2").to_string(), "((a + 1) * 2)");
        assert_eq!(expr("a + 1 * 2").to_string(), "(a + (1 * 2))");
        assert_eq!(
            expr("NOT (a = 1 OR b = 2)").to_string(),
            "NOT ((a = 1) OR (b = 2))"
        );
        assert_eq!(expr("((((a))))").to_string(), "a");
    }

    #[test]
    fn primaries_are_literals_columns_and_qualified_columns() {
        assert_eq!(expr("42"), Expr::Literal(Value::Int(42)));
        assert_eq!(
            expr("'it''s'"),
            Expr::Literal(Value::Text("it's".to_string()))
        );
        assert_eq!(expr("TRUE"), Expr::Literal(Value::Bool(true)));
        assert_eq!(expr("NULL"), Expr::Literal(Value::Null));
        // Both spellings are Expr::Column — never a resolved ColumnRef.
        assert_eq!(expr("a"), Expr::col("a"));
        assert_eq!(expr("t.a"), Expr::qcol("t", "a"));
    }

    #[test]
    fn star_projection() {
        let s = sel("SELECT * FROM t;");
        assert_eq!(s.projection, vec![SelectItem::Star]);
        assert_eq!(s.from, "t");
        assert!(s.join.is_none() && s.predicate.is_none() && s.limit.is_none());
        assert!(s.group_by.is_empty() && s.order_by.is_empty());
    }

    #[test]
    fn projection_expressions_and_as_aliases() {
        let s = sel("SELECT a, b + 1 AS n, -a FROM t");
        assert_eq!(
            s.projection
                .iter()
                .map(SelectItem::output_name)
                .collect::<Vec<_>>(),
            vec![
                Some("a".to_string()),
                Some("n".to_string()),
                Some("-a".to_string()),
            ]
        );
        assert_eq!(
            s.projection[1],
            SelectItem::aliased(
                Expr::bin(Expr::col("b"), BinOp::Add, Expr::Literal(Value::Int(1))),
                "n"
            )
        );
    }

    #[test]
    fn join_with_qualified_columns() {
        let s = sel("SELECT t.a, u.b FROM t JOIN u ON t.a = u.a WHERE u.b <> 'q'");
        assert_eq!(
            s.projection,
            vec![
                SelectItem::expr(Expr::qcol("t", "a")),
                SelectItem::expr(Expr::qcol("u", "b")),
            ]
        );
        let join = s.join.expect("one join");
        assert_eq!(join.table, "u");
        assert_eq!(
            join.on,
            Expr::bin(Expr::qcol("t", "a"), BinOp::Eq, Expr::qcol("u", "a"))
        );
        assert_eq!(s.predicate.unwrap().to_string(), "(u.b <> 'q')");
    }

    #[test]
    fn all_five_aggregates_including_count_star() {
        let s = sel("SELECT b, COUNT(*), SUM(a), MIN(a), MAX(a), AVG(a) FROM t GROUP BY b");
        let aggs: Vec<&AggExpr> = s
            .projection
            .iter()
            .filter_map(|i| match i {
                SelectItem::Expr {
                    expr: Expr::Agg(a), ..
                } => Some(&**a),
                _ => None,
            })
            .collect();
        assert_eq!(aggs.len(), 5);
        assert!(aggs[0].is_count_star());
        assert_eq!(
            aggs.iter().map(|a| a.func).collect::<Vec<_>>(),
            vec![
                AggFunc::Count,
                AggFunc::Sum,
                AggFunc::Min,
                AggFunc::Max,
                AggFunc::Avg
            ]
        );
        // Aliases come from the source spelling via ast's constructors.
        assert_eq!(
            aggs.iter().map(|a| a.alias.as_str()).collect::<Vec<_>>(),
            vec!["COUNT(*)", "SUM(a)", "MIN(a)", "MAX(a)", "AVG(a)"]
        );
        assert_eq!(s.group_by, vec![Expr::col("b")]);
        // COUNT(a) is the argument form, and an expression argument parses.
        assert_eq!(
            sel("SELECT COUNT(a), SUM(a * 2) FROM t").projection[1].output_name(),
            Some("SUM((a * 2))".to_string())
        );
    }

    #[test]
    fn aliased_aggregate_names_both_halves() {
        let s = sel("SELECT SUM(t.a) AS total FROM t");
        let SelectItem::Expr {
            expr: Expr::Agg(a),
            alias,
        } = &s.projection[0]
        else {
            panic!("expected an aggregate item");
        };
        assert_eq!(alias.as_deref(), Some("total"));
        assert_eq!(a.alias, "total", "AggExpr::alias follows the AS alias");
        assert_eq!(a.arg, Some(Expr::qcol("t", "a")));
    }

    #[test]
    fn order_by_keys_and_limit() {
        let s = sel("SELECT a FROM t ORDER BY a DESC, b ASC, c LIMIT 5");
        assert_eq!(
            s.order_by,
            vec![
                (Expr::col("a"), true),
                (Expr::col("b"), false),
                (Expr::col("c"), false), // ASC is the default
            ]
        );
        assert_eq!(s.limit, Some(5));
        assert_eq!(sel("SELECT a FROM t LIMIT 0").limit, Some(0));
        assert_eq!(sel("SELECT a FROM t").limit, None);
        // ORDER BY takes expressions, not just column names.
        assert_eq!(
            sel("SELECT a FROM t ORDER BY b + 1 DESC").order_by[0]
                .0
                .to_string(),
            "(b + 1)"
        );
    }

    #[test]
    fn malformed_statements_are_errors_naming_what_was_expected() {
        // Truncated input: every one of these must not index past the end.
        assert!(err("SELECT a FROM").contains("table name after FROM"));
        assert!(err("SELECT a FROM t WHERE").contains("an expression"));
        assert!(err("SELECT").contains("an expression"));
        assert!(err("SELECT a FROM t ORDER BY").contains("an expression"));
        assert!(err("SELECT a FROM t LIMIT").contains("after LIMIT"));

        assert!(err("SELECT FROM t").contains("an expression"));
        assert!(err("SELECT (a + 1 FROM t").contains("RParen"));
        // No bare aliases: only `expr AS name`, so `a t` ends the projection
        // and FROM is what is missing.
        assert!(err("SELECT a t FROM t").contains("From"));
        // Two statements in one call is trailing garbage.
        assert!(err("SELECT a FROM t; SELECT b FROM t").contains("end of statement"));
        assert!(err("SELECT a FROM t JOIN u").contains("On"));
        assert!(err("SELECT a FROM t LIMIT x").contains("after LIMIT"));
        assert!(err("SELECT a FROM t GROUP b").contains("By"));
        assert!(
            err("SELECT a FROM t JOIN u ON t.a = u.a JOIN v ON t.a = v.a")
                .contains("at most one JOIN")
        );
        // No HAVING: an aggregate outside the SELECT list is a parse error.
        assert!(err("SELECT a FROM t WHERE COUNT(*) > 1").contains("no HAVING"));
        assert!(err("SELECT a FROM t GROUP BY SUM(a)").contains("no HAVING"));
        assert!(err("SELECT SUM(*) FROM t").contains("only COUNT(*)"));
        assert!(err("SELECT SUM(COUNT(a)) FROM t").contains("nested"));
        assert!(err("DELETE FROM t").contains("Select"), "not our statement");
    }

    #[test]
    fn every_prefix_of_a_full_statement_parses_or_errors_but_never_panics() {
        let sql = "SELECT t.b, COUNT(*), SUM(t.a) AS total FROM t JOIN u ON t.a = u.a \
                   WHERE u.b <> 'q' GROUP BY t.b ORDER BY t.b DESC LIMIT 5;";
        let toks = tokenize(sql).expect("lexes");
        for n in 0..=toks.len() {
            // The value is irrelevant; not panicking is the assertion.
            let _ = parse_select(&toks[..n]);
        }
        assert!(parse_select(&toks).is_ok(), "the whole statement parses");
    }
}
