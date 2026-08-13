//! bead: quern-parser-ddl — CREATE TABLE, DROP TABLE
//!
//! `CREATE TABLE t (a INT PRIMARY KEY, b TEXT, c BOOL)` and `DROP TABLE t`,
//! the whole of §1's DDL. The cursor and the error shape come from
//! [`crate::sql::parser_query`] — there is one `Cursor` in quern, not three.
//!
//! `CREATE TABLE` builds a `types::Schema` directly, so the parser is the only
//! place that turns `Token::IntType`/`TextType`/`BoolType` into `Type`. Note
//! those are the *type* keywords: `Token::Int`/`Text`/`Bool` are literals and
//! never appear in a column definition.
//!
//! Everything malformed is `Err(QuernError::Parse(..))`, never a panic:
//! unknown type, empty column list, a duplicate column name, a second
//! `PRIMARY KEY`, a missing `)`, and any trailing token. A trailing `;` is
//! optional, matching `parse_select`.
//!
//! Whether a `PRIMARY KEY` column is actually `INT` is not checked here —
//! `catalog.rs` owns that, along with every other cross-column rule.

use crate::sql::ast::Statement;
use crate::sql::parser_query::Cursor;
use crate::sql::token::Token;
use crate::types::{Column, QuernError, Result, Schema, Type};

/// `CREATE TABLE ..` as a whole statement; a trailing `;` is allowed and
/// anything after it is an error.
pub fn parse_create_table(tokens: &[Token]) -> Result<Statement> {
    let mut c = Cursor::new(tokens);
    let stmt = create_table(&mut c)?;
    end_of_statement(&mut c)?;
    Ok(stmt)
}

/// `DROP TABLE ..` as a whole statement, same trailing-`;` rule.
pub fn parse_drop_table(tokens: &[Token]) -> Result<Statement> {
    let mut c = Cursor::new(tokens);
    let stmt = drop_table(&mut c)?;
    end_of_statement(&mut c)?;
    Ok(stmt)
}

/// `CREATE TABLE ..` at the cursor, for a caller that dispatches on the first
/// token itself.
pub(crate) fn create_table(c: &mut Cursor) -> Result<Statement> {
    c.expect(&Token::Create)?;
    c.expect(&Token::Table)?;
    let table = c.ident("a table name after CREATE TABLE")?;
    c.expect(&Token::LParen)?;

    let mut columns: Vec<Column> = Vec::new();
    loop {
        let column = column_def(c, &table)?;
        if let Some(prev) = columns
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(&column.name))
        {
            // Case-insensitive, because §1's identifiers are and
            // `Schema::column_index` resolves them that way.
            return Err(QuernError::Parse(format!(
                "duplicate column name {} in CREATE TABLE {table}",
                prev.name
            )));
        }
        if column.primary_key {
            if let Some(prev) = columns.iter().find(|p| p.primary_key) {
                return Err(QuernError::Parse(format!(
                    "table {table} declares a second PRIMARY KEY on {}; {} already has it",
                    column.name, prev.name
                )));
            }
        }
        columns.push(column);
        if !c.eat(&Token::Comma) {
            break;
        }
    }
    c.expect(&Token::RParen)?;

    Ok(Statement::CreateTable {
        schema: Schema { table, columns },
    })
}

/// `DROP TABLE ..` at the cursor.
pub(crate) fn drop_table(c: &mut Cursor) -> Result<Statement> {
    c.expect(&Token::Drop)?;
    c.expect(&Token::Table)?;
    Ok(Statement::DropTable {
        table: c.ident("a table name after DROP TABLE")?,
    })
}

/// `name TYPE [PRIMARY KEY]`. An empty column list lands here on the `)` and
/// fails as "a column name", which is the error a user can act on.
fn column_def(c: &mut Cursor, table: &str) -> Result<Column> {
    let name = c.ident(&format!("a column name in CREATE TABLE {table}"))?;
    let ty = match c.advance() {
        Some(Token::IntType) => Type::Int,
        Some(Token::TextType) => Type::Text,
        Some(Token::BoolType) => Type::Bool,
        Some(t) => {
            return Err(QuernError::Parse(format!(
                "column {name} has unknown type {t:?}; quern has INT, TEXT and BOOL"
            )));
        }
        None => {
            return Err(QuernError::Parse(format!(
                "expected a type for column {name}, found end of input"
            )));
        }
    };
    let primary_key = c.eat(&Token::Primary);
    if primary_key {
        c.expect(&Token::Key)?;
    }
    Ok(Column {
        name,
        ty,
        primary_key,
    })
}

/// Optional `;`, then nothing. Same rule `parse_select` applies.
fn end_of_statement(c: &mut Cursor) -> Result<()> {
    c.eat(&Token::Semicolon);
    if c.at_end() {
        Ok(())
    } else {
        Err(c.expected("end of statement"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn ddl(sql: &str) -> Statement {
        let toks = tokenize(sql).expect("lexes");
        match toks.first() {
            Some(Token::Drop) => parse_drop_table(&toks),
            _ => parse_create_table(&toks),
        }
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    fn err(sql: &str) -> String {
        let toks = tokenize(sql).expect("lexes");
        let r = match toks.first() {
            Some(Token::Drop) => parse_drop_table(&toks),
            _ => parse_create_table(&toks),
        };
        match r {
            Ok(s) => panic!("{sql} parsed as {s:?}, expected an error"),
            Err(QuernError::Parse(m)) => m,
            Err(e) => panic!("{sql}: expected a Parse error, got {e:?}"),
        }
    }

    fn col(name: &str, ty: Type, primary_key: bool) -> Column {
        Column {
            name: name.to_string(),
            ty,
            primary_key,
        }
    }

    fn schema_of(s: Statement) -> Schema {
        match s {
            Statement::CreateTable { schema } => schema,
            other => panic!("expected CreateTable, got {other:?}"),
        }
    }

    #[test]
    fn every_type_with_and_without_primary_key() {
        for ty in [Type::Int, Type::Text, Type::Bool] {
            let plain = format!("CREATE TABLE t (a {ty})");
            let s = schema_of(ddl(&plain));
            assert_eq!(s.table, "t");
            assert_eq!(s.columns, vec![col("a", ty, false)], "{plain}");
            assert_eq!(s.primary_key(), None);

            let pk = format!("CREATE TABLE t (a {ty} PRIMARY KEY)");
            let s = schema_of(ddl(&pk));
            assert_eq!(s.columns, vec![col("a", ty, true)], "{pk}");
            assert_eq!(s.primary_key(), Some(0));
        }
    }

    #[test]
    fn multi_column_keeps_declaration_order() {
        let s = schema_of(ddl("CREATE TABLE t (a INT PRIMARY KEY, b TEXT, c BOOL);"));
        assert_eq!(
            s.columns,
            vec![
                col("a", Type::Int, true),
                col("b", Type::Text, false),
                col("c", Type::Bool, false),
            ]
        );
        // The PK need not be first.
        let s = schema_of(ddl("CREATE TABLE t (b TEXT, a INT PRIMARY KEY)"));
        assert_eq!(s.primary_key(), Some(1));
    }

    #[test]
    fn drop_table_names_the_table() {
        assert_eq!(
            ddl("DROP TABLE t"),
            Statement::DropTable {
                table: "t".to_string()
            }
        );
        assert_eq!(
            ddl("DROP TABLE t;"),
            Statement::DropTable {
                table: "t".to_string()
            }
        );
    }

    #[test]
    fn keywords_are_case_insensitive_but_names_keep_their_spelling() {
        let s = schema_of(ddl("create table T (Ab int primary key, cD Text)"));
        assert_eq!(s.table, "T");
        assert_eq!(
            s.columns,
            vec![col("Ab", Type::Int, true), col("cD", Type::Text, false)]
        );
        assert_eq!(
            ddl("dRoP tAbLe T"),
            Statement::DropTable {
                table: "T".to_string()
            }
        );
    }

    #[test]
    fn unknown_type_is_a_parse_error() {
        assert!(err("CREATE TABLE t (a FLOAT)").contains("unknown type"));
        // A *literal* INT/TEXT/BOOL token is not a type keyword either.
        assert!(err("CREATE TABLE t (a 1)").contains("unknown type"));
        assert!(err("CREATE TABLE t (a TRUE)").contains("unknown type"));
    }

    #[test]
    fn empty_column_list_is_a_parse_error() {
        assert!(err("CREATE TABLE t ()").contains("a column name"));
        assert!(err("CREATE TABLE t").contains("expected LParen"));
    }

    #[test]
    fn second_primary_key_is_a_parse_error() {
        let m = err("CREATE TABLE t (a INT PRIMARY KEY, b INT PRIMARY KEY)");
        assert!(m.contains("second PRIMARY KEY"), "{m}");
        assert!(m.contains('a') && m.contains('b'), "{m}");
        // PRIMARY without KEY is still an error, not a silent PK.
        assert!(err("CREATE TABLE t (a INT PRIMARY)").contains("expected Key"));
    }

    #[test]
    fn duplicate_column_name_is_a_parse_error_case_insensitively() {
        assert!(err("CREATE TABLE t (a INT, a TEXT)").contains("duplicate column name"));
        assert!(err("CREATE TABLE t (a INT, A TEXT)").contains("duplicate column name"));
    }

    #[test]
    fn missing_closing_paren_is_a_parse_error() {
        assert!(err("CREATE TABLE t (a INT").contains("expected RParen"));
        assert!(err("CREATE TABLE t (a INT, b TEXT;").contains("expected RParen"));
    }

    #[test]
    fn trailing_tokens_after_the_statement_are_a_parse_error() {
        assert!(err("CREATE TABLE t (a INT) x").contains("end of statement"));
        assert!(err("CREATE TABLE t (a INT);;").contains("end of statement"));
        assert!(err("DROP TABLE t u").contains("end of statement"));
        assert!(err("DROP TABLE t;;").contains("end of statement"));
    }

    #[test]
    fn every_prefix_of_a_ddl_statement_errors_but_never_panics() {
        for sql in [
            "CREATE TABLE t (a INT PRIMARY KEY, b TEXT);",
            "DROP TABLE t;",
        ] {
            let toks = tokenize(sql).expect("lexes");
            // The last prefix is the whole statement minus its optional `;`, so
            // it parses; every shorter one is truncated and must be an Err.
            for n in 0..toks.len() - 1 {
                let head = &toks[..n];
                let r = match head.first() {
                    Some(Token::Drop) => parse_drop_table(head),
                    _ => parse_create_table(head),
                };
                assert!(
                    matches!(r, Err(QuernError::Parse(_))),
                    "truncated {sql:?} to {n} tokens: expected a Parse error, got {r:?}"
                );
            }
        }
    }

    #[test]
    fn a_drop_parsed_as_create_errors_rather_than_confusing_the_dispatcher() {
        let toks = tokenize("DROP TABLE t").expect("lexes");
        assert!(matches!(
            parse_create_table(&toks),
            Err(QuernError::Parse(_))
        ));
    }
}
