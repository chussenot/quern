//! bead: quern-token — Token enum, keyword table

/// Every lexical item in the quern SQL surface (docs/quern.md §1).
///
/// Literals carry their value (`Int`, `Text`, `Bool`, `Null`); `TRUE`/`FALSE`
/// lex straight to `Bool`, so the type keywords are spelled `IntType`,
/// `TextType`, `BoolType` to keep them distinct from the literals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    // identifiers and literals
    Ident(String),
    Int(i64),
    Text(String),
    Bool(bool),
    Null,

    // operators: + - * / = <> < >  (`Star` doubles as `*` in `COUNT(*)`)
    Plus,
    Minus,
    Star,
    Slash,
    Eq,
    NotEq,
    Lt,
    Gt,

    // punctuation
    LParen,
    RParen,
    Comma,
    Semicolon,
    Dot,

    // keywords
    Select,
    From,
    Where,
    Order,
    By,
    Asc,
    Desc,
    Limit,
    Insert,
    Into,
    Values,
    Update,
    Set,
    Delete,
    Create,
    Table,
    Drop,
    Primary,
    Key,
    IntType,
    TextType,
    BoolType,
    Join,
    On,
    Group,
    And,
    Or,
    Not,
    Count,
    Sum,
    Min,
    Max,
    Avg,
    As,
    Begin,
    Commit,
    Rollback,
}

impl Token {
    /// Keyword table. `word` is matched case-insensitively; `None` means the
    /// word is an identifier and should keep its spelling as written.
    pub fn keyword(word: &str) -> Option<Token> {
        let kw = match word.to_ascii_uppercase().as_str() {
            "SELECT" => Token::Select,
            "FROM" => Token::From,
            "WHERE" => Token::Where,
            "ORDER" => Token::Order,
            "BY" => Token::By,
            "ASC" => Token::Asc,
            "DESC" => Token::Desc,
            "LIMIT" => Token::Limit,
            "INSERT" => Token::Insert,
            "INTO" => Token::Into,
            "VALUES" => Token::Values,
            "UPDATE" => Token::Update,
            "SET" => Token::Set,
            "DELETE" => Token::Delete,
            "CREATE" => Token::Create,
            "TABLE" => Token::Table,
            "DROP" => Token::Drop,
            "PRIMARY" => Token::Primary,
            "KEY" => Token::Key,
            "INT" => Token::IntType,
            "TEXT" => Token::TextType,
            "BOOL" => Token::BoolType,
            "JOIN" => Token::Join,
            "ON" => Token::On,
            "GROUP" => Token::Group,
            "AND" => Token::And,
            "OR" => Token::Or,
            "NOT" => Token::Not,
            "NULL" => Token::Null,
            "TRUE" => Token::Bool(true),
            "FALSE" => Token::Bool(false),
            "COUNT" => Token::Count,
            "SUM" => Token::Sum,
            "MIN" => Token::Min,
            "MAX" => Token::Max,
            "AVG" => Token::Avg,
            "AS" => Token::As,
            "BEGIN" => Token::Begin,
            "COMMIT" => Token::Commit,
            "ROLLBACK" => Token::Rollback,
            _ => return None,
        };
        Some(kw)
    }
}

#[cfg(test)]
mod tests {
    use super::Token;

    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(Token::keyword("select"), Some(Token::Select));
        assert_eq!(Token::keyword("SeLeCt"), Some(Token::Select));
        assert_eq!(Token::keyword("TRUE"), Some(Token::Bool(true)));
        assert_eq!(Token::keyword("false"), Some(Token::Bool(false)));
        assert_eq!(Token::keyword("null"), Some(Token::Null));
    }

    #[test]
    fn int_keyword_is_the_type_not_the_literal() {
        assert_eq!(Token::keyword("int"), Some(Token::IntType));
        assert_eq!(Token::keyword("text"), Some(Token::TextType));
        assert_eq!(Token::keyword("bool"), Some(Token::BoolType));
    }

    #[test]
    fn non_keywords_are_identifiers() {
        assert_eq!(Token::keyword("users"), None);
        assert_eq!(Token::keyword("selected"), None);
    }
}
