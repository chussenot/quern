//! bead: treadle-lexer — &str -> Vec<Token>, line tracking
//!
//! Every line number in the program originates here: §3 puts a `line` on every
//! AST node and §4 compares error lines between the two engines, so a lexer
//! that miscounts by one makes both engines wrong in the same place — which the
//! differential fuzzer cannot see, because it only compares them to each other.
//! The counting rules are therefore pinned, not left to the reader:
//!
//! - **A newline inside a string literal advances the line, and the string's
//!   token carries the line it STARTED on.** Both halves are deliberate: a
//!   multi-line literal is real source text so it must move the counter, and an
//!   error about the literal should point at where the reader sees it begin. The
//!   alternative (the line it ended on) is defensible, so this pins it.
//! - **A `//` comment runs to end of line and the newline after it still
//!   counts** — the comment scanner stops *before* the `\n` and lets the main
//!   loop consume it, so there is one place that increments the counter.
//! - **`Eof` carries the line the last token ended on** (1 for a source with no
//!   tokens at all), not the line the counter happens to sit on after trailing
//!   whitespace. Files end with a newline, and an "expected `;`, found end of
//!   input" error must not point at the blank line past the program.
//!
//! Escapes are exactly the four of §2 — `\n` `\t` `\\` `\"`. An unknown escape
//! is an error rather than a silent passthrough, so `"\q"` cannot mean two
//! different things in two engines later.
//!
//! Nothing here panics: an unexpected character, an unterminated string and an
//! integer literal too large for i64 are all `TreadleError::Lex` with a line.

use crate::error::{Result, TreadleError};
use crate::front::token::{keyword, Token, TokenKind};

/// The lexer's only error shape.
///
/// The four wordings below are **agent `error`'s**, taken verbatim from its
/// announced constructors so that the swap is mechanical and changes no text:
/// `unterminated_string(line)`, `unknown_escape(line, ch)`,
/// `unexpected_char(line, ch)` and `bad_int(line, text)`. They are struct
/// literals only because `error.rs` (bead .2, a frozen §3 file with a single
/// author) had not landed on master yet — §4 makes the wording the contract, so
/// matching the strings now is what keeps the two from forking.
fn lex_err(line: u32, msg: impl Into<String>) -> TreadleError {
    TreadleError::Lex {
        line,
        msg: msg.into(),
    }
}

/// Source text to tokens, ending with exactly one [`TokenKind::Eof`].
pub fn tokenize(src: &str) -> Result<Vec<Token>> {
    let mut chars = src.chars().peekable();
    let mut out: Vec<Token> = Vec::new();
    // 1-based, and the counter the whole file is about.
    let mut line: u32 = 1;
    // The line the last token *ended* on — what Eof gets. See the module doc.
    let mut last_line: u32 = 1;

    while let Some(c) = chars.next() {
        // Captured before the match, so a token that spans lines (a multi-line
        // string) still reports where it began, and no arm has to remember to.
        let tok_line = line;

        let kind = match c {
            '\n' => {
                line += 1;
                continue;
            }
            ' ' | '\t' | '\r' => continue,

            '(' => TokenKind::LParen,
            ')' => TokenKind::RParen,
            '{' => TokenKind::LBrace,
            '}' => TokenKind::RBrace,
            ',' => TokenKind::Comma,
            ';' => TokenKind::Semi,
            '+' => TokenKind::Plus,
            '-' => TokenKind::Minus,
            '*' => TokenKind::Star,
            '%' => TokenKind::Percent,

            // The pairs. `next_if_eq` consumes the second character only when it
            // is there, which is the whole of "peek before you commit".
            '!' => two(&mut chars, '=', TokenKind::BangEq, TokenKind::Bang),
            '=' => two(&mut chars, '=', TokenKind::EqEq, TokenKind::Eq),
            '<' => two(&mut chars, '=', TokenKind::LtEq, TokenKind::Lt),
            '>' => two(&mut chars, '=', TokenKind::GtEq, TokenKind::Gt),

            '/' => {
                if chars.next_if_eq(&'/').is_some() {
                    // To end of line, and stop *before* the newline: the main
                    // loop increments the counter, so the line after a comment
                    // is numbered like any other.
                    while chars.next_if(|&c| c != '\n').is_some() {}
                    continue;
                }
                TokenKind::Slash
            }

            '"' => {
                let mut s = String::new();
                loop {
                    match chars.next() {
                        // Names the line the literal opened on, not the end of
                        // the file — that is the line the reader must fix.
                        None => {
                            return Err(lex_err(tok_line, "unterminated string"));
                        }
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some('\\') => s.push('\\'),
                            Some('"') => s.push('"'),
                            // The line the backslash is on, which for a
                            // multi-line literal is not the line it started on.
                            Some(bad) => {
                                return Err(lex_err(line, format!("unknown escape \\{bad}")));
                            }
                            None => {
                                return Err(lex_err(tok_line, "unterminated string"));
                            }
                        },
                        Some('\n') => {
                            line += 1;
                            s.push('\n');
                        }
                        Some(ch) => s.push(ch),
                    }
                }
                TokenKind::Str(s)
            }

            '0'..='9' => {
                let mut digits = String::from(c);
                while let Some(d) = chars.next_if(char::is_ascii_digit) {
                    digits.push(d);
                }
                // No sign here: `-7` is prefix minus applied to 7, so the
                // largest literal is i64::MAX and i64::MIN is reachable only as
                // `-9223372036854775808`, which the parser folds. A literal past
                // the range is an error, never a wrap and never a panic.
                match digits.parse::<i64>() {
                    Ok(n) => TokenKind::Int(n),
                    Err(_) => {
                        return Err(lex_err(tok_line, format!("not a valid integer: {digits}")));
                    }
                }
            }

            'a'..='z' | 'A'..='Z' | '_' => {
                let mut word = String::from(c);
                while let Some(ch) = chars.next_if(|&ch| ch.is_ascii_alphanumeric() || ch == '_') {
                    word.push(ch);
                }
                // Case-SENSITIVE, and `true`/`false`/`nil` come back already as
                // Bool/Nil, so there is no second pass over the token stream.
                keyword(&word).unwrap_or(TokenKind::Ident(word))
            }

            other => {
                return Err(lex_err(tok_line, format!("unexpected character '{other}'")));
            }
        };

        out.push(Token::new(kind, tok_line));
        last_line = line;
    }

    out.push(Token::new(TokenKind::Eof, last_line));
    Ok(out)
}

/// `c` then `second` is `long`; `c` alone is `short`.
fn two(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    second: char,
    long: TokenKind,
    short: TokenKind,
) -> TokenKind {
    if chars.next_if_eq(&second).is_some() {
        long
    } else {
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        tokenize(src)
            .expect("should lex")
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    /// Every token as `(kind, line)`, which is what the line-tracking cases are
    /// really about.
    fn spanned(src: &str) -> Vec<(TokenKind, u32)> {
        tokenize(src)
            .expect("should lex")
            .into_iter()
            .map(|t| (t.kind, t.line))
            .collect()
    }

    fn lex_error(src: &str) -> (u32, String) {
        match tokenize(src) {
            Err(TreadleError::Lex { line, msg }) => (line, msg),
            other => panic!("expected a Lex error, got {other:?}"),
        }
    }

    #[test]
    fn the_first_example_of_section_2_lexes() {
        assert_eq!(
            kinds("let x = 1;\nx = x + 1;\nprint x;"),
            vec![
                Let,
                Ident("x".into()),
                Eq,
                Int(1),
                Semi,
                Ident("x".into()),
                Eq,
                Ident("x".into()),
                Plus,
                Int(1),
                Semi,
                Print,
                Ident("x".into()),
                Semi,
                Eof,
            ]
        );
    }

    #[test]
    fn print_several_values_and_a_function_call() {
        assert_eq!(
            kinds("print \"a\", x, true;"),
            vec![
                Print,
                Str("a".into()),
                Comma,
                Ident("x".into()),
                Comma,
                Bool(true),
                Semi,
                Eof
            ]
        );
        assert_eq!(
            kinds("fn add(a, b) { return a + b; }"),
            vec![
                Fn,
                Ident("add".into()),
                LParen,
                Ident("a".into()),
                Comma,
                Ident("b".into()),
                RParen,
                LBrace,
                Return,
                Ident("a".into()),
                Plus,
                Ident("b".into()),
                Semi,
                RBrace,
                Eof
            ]
        );
    }

    /// A newline inside a literal advances the counter, and the literal's token
    /// still carries the line it opened on.
    #[test]
    fn newline_in_a_string_advances_the_line_but_the_token_keeps_its_start() {
        assert_eq!(
            spanned("print \"a\nb\";\nprint 2;"),
            vec![
                (Print, 1),
                (Str("a\nb".into()), 1), // opened on 1, closed on 2
                (Semi, 2),               // the `;` after it is genuinely on 2
                (Print, 3),
                (Int(2), 3),
                (Semi, 3),
                (Eof, 3),
            ]
        );
        // three embedded newlines move the counter by three
        assert_eq!(
            spanned("\"a\n\n\nb\" x"),
            vec![
                (Str("a\n\n\nb".into()), 1),
                (Ident("x".into()), 4),
                (Eof, 4)
            ]
        );
        // an escaped \n is NOT a source newline: same character in the value,
        // but the counter does not move.
        assert_eq!(
            spanned(r#""a\nb" x"#),
            vec![(Str("a\nb".into()), 1), (Ident("x".into()), 1), (Eof, 1)]
        );
    }

    /// The comment ends at the newline, and that newline still counts.
    #[test]
    fn a_comment_runs_to_end_of_line_and_its_newline_still_counts() {
        assert_eq!(
            spanned("let x = 1; // sets x\nprint x;"),
            vec![
                (Let, 1),
                (Ident("x".into()), 1),
                (Eq, 1),
                (Int(1), 1),
                (Semi, 1),
                (Print, 2), // not 1: the newline after the comment counted
                (Ident("x".into()), 2),
                (Semi, 2),
                (Eof, 2),
            ]
        );
        // a comment is not a token, and one on its own line still moves the count
        assert_eq!(kinds("// nothing here"), vec![Eof]);
        assert_eq!(
            spanned("// a\n// b\nx"),
            vec![(Ident("x".into()), 3), (Eof, 3)]
        );
        // a comment at end of file, unterminated by a newline, is fine
        assert_eq!(
            spanned("x // trailing"),
            vec![(Ident("x".into()), 1), (Eof, 1)]
        );
        // `//` inside a string is text, not a comment
        assert_eq!(
            kinds("\"// not a comment\""),
            vec![Str("// not a comment".into()), Eof]
        );
    }

    /// A token flush against the end of file, and what line `Eof` reports.
    #[test]
    fn a_token_at_end_of_file_and_the_line_eof_reports() {
        // no trailing newline: the last token and Eof share a line
        assert_eq!(
            spanned("print x"),
            vec![(Print, 1), (Ident("x".into()), 1), (Eof, 1)]
        );
        assert_eq!(
            spanned("a\nb\nc"),
            vec![
                (Ident("a".into()), 1),
                (Ident("b".into()), 2),
                (Ident("c".into()), 3),
                (Eof, 3),
            ]
        );
        // a trailing newline must NOT push Eof onto the blank line past the
        // program — otherwise every "found end of input" error is off by one.
        assert_eq!(
            spanned("a\nb\n"),
            vec![(Ident("a".into()), 1), (Ident("b".into()), 2), (Eof, 2)]
        );
        assert_eq!(spanned("a\n\n\n\n"), vec![(Ident("a".into()), 1), (Eof, 1)]);
        // and with nothing to point at, Eof is line 1
        assert_eq!(spanned(""), vec![(Eof, 1)]);
        assert_eq!(spanned("   \n\t\n"), vec![(Eof, 1)]);
        // CRLF: the \r is whitespace, the \n counts once
        assert_eq!(
            spanned("a\r\nb"),
            vec![(Ident("a".into()), 1), (Ident("b".into()), 2), (Eof, 2)]
        );
        // exactly one Eof, always last
        let toks = tokenize("a\nb").unwrap();
        assert_eq!(toks.iter().filter(|t| t.kind == Eof).count(), 1);
        assert_eq!(toks.last().unwrap().kind, Eof);
    }

    /// §4 compares error lines between the engines, so an error on the last line
    /// of a multi-line program must name that line.
    #[test]
    fn an_error_on_the_last_line_names_the_last_line() {
        assert_eq!(lex_error("let a = 1;\nlet b = 2;\nlet c = @;").0, 3);
        assert_eq!(lex_error("a;\nb;\nc;\nd;\n\"unclosed").0, 5);
        // and one in the middle names the middle
        assert_eq!(lex_error("a;\n@;\nc;").0, 2);
        // a comment before it does not shift the count
        assert_eq!(lex_error("// one\n// two\n@").0, 3);
        // nor does a multi-line string before it
        assert_eq!(lex_error("\"a\nb\";\n@").0, 3);
    }

    /// Exactly four escapes, and they decode to the characters themselves.
    #[test]
    fn all_four_escapes_and_nothing_else() {
        // source text: "\n\t\\\"" -> newline, tab, backslash, quote
        assert_eq!(kinds(r#""\n\t\\\"""#), vec![Str("\n\t\\\"".into()), Eof]);
        for (src, want) in [
            (r#""\n""#, "\n"),
            (r#""\t""#, "\t"),
            (r#""\\""#, "\\"),
            (r#""\"""#, "\""),
        ] {
            assert_eq!(kinds(src), vec![Str(want.into()), Eof], "{src}");
        }
        // an escaped quote does not close the literal
        assert_eq!(
            kinds(r#""a\"b" c"#),
            vec![Str("a\"b".into()), Ident("c".into()), Eof]
        );
        // an escaped backslash does not escape the quote after it
        assert_eq!(
            kinds(r#""a\\" c"#),
            vec![Str("a\\".into()), Ident("c".into()), Eof]
        );
        // the empty string, and one holding non-ASCII text
        assert_eq!(kinds(r#""""#), vec![Str(String::new()), Eof]);
        assert_eq!(
            kinds("\"héllo → 世界\""),
            vec![Str("héllo → 世界".into()), Eof]
        );
    }

    /// An unknown escape is an error, not a silent passthrough.
    #[test]
    fn an_unknown_escape_is_a_lex_error() {
        for src in [r#""\q""#, r#""\r""#, r#""\0""#, r#""\x41""#, r#""a\zb""#] {
            let (line, msg) = lex_error(src);
            assert_eq!(line, 1, "{src}");
            assert!(msg.contains("unknown escape"), "{src}: {msg}");
        }
        // it names the line the backslash is on, not the literal's first line
        assert_eq!(lex_error("\"a\n\\q\"").0, 2);
        // a backslash at end of input is an unterminated string, not an escape
        assert!(lex_error("\"a\\").1.contains("unterminated"));
    }

    /// An unterminated literal names the line it started on.
    #[test]
    fn an_unterminated_string_names_the_line_it_started_on() {
        let (line, msg) = lex_error("let s = \"abc;\nprint 1;");
        assert_eq!(line, 1);
        assert!(msg.contains("unterminated"), "{msg}");
        // started on 2, ran to end of file three lines later: still 2
        assert_eq!(lex_error("print 1;\nlet s = \"ab\ncd\nef").0, 2);
        assert_eq!(lex_error("\"").0, 1);
    }

    /// `!` vs `!=`, `<` vs `<=`, `>` vs `>=`, `=` vs `==`, `/` vs `//`.
    #[test]
    fn one_and_two_character_operators_are_lexed_apart() {
        for (src, want) in [
            ("!", Bang),
            ("!=", BangEq),
            ("<", Lt),
            ("<=", LtEq),
            (">", Gt),
            (">=", GtEq),
            ("=", Eq),
            ("==", EqEq),
            ("/", Slash),
            ("+", Plus),
            ("-", Minus),
            ("*", Star),
            ("%", Percent),
        ] {
            assert_eq!(kinds(src), vec![want.clone(), Eof], "{src}");
        }
        // maximal munch: the pair binds before the leftover single
        assert_eq!(kinds("!=="), vec![BangEq, Eq, Eof]);
        assert_eq!(kinds("<=<"), vec![LtEq, Lt, Eof]);
        assert_eq!(kinds(">=>"), vec![GtEq, Gt, Eof]);
        assert_eq!(kinds("==="), vec![EqEq, Eq, Eof]);
        // `/` vs `//`: two divisions, then a comment that eats the rest
        assert_eq!(kinds("/ /"), vec![Slash, Slash, Eof]);
        assert_eq!(
            kinds("a / b // c / d"),
            vec![Ident("a".into()), Slash, Ident("b".into()), Eof]
        );
        assert_eq!(
            kinds("a // b\n/ c"),
            vec![Ident("a".into()), Slash, Ident("c".into()), Eof]
        );
        // `and`/`or` are words, so the C spellings are not operators here
        assert!(lex_error("a && b").1.contains("unexpected character"));
    }

    #[test]
    fn an_integer_literal_too_large_for_i64_is_a_lex_error() {
        assert_eq!(kinds("9223372036854775807"), vec![Int(i64::MAX), Eof]);
        let (line, msg) = lex_error("9223372036854775808"); // i64::MAX + 1
        assert_eq!(line, 1);
        assert!(msg.contains("not a valid integer"), "{msg}");
        // absurdly long, and on a later line, still an error with the right line
        assert_eq!(
            lex_error(&format!("let x = 1;\nlet y = {};", "9".repeat(400))).0,
            2
        );
        // `-9223372036854775808` lexes as prefix minus applied to a literal that
        // is itself out of range; the parser folds the pair, so this is the one
        // place the boundary is visible.
        assert!(tokenize("-9223372036854775808").is_err());
        assert_eq!(
            kinds("-9223372036854775807"),
            vec![Minus, Int(i64::MAX), Eof]
        );
        assert_eq!(kinds("007"), vec![Int(7), Eof]);
    }

    #[test]
    fn an_unexpected_character_is_a_lex_error_with_its_line() {
        for (src, want_line) in [("@", 1), ("let x = 1;\n#", 2), ("a\nb\n$\n", 3)] {
            let (line, msg) = lex_error(src);
            assert_eq!(line, want_line, "{src}");
            assert!(msg.contains("unexpected character"), "{src}: {msg}");
        }
        // non-ASCII outside a string is unexpected, and does not panic on the
        // multi-byte boundary
        assert!(tokenize("let π = 1;").is_err());
    }

    /// The keyword table is case-sensitive, and that has to survive the stream.
    #[test]
    fn keywords_are_case_sensitive_in_the_token_stream() {
        assert_eq!(
            kinds("let Let true True nil NIL and And"),
            vec![
                Let,
                Ident("Let".into()),
                Bool(true),
                Ident("True".into()),
                Nil,
                Ident("NIL".into()),
                And,
                Ident("And".into()),
                Eof,
            ]
        );
        // a keyword is only a keyword whole: `lets` and `_let` are identifiers
        assert_eq!(
            kinds("lets _let let1"),
            vec![
                Ident("lets".into()),
                Ident("_let".into()),
                Ident("let1".into()),
                Eof
            ]
        );
    }
}
