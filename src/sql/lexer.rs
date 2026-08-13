//! bead: quern-lexer — &str -> Vec<Token>

use crate::sql::token::Token;
use crate::types::{QuernError, Result};

/// Turn SQL text into tokens. Malformed input is an `Err(QuernError::Parse)`,
/// never a panic (docs/quern.md §1).
pub fn tokenize(input: &str) -> Result<Vec<Token>> {
    let cs: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < cs.len() {
        let c = cs[i];
        match c {
            _ if c.is_whitespace() => i += 1,

            // `--` runs to end of line; a lone `-` is the operator.
            '-' if cs.get(i + 1) == Some(&'-') => {
                while i < cs.len() && cs[i] != '\n' {
                    i += 1;
                }
            }
            '-' => {
                out.push(Token::Minus);
                i += 1;
            }

            '\'' => {
                let (text, next) = string_literal(&cs, i)?;
                out.push(Token::Text(text));
                i = next;
            }

            _ if c.is_ascii_digit() => {
                let start = i;
                while i < cs.len() && cs[i].is_ascii_digit() {
                    i += 1;
                }
                let digits: String = cs[start..i].iter().collect();
                let n = digits
                    .parse::<i64>()
                    .map_err(|e| QuernError::Parse(format!("bad integer `{digits}`: {e}")))?;
                out.push(Token::Int(n));
            }

            _ if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_') {
                    i += 1;
                }
                let word: String = cs[start..i].iter().collect();
                // Keywords fold to uppercase for lookup; identifiers keep their
                // spelling as written.
                out.push(Token::keyword(&word).unwrap_or(Token::Ident(word)));
            }

            // `<>` before `<`, so `<` never swallows a following `>`.
            '<' if cs.get(i + 1) == Some(&'>') => {
                out.push(Token::NotEq);
                i += 2;
            }

            _ => {
                let tok = match c {
                    '+' => Token::Plus,
                    '*' => Token::Star,
                    '/' => Token::Slash,
                    '=' => Token::Eq,
                    '<' => Token::Lt,
                    '>' => Token::Gt,
                    '(' => Token::LParen,
                    ')' => Token::RParen,
                    ',' => Token::Comma,
                    ';' => Token::Semicolon,
                    '.' => Token::Dot,
                    _ => {
                        return Err(QuernError::Parse(format!(
                            "unexpected character `{c}` at position {i}"
                        )))
                    }
                };
                out.push(tok);
                i += 1;
            }
        }
    }

    Ok(out)
}

/// Reads a single-quoted string starting at `open` (the opening quote).
/// `''` inside the string is one literal quote. Returns the value and the
/// index just past the closing quote.
fn string_literal(cs: &[char], open: usize) -> Result<(String, usize)> {
    let mut s = String::new();
    let mut i = open + 1;
    while i < cs.len() {
        match cs[i] {
            '\'' if cs.get(i + 1) == Some(&'\'') => {
                s.push('\'');
                i += 2;
            }
            '\'' => return Ok((s, i + 1)),
            c => {
                s.push(c);
                i += 1;
            }
        }
    }
    Err(QuernError::Parse(format!(
        "unterminated string literal starting at position {open}"
    )))
}

#[cfg(test)]
mod tests {
    use super::tokenize;
    use crate::sql::token::Token;
    use crate::types::QuernError;

    /// `unwrap()` would need `QuernError: Debug`; this needs nothing of it.
    fn lex(sql: &str) -> Vec<Token> {
        match tokenize(sql) {
            Ok(tokens) => tokens,
            Err(_) => panic!("tokenize({sql:?}) unexpectedly failed"),
        }
    }

    #[test]
    fn keywords_case_insensitive_identifiers_preserved() {
        assert_eq!(
            lex("sElEcT MyCol FROM Tbl;"),
            vec![
                Token::Select,
                Token::Ident("MyCol".into()),
                Token::From,
                Token::Ident("Tbl".into()),
                Token::Semicolon,
            ]
        );
    }

    #[test]
    fn doubled_quote_is_an_escape() {
        assert_eq!(lex("'it''s'"), vec![Token::Text("it's".into())]);
        assert_eq!(lex("''"), vec![Token::Text(String::new())]);
        assert_eq!(
            lex("'a''' , 'b'"),
            vec![
                Token::Text("a'".into()),
                Token::Comma,
                Token::Text("b".into())
            ]
        );
    }

    #[test]
    fn not_eq_versus_less_than() {
        assert_eq!(
            lex("a <> 1 AND b < 2 AND c > 3"),
            vec![
                Token::Ident("a".into()),
                Token::NotEq,
                Token::Int(1),
                Token::And,
                Token::Ident("b".into()),
                Token::Lt,
                Token::Int(2),
                Token::And,
                Token::Ident("c".into()),
                Token::Gt,
                Token::Int(3),
            ]
        );
        // `<` followed by a separate `>` stays two tokens.
        assert_eq!(lex("< >"), vec![Token::Lt, Token::Gt]);
    }

    #[test]
    fn comments_run_to_end_of_line() {
        assert_eq!(
            lex("SELECT -- everything\n * FROM t -- trailing"),
            vec![
                Token::Select,
                Token::Star,
                Token::From,
                Token::Ident("t".into())
            ]
        );
        // A lone minus is still the operator.
        assert_eq!(
            lex("1 - 2"),
            vec![Token::Int(1), Token::Minus, Token::Int(2)]
        );
    }

    #[test]
    fn literals_and_types() {
        assert_eq!(
            lex("INSERT INTO t VALUES (1, 'x', TRUE, FALSE, NULL)"),
            vec![
                Token::Insert,
                Token::Into,
                Token::Ident("t".into()),
                Token::Values,
                Token::LParen,
                Token::Int(1),
                Token::Comma,
                Token::Text("x".into()),
                Token::Comma,
                Token::Bool(true),
                Token::Comma,
                Token::Bool(false),
                Token::Comma,
                Token::Null,
                Token::RParen,
            ]
        );
    }

    #[test]
    fn malformed_input_errors_never_panics() {
        assert!(matches!(
            tokenize("'unterminated"),
            Err(QuernError::Parse(_))
        ));
        assert!(matches!(
            tokenize("SELECT # FROM t"),
            Err(QuernError::Parse(_))
        ));
        // out of i64 range
        assert!(matches!(
            tokenize("99999999999999999999999"),
            Err(QuernError::Parse(_))
        ));
    }
}
