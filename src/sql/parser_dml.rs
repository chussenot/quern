//! bead: quern-parser-dml — INSERT, UPDATE, DELETE, BEGIN/COMMIT/ROLLBACK
//!
//! The mutating half of §1's SQL surface. Everything here runs on
//! [`Cursor`](crate::sql::parser_query::Cursor) and
//! [`parse_expr`](crate::sql::parser_query::parse_expr) from `parser_query.rs`
//! — there is one expression parser in quern and it does not live here.
//!
//! [`parse_dml`] dispatches on the leading keyword; `parse_insert`,
//! `parse_update` and `parse_delete` are the same statements individually, in
//! the shape `parse_select` has. Every malformed or truncated input is an
//! `Err(QuernError::Parse)`, never a panic. A single trailing `;` is allowed.
//!
//! Aggregates are rejected in all three clauses (`VALUES`, `SET`, `WHERE`):
//! `parse_expr` cannot know which clause it is in, so the check is the
//! caller's, and `exec::eval` must never meet a bare `Agg`.
//!
//! What is deliberately NOT decided here: a named `INSERT` that omits a column
//! parses faithfully into `columns: Some(..)` with a shorter row, and
//! `plan::logical` decides whether that is a NULL-fill or an error (bead
//! bd_30-agents-dwm.53). Column names are not checked against any schema —
//! the parser has no catalog.

use crate::sql::ast::{Expr, Statement};
use crate::sql::parser_query::{parse_expr, Cursor};
use crate::sql::token::Token;
use crate::types::{QuernError, Result};

/// Any of the statements this module owns, dispatched on the first token.
/// `plan::mod` can call this, `parser_ddl`'s entry and `parse_select` in turn.
pub fn parse_dml(tokens: &[Token]) -> Result<Statement> {
    match tokens.first() {
        Some(Token::Insert) => parse_insert(tokens),
        Some(Token::Update) => parse_update(tokens),
        Some(Token::Delete) => parse_delete(tokens),
        Some(Token::Begin) | Some(Token::Commit) | Some(Token::Rollback) => whole(tokens, txn),
        _ => Err(Cursor::new(tokens).expected("INSERT, UPDATE, DELETE, BEGIN, COMMIT or ROLLBACK")),
    }
}

/// `INSERT INTO t [(a, b)] VALUES (..), (..)`
pub fn parse_insert(tokens: &[Token]) -> Result<Statement> {
    whole(tokens, insert)
}

/// `UPDATE t SET a = expr, .. [WHERE expr]`
pub fn parse_update(tokens: &[Token]) -> Result<Statement> {
    whole(tokens, update)
}

/// `DELETE FROM t [WHERE expr]`
pub fn parse_delete(tokens: &[Token]) -> Result<Statement> {
    whole(tokens, delete)
}

/// One whole statement and nothing else: optional trailing `;`, then end.
fn whole(tokens: &[Token], f: fn(&mut Cursor) -> Result<Statement>) -> Result<Statement> {
    let mut c = Cursor::new(tokens);
    let stmt = f(&mut c)?;
    c.eat(&Token::Semicolon);
    if !c.at_end() {
        return Err(c.expected("end of statement"));
    }
    Ok(stmt)
}

/// An expression in a clause that cannot contain an aggregate — which is all
/// of them here, since there is nothing to group by in a mutation.
fn dml_expr(c: &mut Cursor, clause: &str) -> Result<Expr> {
    let e = parse_expr(c)?;
    if e.contains_agg() {
        return Err(QuernError::Parse(format!(
            "aggregate not allowed in {clause}"
        )));
    }
    Ok(e)
}

fn insert(c: &mut Cursor) -> Result<Statement> {
    c.expect(&Token::Insert)?;
    c.expect(&Token::Into)?;
    let table = c.ident("a table name after INTO")?;

    // `None` is positional; `Some` is a named list, in the writer's order.
    let columns = if c.eat(&Token::LParen) {
        let mut cols: Vec<String> = Vec::new();
        loop {
            let name = c.ident("a column name in the INSERT column list")?;
            if cols.contains(&name) {
                return Err(QuernError::Parse(format!(
                    "column {name} appears twice in the INSERT column list"
                )));
            }
            cols.push(name);
            if !c.eat(&Token::Comma) {
                break;
            }
        }
        c.expect(&Token::RParen)?;
        Some(cols)
    } else {
        None
    };

    c.expect(&Token::Values)?;
    let mut rows: Vec<Vec<Expr>> = Vec::new();
    loop {
        c.expect(&Token::LParen)?;
        let mut row = Vec::new();
        loop {
            row.push(dml_expr(c, "an INSERT value")?);
            if !c.eat(&Token::Comma) {
                break;
            }
        }
        c.expect(&Token::RParen)?;
        // Ragged rows are a parse error, so lowering can trust rows[0].len().
        let want = columns
            .as_ref()
            .map(Vec::len)
            .or(rows.first().map(Vec::len));
        if let Some(want) = want {
            if want != row.len() {
                return Err(QuernError::Parse(format!(
                    "INSERT row has {} values, expected {want}",
                    row.len()
                )));
            }
        }
        rows.push(row);
        if !c.eat(&Token::Comma) {
            break;
        }
    }

    Ok(Statement::Insert {
        table,
        columns,
        rows,
    })
}

fn update(c: &mut Cursor) -> Result<Statement> {
    c.expect(&Token::Update)?;
    let table = c.ident("a table name after UPDATE")?;
    c.expect(&Token::Set)?;
    let mut sets = Vec::new();
    loop {
        let col = c.ident("a column name in SET")?;
        c.expect(&Token::Eq)?;
        sets.push((col, dml_expr(c, "an UPDATE SET expression")?));
        if !c.eat(&Token::Comma) {
            break;
        }
    }
    Ok(Statement::Update {
        table,
        sets,
        predicate: predicate(c, "UPDATE WHERE")?,
    })
}

fn delete(c: &mut Cursor) -> Result<Statement> {
    c.expect(&Token::Delete)?;
    c.expect(&Token::From)?;
    let table = c.ident("a table name after FROM")?;
    Ok(Statement::Delete {
        table,
        predicate: predicate(c, "DELETE WHERE")?,
    })
}

fn predicate(c: &mut Cursor, clause: &str) -> Result<Option<Expr>> {
    if c.eat(&Token::Where) {
        Ok(Some(dml_expr(c, clause)?))
    } else {
        Ok(None)
    }
}

fn txn(c: &mut Cursor) -> Result<Statement> {
    match c.advance() {
        Some(Token::Begin) => Ok(Statement::Begin),
        Some(Token::Commit) => Ok(Statement::Commit),
        Some(Token::Rollback) => Ok(Statement::Rollback),
        _ => Err(c.expected("BEGIN, COMMIT or ROLLBACK")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::BinOp;
    use crate::sql::lexer::tokenize;
    use crate::types::Value;

    fn stmt(sql: &str) -> Statement {
        parse_dml(&tokenize(sql).expect("lexes")).expect("parses")
    }

    fn err(sql: &str) -> String {
        match parse_dml(&tokenize(sql).expect("lexes")) {
            Err(QuernError::Parse(m)) => m,
            other => panic!("{sql}: expected a parse error, got {other:?}"),
        }
    }

    fn int(n: i64) -> Expr {
        Expr::Literal(Value::Int(n))
    }

    #[test]
    fn positional_insert_has_no_column_list() {
        assert_eq!(
            stmt("INSERT INTO t VALUES (1, 'x', TRUE);"),
            Statement::Insert {
                table: "t".to_string(),
                columns: None,
                rows: vec![vec![
                    int(1),
                    Expr::Literal(Value::Text("x".to_string())),
                    Expr::Literal(Value::Bool(true)),
                ]],
            }
        );
    }

    #[test]
    fn named_insert_keeps_the_writers_column_order() {
        let Statement::Insert { columns, rows, .. } =
            stmt("INSERT INTO t (a, b, c) VALUES (1, 'x', TRUE)")
        else {
            panic!("expected an Insert");
        };
        assert_eq!(
            columns.as_deref(),
            Some(["a", "b", "c"].map(String::from).as_slice())
        );
        assert_eq!(rows.len(), 1);

        // A list in a different order from the schema is NOT normalised here;
        // plan::logical permutes it. The parser reports what was written.
        let Statement::Insert { columns, rows, .. } =
            stmt("INSERT INTO t (c, b, a) VALUES (TRUE, 'three', 3)")
        else {
            panic!("expected an Insert");
        };
        assert_eq!(
            columns.as_deref(),
            Some(["c", "b", "a"].map(String::from).as_slice())
        );
        assert_eq!(rows[0][2], int(3));

        // A short list (bead .53) parses faithfully; the result is lowering's.
        let Statement::Insert { columns, rows, .. } = stmt("INSERT INTO t (a) VALUES (1)") else {
            panic!("expected an Insert");
        };
        assert_eq!(columns.as_deref(), Some(["a".to_string()].as_slice()));
        assert_eq!(rows, vec![vec![int(1)]]);
    }

    #[test]
    fn multi_row_insert_and_value_expressions() {
        let Statement::Insert { rows, .. } =
            stmt("INSERT INTO t VALUES (1, 2 * 3), (2, -1), (3, NULL)")
        else {
            panic!("expected an Insert");
        };
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][1].to_string(), "(2 * 3)");
        assert_eq!(rows[1][1].to_string(), "-1");
        assert_eq!(rows[2][1], Expr::Literal(Value::Null));
    }

    #[test]
    fn update_one_and_many_columns_with_and_without_where() {
        assert_eq!(
            stmt("UPDATE t SET a = 1"),
            Statement::Update {
                table: "t".to_string(),
                sets: vec![("a".to_string(), int(1))],
                predicate: None,
            }
        );
        assert_eq!(
            stmt("UPDATE t SET b = 'w', c = FALSE WHERE a = 2;"),
            Statement::Update {
                table: "t".to_string(),
                sets: vec![
                    ("b".to_string(), Expr::Literal(Value::Text("w".to_string()))),
                    ("c".to_string(), Expr::Literal(Value::Bool(false))),
                ],
                predicate: Some(Expr::bin(Expr::col("a"), BinOp::Eq, int(2))),
            }
        );
        // The right-hand side is a full expression, columns included.
        let Statement::Update {
            sets, predicate, ..
        } = stmt("UPDATE t SET a = a + 1 WHERE NOT c OR a < 3")
        else {
            panic!("expected an Update");
        };
        assert_eq!(sets[0].1.to_string(), "(a + 1)");
        assert_eq!(predicate.unwrap().to_string(), "(NOT c OR (a < 3))");
    }

    #[test]
    fn delete_with_and_without_where() {
        assert_eq!(
            stmt("DELETE FROM t"),
            Statement::Delete {
                table: "t".to_string(),
                predicate: None,
            }
        );
        assert_eq!(
            stmt("DELETE FROM t WHERE a = 3;"),
            Statement::Delete {
                table: "t".to_string(),
                predicate: Some(Expr::bin(Expr::col("a"), BinOp::Eq, int(3))),
            }
        );
    }

    #[test]
    fn all_three_transaction_statements() {
        assert_eq!(stmt("BEGIN"), Statement::Begin);
        assert_eq!(stmt("BEGIN;"), Statement::Begin);
        assert_eq!(stmt("COMMIT;"), Statement::Commit);
        assert_eq!(stmt("ROLLBACK"), Statement::Rollback);
        assert!(err("BEGIN COMMIT").contains("end of statement"));
    }

    #[test]
    fn malformed_statements_are_errors_naming_what_was_expected() {
        // VALUES with no rows, empty and duplicate column lists.
        assert!(err("INSERT INTO t VALUES;").contains("LParen"));
        assert!(err("INSERT INTO t VALUES ()").contains("an expression"));
        assert!(err("INSERT INTO t () VALUES (1)").contains("a column name"));
        assert!(err("INSERT INTO t (a, a) VALUES (1, 2)").contains("twice"));
        // Ragged rows, and a row that disagrees with the column list.
        assert!(err("INSERT INTO t VALUES (1, 2), (3)").contains("expected 2"));
        assert!(err("INSERT INTO t (a, b) VALUES (1)").contains("expected 2"));
        // SET with no assignments, missing FROM, trailing tokens.
        assert!(err("UPDATE t SET WHERE a = 1").contains("a column name in SET"));
        assert!(err("UPDATE t SET a = 1, WHERE a = 2").contains("a column name in SET"));
        assert!(err("DELETE t WHERE a = 1").contains("From"));
        assert!(err("DELETE FROM t WHERE a = 1 garbage").contains("end of statement"));
        assert!(err("DELETE FROM t; DELETE FROM u").contains("end of statement"));
        // No aggregate is legal in any clause a mutation has.
        assert!(err("INSERT INTO t VALUES (COUNT(*))").contains("aggregate not allowed"));
        assert!(err("UPDATE t SET a = SUM(b)").contains("aggregate not allowed"));
        assert!(err("UPDATE t SET a = 1 WHERE COUNT(*) > 0").contains("aggregate not allowed"));
        assert!(err("DELETE FROM t WHERE MAX(a) = 1").contains("aggregate not allowed"));
        // Not our statements.
        assert!(err("SELECT a FROM t").contains("INSERT, UPDATE"));
        assert!(err("").contains("INSERT, UPDATE"));
        // Truncation, keyword by keyword.
        assert!(err("INSERT").contains("Into"));
        assert!(err("INSERT INTO").contains("table name after INTO"));
        assert!(err("INSERT INTO t").contains("Values"));
        assert!(err("INSERT INTO t (a").contains("RParen"));
        assert!(err("INSERT INTO t VALUES (1").contains("RParen"));
        assert!(err("UPDATE").contains("table name after UPDATE"));
        assert!(err("UPDATE t").contains("Set"));
        assert!(err("UPDATE t SET a").contains("Eq"));
        assert!(err("UPDATE t SET a =").contains("an expression"));
        assert!(err("UPDATE t SET a = 1 WHERE").contains("an expression"));
        assert!(err("DELETE").contains("From"));
        assert!(err("DELETE FROM").contains("table name after FROM"));
    }

    #[test]
    fn every_prefix_of_every_statement_parses_or_errors_but_never_panics() {
        for sql in [
            "INSERT INTO t (a, b, c) VALUES (1, 'x', TRUE), (2, 'y', FALSE);",
            "UPDATE t SET b = 'w', c = FALSE WHERE a = 2 + 3 * -4;",
            "DELETE FROM t WHERE a = 3 AND NOT c;",
            "BEGIN;",
        ] {
            let toks = tokenize(sql).expect("lexes");
            for n in 0..=toks.len() {
                // The value is irrelevant; not panicking is the assertion.
                let _ = parse_dml(&toks[..n]);
            }
            assert!(parse_dml(&toks).is_ok(), "{sql} parses whole");
        }
    }

    #[test]
    fn the_individual_entry_points_agree_with_the_dispatcher() {
        let cases = [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1",
            "DELETE FROM t",
        ];
        let direct = [parse_insert, parse_update, parse_delete];
        for (sql, f) in cases.iter().zip(direct) {
            let toks = tokenize(sql).expect("lexes");
            assert_eq!(f(&toks).expect("parses"), stmt(sql));
        }
        // Each one only accepts its own statement.
        assert!(parse_insert(&tokenize("DELETE FROM t").unwrap()).is_err());
    }
}
