use logos::Logos;

use crate::ast::*;
use crate::error::ParseError;
use crate::token::Token;

struct Parser {
    tokens: Vec<(Token, std::ops::Range<usize>)>,
    pos: usize,
}

impl Parser {
    fn new(input: &str) -> Result<Self, ParseError> {
        let lexer = Token::lexer(input);
        let mut tokens = Vec::new();
        for (result, span) in lexer.spanned() {
            match result {
                Ok(tok) => tokens.push((tok, span)),
                Err(()) => return Err(ParseError::LexerError(span.start)),
            }
        }
        Ok(Parser { tokens, pos: 0 })
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|(t, _)| t)
    }

    fn advance(&mut self) -> Option<(Token, usize)> {
        if self.pos < self.tokens.len() {
            let (tok, span) = self.tokens[self.pos].clone();
            self.pos += 1;
            Some((tok, span.start))
        } else {
            None
        }
    }

    fn expect(&mut self, expected: &Token) -> Result<usize, ParseError> {
        match self.advance() {
            Some((tok, pos)) if &tok == expected => Ok(pos),
            Some((tok, pos)) => Err(ParseError::UnexpectedToken {
                pos,
                expected: format!("{expected:?}"),
                got: format!("{tok:?}"),
            }),
            None => Err(ParseError::UnexpectedEof(format!("expected {expected:?}"))),
        }
    }

    fn expect_ident(&mut self) -> Result<String, ParseError> {
        match self.advance() {
            Some((Token::Ident(s), _)) => Ok(s),
            Some((tok, pos)) => Err(ParseError::UnexpectedToken {
                pos,
                expected: "identifier".to_string(),
                got: format!("{tok:?}"),
            }),
            None => Err(ParseError::UnexpectedEof("expected identifier".to_string())),
        }
    }

    fn expect_int(&mut self) -> Result<i64, ParseError> {
        match self.advance() {
            Some((Token::IntLit(n), _)) => Ok(n),
            Some((tok, pos)) => Err(ParseError::UnexpectedToken {
                pos,
                expected: "integer".to_string(),
                got: format!("{tok:?}"),
            }),
            None => Err(ParseError::UnexpectedEof("expected integer".to_string())),
        }
    }

    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        match self.peek() {
            Some(Token::Create) => self.parse_create(),
            Some(Token::Insert) => self.parse_insert(),
            Some(Token::Fetch) => self.parse_fetch(),
            Some(Token::Search) => self.parse_search(),
            Some(Token::Explain) => self.parse_explain(),
            Some(tok) => {
                let tok_str = format!("{tok:?}");
                let pos = self.tokens[self.pos].1.start;
                Err(ParseError::UnexpectedToken {
                    pos,
                    expected: "statement keyword".to_string(),
                    got: tok_str,
                })
            }
            None => Err(ParseError::UnexpectedEof(
                "expected statement".to_string(),
            )),
        }
    }

    fn parse_create(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // CREATE
        self.expect(&Token::Collection)?;
        let name = self.expect_ident()?;
        self.expect(&Token::With)?;
        self.expect(&Token::Dimensions)?;
        let dims = self.expect_int()?;
        self.expect(&Token::Semicolon)?;
        Ok(Statement::CreateCollection(CreateCollectionStmt {
            name,
            dimensions: dims as usize,
        }))
    }

    fn parse_insert(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // INSERT
        self.expect(&Token::Into)?;
        let collection = self.expect_ident()?;
        self.expect(&Token::LParen)?;

        // Parse field list
        let mut fields = vec![self.expect_ident()?];
        while self.peek() == Some(&Token::Comma) {
            self.advance();
            fields.push(self.expect_ident()?);
        }
        self.expect(&Token::RParen)?;

        self.expect(&Token::Values)?;
        self.expect(&Token::LParen)?;

        // Parse values
        let mut values = vec![self.parse_literal()?];
        while self.peek() == Some(&Token::Comma) {
            self.advance();
            values.push(self.parse_literal()?);
        }
        self.expect(&Token::RParen)?;

        // Optional VECTOR [...]
        let vector = if self.peek() == Some(&Token::Vector) {
            self.advance();
            Some(self.parse_float_list()?)
        } else {
            None
        };

        self.expect(&Token::Semicolon)?;
        Ok(Statement::Insert(InsertStmt {
            collection,
            fields,
            values,
            vector,
        }))
    }

    fn parse_fetch(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // FETCH
        let fields = self.parse_field_list()?;
        self.expect(&Token::From)?;
        let collection = self.expect_ident()?;

        let filter = if self.peek() == Some(&Token::Where) {
            self.advance();
            Some(self.parse_where_clause()?)
        } else {
            None
        };

        let limit = if self.peek() == Some(&Token::Limit) {
            self.advance();
            Some(self.expect_int()? as usize)
        } else {
            None
        };

        self.expect(&Token::Semicolon)?;
        Ok(Statement::Fetch(FetchStmt {
            collection,
            fields,
            filter,
            limit,
        }))
    }

    fn parse_search(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // SEARCH
        let collection = self.expect_ident()?;

        // Optional field list before NEAR
        let fields = if self.peek() == Some(&Token::Near) {
            FieldList::All
        } else {
            // Not specified in grammar — default to All
            FieldList::All
        };

        self.expect(&Token::Near)?;
        self.expect(&Token::Vector)?;
        let near = self.parse_float_list()?;

        let confidence = if self.peek() == Some(&Token::Confidence) {
            self.advance();
            self.expect(&Token::Gt)?;
            Some(self.parse_number_as_f64()?)
        } else {
            None
        };

        let limit = if self.peek() == Some(&Token::Limit) {
            self.advance();
            Some(self.expect_int()? as usize)
        } else {
            None
        };

        self.expect(&Token::Semicolon)?;
        Ok(Statement::Search(SearchStmt {
            collection,
            fields,
            near,
            confidence,
            limit,
        }))
    }

    fn parse_explain(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // EXPLAIN
        let inner = self.parse_statement()?;
        Ok(Statement::Explain(Box::new(inner)))
    }

    fn parse_field_list(&mut self) -> Result<FieldList, ParseError> {
        if self.peek() == Some(&Token::Star) {
            self.advance();
            Ok(FieldList::All)
        } else {
            let mut fields = vec![self.expect_ident()?];
            while self.peek() == Some(&Token::Comma) {
                self.advance();
                fields.push(self.expect_ident()?);
            }
            Ok(FieldList::Named(fields))
        }
    }

    fn parse_where_clause(&mut self) -> Result<WhereClause, ParseError> {
        let mut left = self.parse_where_comparison()?;
        loop {
            match self.peek() {
                Some(Token::And) => {
                    self.advance();
                    let right = self.parse_where_comparison()?;
                    left = WhereClause::And(Box::new(left), Box::new(right));
                }
                Some(Token::Or) => {
                    self.advance();
                    let right = self.parse_where_comparison()?;
                    left = WhereClause::Or(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_where_comparison(&mut self) -> Result<WhereClause, ParseError> {
        let field = self.expect_ident()?;
        match self.advance() {
            Some((Token::Eq, _)) => Ok(WhereClause::Eq(field, self.parse_literal()?)),
            Some((Token::Gt, _)) => Ok(WhereClause::Gt(field, self.parse_literal()?)),
            Some((Token::Lt, _)) => Ok(WhereClause::Lt(field, self.parse_literal()?)),
            Some((tok, pos)) => Err(ParseError::UnexpectedToken {
                pos,
                expected: "comparison operator (=, >, <)".to_string(),
                got: format!("{tok:?}"),
            }),
            None => Err(ParseError::UnexpectedEof(
                "expected comparison operator".to_string(),
            )),
        }
    }

    fn parse_literal(&mut self) -> Result<Literal, ParseError> {
        match self.advance() {
            Some((Token::StringLit(s), _)) => Ok(Literal::String(s)),
            Some((Token::IntLit(n), _)) => Ok(Literal::Int(n)),
            Some((Token::FloatLit(f), _)) => Ok(Literal::Float(f)),
            Some((Token::True, _)) => Ok(Literal::Bool(true)),
            Some((Token::False, _)) => Ok(Literal::Bool(false)),
            Some((Token::Null, _)) => Ok(Literal::Null),
            Some((tok, pos)) => Err(ParseError::UnexpectedToken {
                pos,
                expected: "literal value".to_string(),
                got: format!("{tok:?}"),
            }),
            None => Err(ParseError::UnexpectedEof("expected literal".to_string())),
        }
    }

    fn parse_number_as_f64(&mut self) -> Result<f64, ParseError> {
        match self.advance() {
            Some((Token::FloatLit(f), _)) => Ok(f),
            Some((Token::IntLit(n), _)) => Ok(n as f64),
            Some((tok, pos)) => Err(ParseError::UnexpectedToken {
                pos,
                expected: "number".to_string(),
                got: format!("{tok:?}"),
            }),
            None => Err(ParseError::UnexpectedEof("expected number".to_string())),
        }
    }

    fn parse_float_list(&mut self) -> Result<Vec<f64>, ParseError> {
        self.expect(&Token::LBracket)?;
        let mut values = vec![self.parse_number_as_f64()?];
        while self.peek() == Some(&Token::Comma) {
            self.advance();
            values.push(self.parse_number_as_f64()?);
        }
        self.expect(&Token::RBracket)?;
        Ok(values)
    }
}

/// Parse a TQL statement from the given input string.
pub fn parse(input: &str) -> Result<Statement, ParseError> {
    let mut parser = Parser::new(input)?;
    let stmt = parser.parse_statement()?;

    // Ensure no trailing tokens (except we already consumed semicolon)
    if parser.pos < parser.tokens.len() {
        let (tok, span) = &parser.tokens[parser.pos];
        return Err(ParseError::UnexpectedToken {
            pos: span.start,
            expected: "end of input".to_string(),
            got: format!("{tok:?}"),
        });
    }

    Ok(stmt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_create_collection() {
        let stmt = parse("CREATE COLLECTION venues WITH DIMENSIONS 384;").unwrap();
        assert_eq!(
            stmt,
            Statement::CreateCollection(CreateCollectionStmt {
                name: "venues".to_string(),
                dimensions: 384,
            })
        );
    }

    #[test]
    fn parse_insert_with_vector() {
        let stmt = parse(
            "INSERT INTO venues (id, name, city) VALUES ('v1', 'Apollo', 'London') VECTOR [1.0, 2.0, 3.0];",
        )
        .unwrap();
        assert_eq!(
            stmt,
            Statement::Insert(InsertStmt {
                collection: "venues".to_string(),
                fields: vec![
                    "id".to_string(),
                    "name".to_string(),
                    "city".to_string(),
                ],
                values: vec![
                    Literal::String("v1".to_string()),
                    Literal::String("Apollo".to_string()),
                    Literal::String("London".to_string()),
                ],
                vector: Some(vec![1.0, 2.0, 3.0]),
            })
        );
    }

    #[test]
    fn parse_insert_without_vector() {
        let stmt =
            parse("INSERT INTO venues (id, name) VALUES ('v1', 'Apollo');").unwrap();
        match &stmt {
            Statement::Insert(ins) => assert_eq!(ins.vector, None),
            _ => panic!("expected Insert statement"),
        }
    }

    #[test]
    fn parse_fetch_all() {
        let stmt = parse("FETCH * FROM venues;").unwrap();
        assert_eq!(
            stmt,
            Statement::Fetch(FetchStmt {
                collection: "venues".to_string(),
                fields: FieldList::All,
                filter: None,
                limit: None,
            })
        );
    }

    #[test]
    fn parse_fetch_with_where() {
        let stmt = parse("FETCH name, city FROM venues WHERE city = 'London';").unwrap();
        assert_eq!(
            stmt,
            Statement::Fetch(FetchStmt {
                collection: "venues".to_string(),
                fields: FieldList::Named(vec!["name".to_string(), "city".to_string()]),
                filter: Some(WhereClause::Eq(
                    "city".to_string(),
                    Literal::String("London".to_string()),
                )),
                limit: None,
            })
        );
    }

    #[test]
    fn parse_fetch_with_limit() {
        let stmt = parse("FETCH * FROM venues LIMIT 10;").unwrap();
        match &stmt {
            Statement::Fetch(f) => assert_eq!(f.limit, Some(10)),
            _ => panic!("expected Fetch statement"),
        }
    }

    #[test]
    fn parse_fetch_where_and() {
        let stmt =
            parse("FETCH * FROM venues WHERE city = 'London' AND capacity > 500;").unwrap();
        match &stmt {
            Statement::Fetch(f) => {
                assert_eq!(
                    f.filter,
                    Some(WhereClause::And(
                        Box::new(WhereClause::Eq(
                            "city".to_string(),
                            Literal::String("London".to_string()),
                        )),
                        Box::new(WhereClause::Gt(
                            "capacity".to_string(),
                            Literal::Int(500),
                        )),
                    ))
                );
            }
            _ => panic!("expected Fetch statement"),
        }
    }

    #[test]
    fn parse_error_missing_semicolon() {
        let result = parse("FETCH * FROM venues");
        assert!(result.is_err());
    }
}
