use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    UnexpectedToken {
        pos: usize,
        expected: String,
        got: String,
    },
    UnexpectedEof(String),
    InvalidSyntax(String),
    InvalidQuery(String),
    LexerError(usize),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::UnexpectedToken { pos, expected, got } => {
                write!(f, "at position {pos}: expected {expected}, got {got}")
            }
            ParseError::UnexpectedEof(msg) => write!(f, "unexpected end of input: {msg}"),
            ParseError::InvalidSyntax(msg) => write!(f, "invalid syntax: {msg}"),
            ParseError::InvalidQuery(msg) => write!(f, "invalid query: {msg}"),
            ParseError::LexerError(pos) => write!(f, "lexer error at position {pos}"),
        }
    }
}

impl std::error::Error for ParseError {}
