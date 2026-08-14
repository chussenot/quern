//! bead: treadle-token — Token enum + keywords
//!
//! The whole lexical surface of §2, and the origin of every line number.
//!
//! **Representation:** a token is [`Token`] — a `{ kind, line }` struct, not a
//! line baked into each variant. §3 requires every AST node to carry a `line`
//! and §4 makes the line part of the observable output, so the parser reads
//! `tok.line` uniformly, for every kind, with no per-variant accessor and no
//! variant that can forget to carry one. Match on `tok.kind`, take `tok.line`.
//!
//! **Keywords are case-SENSITIVE.** `keyword()` is an exact `match` on the
//! word's bytes, so `Let`, `LET` and `nIl` are identifiers. This is unlike the
//! SQL dialect of the previous run; do not lowercase before the lookup.

use std::fmt;

/// What a token is. Literal payloads are plain Rust types, not `Value`: the
/// lexer knows nothing about `value.rs`, and the parser builds `Expr::Lit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    // literals
    Int(i64),
    /// The decoded contents of a string literal, without the quotes.
    Str(String),
    Bool(bool),
    Nil,

    Ident(String),

    // operators, in §2 precedence order, loosest first
    Or,      // prec 1
    And,     // prec 2
    EqEq,    // prec 3
    BangEq,  // prec 3
    Lt,      // prec 4
    Gt,      // prec 4
    LtEq,    // prec 4
    GtEq,    // prec 4
    Plus,    // prec 5
    Minus,   // prec 5, also prefix (prec 7)
    Star,    // prec 6
    Slash,   // prec 6
    Percent, // prec 6
    Bang,    // prec 7, prefix only

    /// Assignment `=`. Not an operator in the §2 table — `let x = 1;` and
    /// `x = x + 1;` are statements — but the source contains it, so it is here.
    Eq,

    // punctuation
    LParen,
    RParen,
    LBrace,
    RBrace,
    Comma,
    Semi,

    // keywords that are not literals or operators
    Let,
    Print,
    If,
    Else,
    While,
    Fn,
    Return,

    /// End of input. Carries the last line, so a parse error at EOF has one.
    Eof,
}

/// A token and the 1-based source line it started on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub line: u32,
}

impl Token {
    pub fn new(kind: TokenKind, line: u32) -> Self {
        Token { kind, line }
    }
}

/// The keyword table: the twelve reserved words of §2, **case-sensitively**.
///
/// `true`, `false` and `nil` are keywords *and* literals, so they map straight
/// to their literal kinds — the lexer never needs a second pass, and they
/// cannot be used as identifiers.
pub fn keyword(word: &str) -> Option<TokenKind> {
    // A `match` on &str compares bytes exactly: `Let` falls through to `None`
    // and stays an identifier. Do not add a `to_lowercase()` here.
    Some(match word {
        "let" => TokenKind::Let,
        "print" => TokenKind::Print,
        "if" => TokenKind::If,
        "else" => TokenKind::Else,
        "while" => TokenKind::While,
        "fn" => TokenKind::Fn,
        "return" => TokenKind::Return,
        "true" => TokenKind::Bool(true),
        "false" => TokenKind::Bool(false),
        "nil" => TokenKind::Nil,
        "and" => TokenKind::And,
        "or" => TokenKind::Or,
        _ => return None,
    })
}

/// How a token is named in an error message. One formatter so the two parser
/// beads cannot drift on wording — §4 makes message text a contract.
impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenKind::Int(n) => write!(f, "{n}"),
            TokenKind::Str(s) => write!(f, "\"{s}\""),
            TokenKind::Bool(true) => f.write_str("true"),
            TokenKind::Bool(false) => f.write_str("false"),
            TokenKind::Nil => f.write_str("nil"),
            TokenKind::Ident(name) => f.write_str(name),
            TokenKind::Or => f.write_str("or"),
            TokenKind::And => f.write_str("and"),
            TokenKind::EqEq => f.write_str("=="),
            TokenKind::BangEq => f.write_str("!="),
            TokenKind::Lt => f.write_str("<"),
            TokenKind::Gt => f.write_str(">"),
            TokenKind::LtEq => f.write_str("<="),
            TokenKind::GtEq => f.write_str(">="),
            TokenKind::Plus => f.write_str("+"),
            TokenKind::Minus => f.write_str("-"),
            TokenKind::Star => f.write_str("*"),
            TokenKind::Slash => f.write_str("/"),
            TokenKind::Percent => f.write_str("%"),
            TokenKind::Bang => f.write_str("!"),
            TokenKind::Eq => f.write_str("="),
            TokenKind::LParen => f.write_str("("),
            TokenKind::RParen => f.write_str(")"),
            TokenKind::LBrace => f.write_str("{"),
            TokenKind::RBrace => f.write_str("}"),
            TokenKind::Comma => f.write_str(","),
            TokenKind::Semi => f.write_str(";"),
            TokenKind::Let => f.write_str("let"),
            TokenKind::Print => f.write_str("print"),
            TokenKind::If => f.write_str("if"),
            TokenKind::Else => f.write_str("else"),
            TokenKind::While => f.write_str("while"),
            TokenKind::Fn => f.write_str("fn"),
            TokenKind::Return => f.write_str("return"),
            TokenKind::Eof => f.write_str("end of input"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every keyword of §2, and nothing else, is reserved.
    #[test]
    fn every_keyword_is_recognised() {
        let table = [
            ("let", TokenKind::Let),
            ("print", TokenKind::Print),
            ("if", TokenKind::If),
            ("else", TokenKind::Else),
            ("while", TokenKind::While),
            ("fn", TokenKind::Fn),
            ("return", TokenKind::Return),
            ("true", TokenKind::Bool(true)),
            ("false", TokenKind::Bool(false)),
            ("nil", TokenKind::Nil),
            ("and", TokenKind::And),
            ("or", TokenKind::Or),
        ];
        assert_eq!(table.len(), 12, "§2 has twelve keywords");
        for (word, want) in table {
            assert_eq!(keyword(word), Some(want.clone()), "keyword {word}");
            // and the keyword's own display form round-trips to its source text
            assert_eq!(want.to_string(), word, "display of keyword {word}");
        }
    }

    /// The one that bites an author coming from run 5's case-insensitive SQL.
    #[test]
    fn keywords_are_case_sensitive() {
        for word in [
            "Let", "LET", "lEt", "If", "IF", "While", "WHILE", "Fn", "FN", "Print", "PRINT",
            "Return", "Else", "True", "TRUE", "False", "FALSE", "Nil", "NIL", "And", "AND", "Or",
            "OR",
        ] {
            assert_eq!(keyword(word), None, "{word} must be an identifier");
        }
    }

    #[test]
    fn non_keywords_are_identifiers() {
        for word in [
            "", "x", "lets", "iff", "printx", "_let", "and_", "nil2", "orange",
        ] {
            assert_eq!(keyword(word), None, "{word} must be an identifier");
        }
    }

    /// The operator pairs a lexer mis-lexes when it forgets to peek: each pair
    /// is two distinct kinds with two distinct spellings.
    #[test]
    fn one_and_two_character_operators_are_distinct() {
        for (short, long) in [
            (TokenKind::Bang, TokenKind::BangEq),
            (TokenKind::Lt, TokenKind::LtEq),
            (TokenKind::Gt, TokenKind::GtEq),
            (TokenKind::Eq, TokenKind::EqEq),
        ] {
            assert_ne!(short, long);
            assert_eq!(long.to_string(), format!("{short}="));
            assert_eq!(short.to_string().len(), 1);
        }
    }

    #[test]
    fn every_token_carries_its_line() {
        let t = Token::new(TokenKind::Ident("x".into()), 7);
        assert_eq!(t.line, 7);
        assert_eq!(t.kind, TokenKind::Ident("x".to_string()));
        // the line is on the token, not the kind: same kind, different lines
        assert_ne!(
            Token::new(TokenKind::Semi, 1),
            Token::new(TokenKind::Semi, 2)
        );
    }

    #[test]
    fn literal_and_eof_display_forms() {
        assert_eq!(TokenKind::Int(-7).to_string(), "-7");
        assert_eq!(TokenKind::Str("a b".into()).to_string(), "\"a b\"");
        assert_eq!(TokenKind::Eof.to_string(), "end of input");
    }
}
