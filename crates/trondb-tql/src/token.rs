use logos::Logos;

fn strip_quotes(lex: &mut logos::Lexer<Token>) -> String {
    let slice = lex.slice();
    slice[1..slice.len() - 1].to_string()
}

#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(skip r"[ \t\r\n\f]+")]
#[logos(skip r"--[^\n]*")]
pub enum Token {
    // Keywords — priority 10 so they beat Ident
    #[token("FETCH", priority = 10, ignore(ascii_case))]
    Fetch,

    #[token("SEARCH", priority = 10, ignore(ascii_case))]
    Search,

    #[token("FROM", priority = 10, ignore(ascii_case))]
    From,

    #[token("WHERE", priority = 10, ignore(ascii_case))]
    Where,

    #[token("AND", priority = 10, ignore(ascii_case))]
    And,

    #[token("OR", priority = 10, ignore(ascii_case))]
    Or,

    #[token("LIMIT", priority = 10, ignore(ascii_case))]
    Limit,

    #[token("CREATE", priority = 10, ignore(ascii_case))]
    Create,

    #[token("COLLECTION", priority = 10, ignore(ascii_case))]
    Collection,

    #[token("WITH", priority = 10, ignore(ascii_case))]
    With,

    #[token("DIMENSIONS", priority = 10, ignore(ascii_case))]
    Dimensions,

    #[token("INSERT", priority = 10, ignore(ascii_case))]
    Insert,

    #[token("INTO", priority = 10, ignore(ascii_case))]
    Into,

    #[token("VALUES", priority = 10, ignore(ascii_case))]
    Values,

    #[token("VECTOR", priority = 10, ignore(ascii_case))]
    Vector,

    #[token("NEAR", priority = 10, ignore(ascii_case))]
    Near,

    #[token("CONFIDENCE", priority = 10, ignore(ascii_case))]
    Confidence,

    #[token("EXPLAIN", priority = 10, ignore(ascii_case))]
    Explain,

    #[token("NULL", priority = 10, ignore(ascii_case))]
    Null,

    #[token("TRUE", priority = 10, ignore(ascii_case))]
    True,

    #[token("FALSE", priority = 10, ignore(ascii_case))]
    False,

    // Identifiers
    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*", priority = 1, callback = |lex| lex.slice().to_string())]
    Ident(String),

    // String literals (strip quotes)
    #[regex(r"'[^']*'", callback = strip_quotes)]
    StringLit(String),

    // Float literals (must be before int to match longer pattern)
    #[regex(r"-?[0-9]+\.[0-9]+", priority = 3, callback = |lex| lex.slice().parse::<f64>().unwrap())]
    FloatLit(f64),

    // Int literals
    #[regex(r"-?[0-9]+", priority = 2, callback = |lex| lex.slice().parse::<i64>().unwrap())]
    IntLit(i64),

    // Symbols (multi-char before single-char)
    #[token(">=")]
    Gte,

    #[token("<=")]
    Lte,

    #[token("!=")]
    Neq,

    #[token("*")]
    Star,

    #[token(",")]
    Comma,

    #[token(";")]
    Semicolon,

    #[token("(")]
    LParen,

    #[token(")")]
    RParen,

    #[token("[")]
    LBracket,

    #[token("]")]
    RBracket,

    #[token("=")]
    Eq,

    #[token(">")]
    Gt,

    #[token("<")]
    Lt,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(input: &str) -> Vec<Token> {
        Token::lexer(input)
            .map(|r| r.expect("lexer error"))
            .collect()
    }

    #[test]
    fn lex_fetch_statement() {
        let tokens = lex("FETCH * FROM venues WHERE id = 'v1';");
        assert_eq!(
            tokens,
            vec![
                Token::Fetch,
                Token::Star,
                Token::From,
                Token::Ident("venues".to_string()),
                Token::Where,
                Token::Ident("id".to_string()),
                Token::Eq,
                Token::StringLit("v1".to_string()),
                Token::Semicolon,
            ]
        );
    }

    #[test]
    fn lex_create_collection() {
        let tokens = lex("CREATE COLLECTION venues WITH DIMENSIONS 384;");
        assert_eq!(
            tokens,
            vec![
                Token::Create,
                Token::Collection,
                Token::Ident("venues".to_string()),
                Token::With,
                Token::Dimensions,
                Token::IntLit(384),
                Token::Semicolon,
            ]
        );
    }

    #[test]
    fn lex_search_statement() {
        let tokens = lex("SEARCH venues NEAR VECTOR [1.0, 2.0] CONFIDENCE > 0.8 LIMIT 5;");
        assert_eq!(
            tokens,
            vec![
                Token::Search,
                Token::Ident("venues".to_string()),
                Token::Near,
                Token::Vector,
                Token::LBracket,
                Token::FloatLit(1.0),
                Token::Comma,
                Token::FloatLit(2.0),
                Token::RBracket,
                Token::Confidence,
                Token::Gt,
                Token::FloatLit(0.8),
                Token::Limit,
                Token::IntLit(5),
                Token::Semicolon,
            ]
        );
    }

    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(lex("FETCH"), vec![Token::Fetch]);
        assert_eq!(lex("fetch"), vec![Token::Fetch]);
        assert_eq!(lex("Fetch"), vec![Token::Fetch]);
    }
}
