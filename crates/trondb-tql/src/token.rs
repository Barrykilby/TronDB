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

    #[token("EDGE", priority = 10, ignore(ascii_case))]
    Edge,

    #[token("TRAVERSE", priority = 10, ignore(ascii_case))]
    Traverse,

    #[token("DEPTH", priority = 10, ignore(ascii_case))]
    Depth,

    #[token("TO", priority = 10, ignore(ascii_case))]
    To,

    #[token("DELETE", priority = 10, ignore(ascii_case))]
    Delete,

    #[token("REPRESENTATION", priority = 10, ignore(ascii_case))]
    Representation,

    #[token("MODEL", priority = 10, ignore(ascii_case))]
    Model,

    #[token("METRIC", priority = 10, ignore(ascii_case))]
    TokenMetric,

    #[token("SPARSE", priority = 10, ignore(ascii_case))]
    Sparse,

    #[token("FIELD", priority = 10, ignore(ascii_case))]
    Field,

    #[token("DATETIME", priority = 10, ignore(ascii_case))]
    DateTime,

    #[token("TEXT", priority = 10, ignore(ascii_case))]
    Text,

    #[token("BOOL", priority = 10, ignore(ascii_case))]
    TokenBool,

    #[token("INT", priority = 10, ignore(ascii_case))]
    TokenInt,

    #[token("FLOAT", priority = 10, ignore(ascii_case))]
    TokenFloat,

    #[token("ENTITY_REF", priority = 10, ignore(ascii_case))]
    EntityRef,

    #[token("INDEX", priority = 10, ignore(ascii_case))]
    Index,

    #[token("ON", priority = 10, ignore(ascii_case))]
    On,

    #[token("INNER_PRODUCT", priority = 10, ignore(ascii_case))]
    InnerProduct,

    #[token("COSINE", priority = 10, ignore(ascii_case))]
    Cosine,

    #[token("COLLOCATE", priority = 10, ignore(ascii_case))]
    Collocate,

    #[token("AFFINITY", priority = 10, ignore(ascii_case))]
    Affinity,

    #[token("GROUP", priority = 10, ignore(ascii_case))]
    Group,

    #[token("ALTER", priority = 10, ignore(ascii_case))]
    Alter,

    #[token("DROP", priority = 10, ignore(ascii_case))]
    Drop,

    #[token("ENTITY", priority = 10, ignore(ascii_case))]
    Entity,

    #[token("DEMOTE", priority = 10, ignore(ascii_case))]
    Demote,

    #[token("PROMOTE", priority = 10, ignore(ascii_case))]
    Promote,

    #[token("TIERS", priority = 10, ignore(ascii_case))]
    Tiers,

    #[token("WARM", priority = 10, ignore(ascii_case))]
    Warm,

    #[token("ARCHIVE", priority = 10, ignore(ascii_case))]
    Archive,

    #[token("DECAY", priority = 10, ignore(ascii_case))]
    Decay,

    #[token("RATE", priority = 10, ignore(ascii_case))]
    Rate,

    #[token("FLOOR", priority = 10, ignore(ascii_case))]
    TokenFloor,

    #[token("PRUNE", priority = 10, ignore(ascii_case))]
    Prune,

    #[token("EXPONENTIAL", priority = 10, ignore(ascii_case))]
    Exponential,

    #[token("LINEAR", priority = 10, ignore(ascii_case))]
    Linear,

    #[token("STEP", priority = 10, ignore(ascii_case))]
    Step,

    #[token("UPDATE", priority = 10, ignore(ascii_case))]
    Update,

    #[token("SET", priority = 10, ignore(ascii_case))]
    Set,

    #[token("IN", priority = 10, ignore(ascii_case))]
    In,

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

    #[token(":")]
    Colon,
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

    #[test]
    fn lex_edge_statement() {
        let tokens = lex("CREATE EDGE knows FROM people TO people;");
        assert_eq!(
            tokens,
            vec![
                Token::Create,
                Token::Edge,
                Token::Ident("knows".to_string()),
                Token::From,
                Token::Ident("people".to_string()),
                Token::To,
                Token::Ident("people".to_string()),
                Token::Semicolon,
            ]
        );
    }

    #[test]
    fn lex_representation_tokens() {
        let tokens = lex("REPRESENTATION identity MODEL 'jina-v4' DIMENSIONS 1024 METRIC COSINE");
        assert_eq!(tokens[0], Token::Representation);
        assert_eq!(tokens[1], Token::Ident("identity".to_string()));
        assert_eq!(tokens[2], Token::Model);
        assert_eq!(tokens[3], Token::StringLit("jina-v4".to_string()));
        assert_eq!(tokens[4], Token::Dimensions);
        assert_eq!(tokens[5], Token::IntLit(1024));
        assert_eq!(tokens[6], Token::TokenMetric);
        assert_eq!(tokens[7], Token::Cosine);
    }

    #[test]
    fn lex_sparse_vector_literal() {
        let tokens = lex("[1:0.8, 42:0.5]");
        assert_eq!(tokens, vec![
            Token::LBracket,
            Token::IntLit(1),
            Token::Colon,
            Token::FloatLit(0.8),
            Token::Comma,
            Token::IntLit(42),
            Token::Colon,
            Token::FloatLit(0.5),
            Token::RBracket,
        ]);
    }

    #[test]
    fn lex_field_and_index_tokens() {
        let tokens = lex("FIELD status TEXT INDEX idx_status ON");
        assert_eq!(tokens[0], Token::Field);
        assert_eq!(tokens[1], Token::Ident("status".to_string()));
        assert_eq!(tokens[2], Token::Text);
        assert_eq!(tokens[3], Token::Index);
        assert_eq!(tokens[4], Token::Ident("idx_status".to_string()));
        assert_eq!(tokens[5], Token::On);
    }

    #[test]
    fn lex_sparse_keyword() {
        let tokens = lex("SPARSE INNER_PRODUCT ENTITY_REF BOOL INT FLOAT DATETIME");
        assert_eq!(tokens[0], Token::Sparse);
        assert_eq!(tokens[1], Token::InnerProduct);
        assert_eq!(tokens[2], Token::EntityRef);
        assert_eq!(tokens[3], Token::TokenBool);
        assert_eq!(tokens[4], Token::TokenInt);
        assert_eq!(tokens[5], Token::TokenFloat);
        assert_eq!(tokens[6], Token::DateTime);
    }
}
