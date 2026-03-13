use logos::Logos;

fn strip_quotes_and_unescape(lex: &mut logos::Lexer<Token>) -> String {
    let slice = lex.slice();
    let inner = &slice[1..slice.len() - 1];
    inner.replace("\\'", "'").replace("\\\\", "\\")
}

#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(skip r"[ \t\r\n\f]+")]
#[logos(skip r"--[^\n]*")]
#[logos(skip r"/\*[^+]([^*]|\*[^/])*\*/")]
pub enum Token {
    // Query hints: /*+ HINT_NAME [VALUE] */
    #[regex(r"/\*\+[^*]*\*/", priority = 5, callback = |lex| {
        let s = lex.slice();
        s[3..s.len()-2].trim().to_string()
    })]
    Hint(String),

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

    #[token("WITHIN", priority = 10, ignore(ascii_case))]
    Within,

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

    #[token("FIELDS", priority = 10, ignore(ascii_case))]
    Fields,

    #[token("USING", priority = 10, ignore(ascii_case))]
    Using,

    #[token("MODEL_PATH", priority = 10, ignore(ascii_case))]
    ModelPath,

    #[token("DEVICE", priority = 10, ignore(ascii_case))]
    TokenDevice,

    #[token("VECTORISER", priority = 10, ignore(ascii_case))]
    TokenVectoriser,

    #[token("ENDPOINT", priority = 10, ignore(ascii_case))]
    TokenEndpoint,

    #[token("AUTH", priority = 10, ignore(ascii_case))]
    TokenAuth,

    #[token("INFER", priority = 10, ignore(ascii_case))]
    Infer,

    #[token("CONFIRM", priority = 10, ignore(ascii_case))]
    Confirm,

    #[token("RETURNING", priority = 10, ignore(ascii_case))]
    Returning,

    #[token("TOP", priority = 10, ignore(ascii_case))]
    Top,

    #[token("HISTORY", priority = 10, ignore(ascii_case))]
    History,

    #[token("AUTO", priority = 10, ignore(ascii_case))]
    Auto,

    #[token("EDGES", priority = 10, ignore(ascii_case))]
    Edges,

    #[token("VIA", priority = 10, ignore(ascii_case))]
    Via,

    #[token("TYPE", priority = 10, ignore(ascii_case))]
    Type,

    #[token("ALL", priority = 10, ignore(ascii_case))]
    All,

    #[token("NOT", priority = 10, ignore(ascii_case))]
    Not,

    #[token("IS", priority = 10, ignore(ascii_case))]
    Is,

    #[token("LIKE", priority = 10, ignore(ascii_case))]
    Like,

    #[token("ORDER", priority = 10, ignore(ascii_case))]
    Order,

    #[token("BY", priority = 10, ignore(ascii_case))]
    By,

    #[token("ASC", priority = 10, ignore(ascii_case))]
    Asc,

    #[token("DESC", priority = 10, ignore(ascii_case))]
    Desc,

    #[token("JOIN", priority = 10, ignore(ascii_case))]
    Join,

    #[token("INNER", priority = 10, ignore(ascii_case))]
    Inner,

    #[token("LEFT", priority = 10, ignore(ascii_case))]
    Left,

    #[token("RIGHT", priority = 10, ignore(ascii_case))]
    Right,

    #[token("FULL", priority = 10, ignore(ascii_case))]
    Full,

    #[token("AS", priority = 10, ignore(ascii_case))]
    As,

    #[token("OF", priority = 10, ignore(ascii_case))]
    Of,

    #[token("TRANSACTION", priority = 10, ignore(ascii_case))]
    Transaction,

    #[token("VALID", priority = 10, ignore(ascii_case))]
    Valid,

    #[token("DURING", priority = 10, ignore(ascii_case))]
    During,

    #[token("MATCH", priority = 10, ignore(ascii_case))]
    Match,

    #[token("CHECKPOINT", priority = 10, ignore(ascii_case))]
    Checkpoint,

    #[token("BACKUP", priority = 10, ignore(ascii_case))]
    Backup,

    #[token("RESTORE", priority = 10, ignore(ascii_case))]
    Restore,

    #[token("RENAME", priority = 10, ignore(ascii_case))]
    Rename,

    #[token("IMPORT", priority = 10, ignore(ascii_case))]
    Import,

    // Identifiers
    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*", priority = 1, callback = |lex| lex.slice().to_string())]
    Ident(String),

    // String literals (strip quotes, support backslash escaping)
    #[regex(r"'([^'\\]|\\.)*'", callback = strip_quotes_and_unescape)]
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

    #[token("->")]
    Arrow,

    #[token("..")]
    DotDot,

    #[token("-")]
    Dash,

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

    #[token(".")]
    Dot,
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

    #[test]
    fn lex_fields_keyword() {
        let tokens = lex("FIELDS (name, description)");
        assert_eq!(tokens[0], Token::Fields);
        assert_eq!(tokens[1], Token::LParen);
        assert_eq!(tokens[2], Token::Ident("name".to_string()));
        assert_eq!(tokens[3], Token::Comma);
        assert_eq!(tokens[4], Token::Ident("description".to_string()));
        assert_eq!(tokens[5], Token::RParen);
    }

    #[test]
    fn lex_using_keyword() {
        assert_eq!(lex("USING"), vec![Token::Using]);
        assert_eq!(lex("using"), vec![Token::Using]);
    }

    #[test]
    fn lex_vectoriser_config_tokens() {
        let tokens = lex("MODEL_PATH '/models/bge.onnx' DEVICE 'cpu'");
        assert_eq!(tokens[0], Token::ModelPath);
        assert_eq!(tokens[1], Token::StringLit("/models/bge.onnx".to_string()));
        assert_eq!(tokens[2], Token::TokenDevice);
        assert_eq!(tokens[3], Token::StringLit("cpu".to_string()));
    }

    #[test]
    fn lex_infer_keywords() {
        let tokens = lex("INFER EDGES FROM 'x' RETURNING TOP 5");
        assert_eq!(
            tokens,
            vec![
                Token::Infer,
                Token::Edges,
                Token::From,
                Token::StringLit("x".to_string()),
                Token::Returning,
                Token::Top,
                Token::IntLit(5),
            ]
        );
    }

    #[test]
    fn lex_confirm_keywords() {
        let tokens = lex("CONFIRM EDGE FROM 'x' TO 'y' TYPE test CONFIDENCE 0.9");
        assert_eq!(
            tokens,
            vec![
                Token::Confirm,
                Token::Edge,
                Token::From,
                Token::StringLit("x".to_string()),
                Token::To,
                Token::StringLit("y".to_string()),
                Token::Type,
                Token::Ident("test".to_string()),
                Token::Confidence,
                Token::FloatLit(0.9),
            ]
        );
    }

    #[test]
    fn lex_explain_history_keywords() {
        let tokens = lex("EXPLAIN HISTORY 'entity1' LIMIT 10");
        assert_eq!(
            tokens,
            vec![
                Token::Explain,
                Token::History,
                Token::StringLit("entity1".to_string()),
                Token::Limit,
                Token::IntLit(10),
            ]
        );
    }

    #[test]
    fn lex_infer_auto_keywords() {
        let tokens = lex("INFER AUTO ALL VIA");
        assert_eq!(
            tokens,
            vec![
                Token::Infer,
                Token::Auto,
                Token::All,
                Token::Via,
            ]
        );
    }

    #[test]
    fn lex_not_keyword() {
        let tokens = lex("NOT");
        assert_eq!(tokens, vec![Token::Not]);
    }

    #[test]
    fn lex_is_keyword() {
        let tokens = lex("IS");
        assert_eq!(tokens, vec![Token::Is]);
    }

    #[test]
    fn lex_like_keyword() {
        let tokens = lex("LIKE");
        assert_eq!(tokens, vec![Token::Like]);
    }

    #[test]
    fn lex_advanced_where_full() {
        let tokens = lex("WHERE NOT name IS NULL AND category IN ('a', 'b') OR name LIKE 'Jazz%'");
        assert_eq!(tokens[0], Token::Where);
        assert_eq!(tokens[1], Token::Not);
        assert_eq!(tokens[2], Token::Ident("name".into()));
        assert_eq!(tokens[3], Token::Is);
        assert_eq!(tokens[4], Token::Null);
        assert_eq!(tokens[5], Token::And);
        assert_eq!(tokens[6], Token::Ident("category".into()));
        assert_eq!(tokens[7], Token::In);
        assert_eq!(tokens[8], Token::LParen);
        assert_eq!(tokens[9], Token::StringLit("a".into()));
        assert_eq!(tokens[10], Token::Comma);
        assert_eq!(tokens[11], Token::StringLit("b".into()));
        assert_eq!(tokens[12], Token::RParen);
        assert_eq!(tokens[13], Token::Or);
        assert_eq!(tokens[14], Token::Ident("name".into()));
        assert_eq!(tokens[15], Token::Like);
        assert_eq!(tokens[16], Token::StringLit("Jazz%".into()));
    }

    #[test]
    fn lex_inference_keywords_case_insensitive() {
        assert_eq!(lex("infer"), vec![Token::Infer]);
        assert_eq!(lex("CONFIRM"), vec![Token::Confirm]);
        assert_eq!(lex("Returning"), vec![Token::Returning]);
        assert_eq!(lex("top"), vec![Token::Top]);
        assert_eq!(lex("history"), vec![Token::History]);
        assert_eq!(lex("auto"), vec![Token::Auto]);
        assert_eq!(lex("edges"), vec![Token::Edges]);
        assert_eq!(lex("via"), vec![Token::Via]);
        assert_eq!(lex("TYPE"), vec![Token::Type]);
        assert_eq!(lex("All"), vec![Token::All]);
    }

    #[test]
    fn lex_order_by_keywords() {
        let tokens = lex("ORDER BY name ASC LIMIT 10");
        assert_eq!(tokens[0], Token::Order);
        assert_eq!(tokens[1], Token::By);
        assert_eq!(tokens[2], Token::Ident("name".into()));
        assert_eq!(tokens[3], Token::Asc);
        assert_eq!(tokens[4], Token::Limit);
        assert_eq!(tokens[5], Token::IntLit(10));
    }

    #[test]
    fn lex_desc_keyword() {
        let tokens = lex("DESC");
        assert_eq!(tokens, vec![Token::Desc]);
    }

    #[test]
    fn lex_hint_token() {
        let tokens = lex("FETCH /*+ NO_PROMOTE */ * FROM venues;");
        assert_eq!(tokens[0], Token::Fetch);
        assert_eq!(tokens[1], Token::Hint("NO_PROMOTE".into()));
        assert_eq!(tokens[2], Token::Star);
    }

    #[test]
    fn lex_hint_with_value() {
        let tokens = lex("/*+ MAX_ACU(200) */");
        assert_eq!(tokens[0], Token::Hint("MAX_ACU(200)".into()));
    }

    #[test]
    fn lex_block_comment_skipped() {
        let tokens = lex("FETCH /* this is a comment */ * FROM venues;");
        assert_eq!(tokens[0], Token::Fetch);
        assert_eq!(tokens[1], Token::Star);
        assert_eq!(tokens[2], Token::From);
    }

    #[test]
    fn lex_join_keywords() {
        assert_eq!(lex("JOIN"), vec![Token::Join]);
        assert_eq!(lex("join"), vec![Token::Join]);
        assert_eq!(lex("INNER"), vec![Token::Inner]);
        assert_eq!(lex("LEFT"), vec![Token::Left]);
        assert_eq!(lex("RIGHT"), vec![Token::Right]);
        assert_eq!(lex("FULL"), vec![Token::Full]);
        assert_eq!(lex("AS"), vec![Token::As]);
    }

    #[test]
    fn lex_dot_token() {
        let tokens = lex("e.name");
        assert_eq!(tokens, vec![
            Token::Ident("e".into()),
            Token::Dot,
            Token::Ident("name".into()),
        ]);
    }

    #[test]
    fn lex_structural_join() {
        let tokens = lex("FETCH e.name, v.address FROM entities AS e INNER JOIN venues AS v ON e.venue_id = v.id WHERE e.type = 'event';");
        assert_eq!(tokens[0], Token::Fetch);
        assert_eq!(tokens[1], Token::Ident("e".into()));
        assert_eq!(tokens[2], Token::Dot);
        assert_eq!(tokens[3], Token::Ident("name".into()));
        assert_eq!(tokens[4], Token::Comma);
        assert_eq!(tokens[5], Token::Ident("v".into()));
        assert_eq!(tokens[6], Token::Dot);
        assert_eq!(tokens[7], Token::Ident("address".into()));
        assert_eq!(tokens[8], Token::From);
        assert_eq!(tokens[9], Token::Ident("entities".into()));
        assert_eq!(tokens[10], Token::As);
        assert_eq!(tokens[11], Token::Ident("e".into()));
        assert_eq!(tokens[12], Token::Inner);
        assert_eq!(tokens[13], Token::Join);
    }

    #[test]
    fn lex_probabilistic_join() {
        let tokens = lex("CONFIDENCE > 0.75");
        assert_eq!(tokens[0], Token::Confidence);
        assert_eq!(tokens[1], Token::Gt);
        assert_eq!(tokens[2], Token::FloatLit(0.75));
    }

    #[test]
    fn lex_match_keyword() {
        assert_eq!(lex("MATCH"), vec![Token::Match]);
        assert_eq!(lex("match"), vec![Token::Match]);
    }

    #[test]
    fn lex_arrow_tokens() {
        let tokens = lex("-[e:RELATED_TO]->");
        assert_eq!(tokens[0], Token::Dash);
        assert_eq!(tokens[1], Token::LBracket);
        assert_eq!(tokens[2], Token::Ident("e".into()));
        assert_eq!(tokens[3], Token::Colon);
        assert_eq!(tokens[4], Token::Ident("RELATED_TO".into()));
        assert_eq!(tokens[5], Token::RBracket);
        assert_eq!(tokens[6], Token::Arrow);
    }

    #[test]
    fn lex_dotdot_token() {
        let tokens = lex("1..3");
        assert_eq!(tokens[0], Token::IntLit(1));
        assert_eq!(tokens[1], Token::DotDot);
        assert_eq!(tokens[2], Token::IntLit(3));
    }

    #[test]
    fn lex_of_keyword() {
        assert_eq!(lex("OF"), vec![Token::Of]);
        assert_eq!(lex("of"), vec![Token::Of]);
    }

    #[test]
    fn lex_transaction_keyword() {
        assert_eq!(lex("TRANSACTION"), vec![Token::Transaction]);
        assert_eq!(lex("transaction"), vec![Token::Transaction]);
    }

    #[test]
    fn lex_valid_keyword() {
        assert_eq!(lex("VALID"), vec![Token::Valid]);
        assert_eq!(lex("valid"), vec![Token::Valid]);
    }

    #[test]
    fn lex_during_keyword() {
        assert_eq!(lex("DURING"), vec![Token::During]);
        assert_eq!(lex("during"), vec![Token::During]);
    }

    #[test]
    fn lex_as_of_sequence() {
        let tokens = lex("AS OF '2025-01-01T00:00:00Z'");
        assert_eq!(tokens, vec![
            Token::As,
            Token::Of,
            Token::StringLit("2025-01-01T00:00:00Z".into()),
        ]);
    }

    #[test]
    fn lex_as_of_transaction_sequence() {
        let tokens = lex("AS OF TRANSACTION 42891");
        assert_eq!(tokens, vec![
            Token::As,
            Token::Of,
            Token::Transaction,
            Token::IntLit(42891),
        ]);
    }

    #[test]
    fn lex_valid_during_sequence() {
        let tokens = lex("VALID DURING '2025-01-01'..'2025-06-30'");
        assert_eq!(tokens, vec![
            Token::Valid,
            Token::During,
            Token::StringLit("2025-01-01".into()),
            Token::DotDot,
            Token::StringLit("2025-06-30".into()),
        ]);
    }

    #[test]
    fn lex_valid_from_to_sequence() {
        let tokens = lex("VALID FROM '2025-01-01T00:00:00Z' TO '2025-06-30T00:00:00Z'");
        assert_eq!(tokens, vec![
            Token::Valid,
            Token::From,
            Token::StringLit("2025-01-01T00:00:00Z".into()),
            Token::To,
            Token::StringLit("2025-06-30T00:00:00Z".into()),
        ]);
    }

    #[test]
    fn lex_traverse_match_full() {
        let tokens = lex("TRAVERSE FROM 'ent_abc123' MATCH (a)-[e:RELATED_TO]->(b) DEPTH 1..3 CONFIDENCE > 0.70;");
        assert_eq!(tokens[0], Token::Traverse);
        assert_eq!(tokens[1], Token::From);
        assert_eq!(tokens[2], Token::StringLit("ent_abc123".into()));
        assert_eq!(tokens[3], Token::Match);
        assert_eq!(tokens[4], Token::LParen);
        assert_eq!(tokens[5], Token::Ident("a".into()));
        assert_eq!(tokens[6], Token::RParen);
        assert_eq!(tokens[7], Token::Dash);
        assert_eq!(tokens[8], Token::LBracket);
        assert_eq!(tokens[9], Token::Ident("e".into()));
        assert_eq!(tokens[10], Token::Colon);
        assert_eq!(tokens[11], Token::Ident("RELATED_TO".into()));
        assert_eq!(tokens[12], Token::RBracket);
        assert_eq!(tokens[13], Token::Arrow);
        assert_eq!(tokens[14], Token::LParen);
        assert_eq!(tokens[15], Token::Ident("b".into()));
        assert_eq!(tokens[16], Token::RParen);
        assert_eq!(tokens[17], Token::Depth);
        assert_eq!(tokens[18], Token::IntLit(1));
        assert_eq!(tokens[19], Token::DotDot);
        assert_eq!(tokens[20], Token::IntLit(3));
        assert_eq!(tokens[21], Token::Confidence);
        assert_eq!(tokens[22], Token::Gt);
        assert_eq!(tokens[23], Token::FloatLit(0.70));
        assert_eq!(tokens[24], Token::Semicolon);
    }

    #[test]
    fn lex_checkpoint_keyword() {
        assert_eq!(lex("CHECKPOINT"), vec![Token::Checkpoint]);
        assert_eq!(lex("checkpoint"), vec![Token::Checkpoint]);
    }

    #[test]
    fn lex_string_with_escaped_quote() {
        assert_eq!(lex("'O\\'Brien'"), vec![Token::StringLit("O'Brien".into())]);
    }

    #[test]
    fn lex_string_with_escaped_backslash() {
        assert_eq!(lex("'path\\\\to'"), vec![Token::StringLit("path\\to".into())]);
    }
}
