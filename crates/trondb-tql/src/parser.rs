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

    /// Like expect_ident but also accepts keywords that may be used as names
    /// (e.g. SPARSE as a representation name).
    fn expect_name(&mut self) -> Result<String, ParseError> {
        match self.advance() {
            Some((Token::Ident(s), _)) => Ok(s),
            // Allow SPARSE keyword to be used as an identifier in name positions
            Some((Token::Sparse, _)) => Ok("sparse".to_string()),
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

    fn expect_float_or_int(&mut self) -> Result<f64, ParseError> {
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

    fn expect_string_lit(&mut self) -> Result<String, ParseError> {
        match self.advance() {
            Some((Token::StringLit(s), _)) => Ok(s),
            Some((tok, pos)) => Err(ParseError::UnexpectedToken {
                pos,
                expected: "string literal".to_string(),
                got: format!("{tok:?}"),
            }),
            None => Err(ParseError::UnexpectedEof("expected string literal".to_string())),
        }
    }

    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        match self.peek() {
            Some(Token::Create) => self.parse_create(),
            Some(Token::Insert) => self.parse_insert(),
            Some(Token::Fetch) => self.parse_fetch(),
            Some(Token::Search) => self.parse_search(),
            Some(Token::Explain) => self.parse_explain(),
            Some(Token::Demote) => self.parse_demote(),
            Some(Token::Promote) => self.parse_promote(),
            Some(Token::Alter) => {
                self.advance(); // ALTER
                self.expect(&Token::Entity)?;
                let entity_id = self.expect_string_lit()?;
                self.expect(&Token::Drop)?;
                self.expect(&Token::Affinity)?;
                self.expect(&Token::Group)?;
                self.expect(&Token::Semicolon)?;
                Ok(Statement::AlterEntityDropAffinity(AlterEntityDropAffinityStmt { entity_id }))
            }
            Some(Token::Update) => self.parse_update(),
            Some(Token::Delete) => self.parse_delete(),
            Some(Token::Traverse) => self.parse_traverse(),
            Some(Token::Confirm) => self.parse_confirm_edge(),
            Some(Token::Infer) => self.parse_infer(),
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
        match self.peek() {
            Some(Token::Collection) => self.parse_create_collection(),
            Some(Token::Edge) => self.parse_create_edge(),
            Some(Token::Affinity) => {
                self.advance(); // AFFINITY
                self.expect(&Token::Group)?;
                let name = self.expect_string_lit()?;
                self.expect(&Token::Semicolon)?;
                Ok(Statement::CreateAffinityGroup(CreateAffinityGroupStmt { name }))
            }
            Some(tok) => {
                let tok_str = format!("{tok:?}");
                let pos = self.tokens[self.pos].1.start;
                Err(ParseError::UnexpectedToken {
                    pos,
                    expected: "COLLECTION, EDGE, or AFFINITY".to_string(),
                    got: tok_str,
                })
            }
            None => Err(ParseError::UnexpectedEof("expected COLLECTION, EDGE, or AFFINITY".to_string())),
        }
    }

    fn parse_create_collection(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // COLLECTION
        let name = self.expect_ident()?;
        self.expect(&Token::LParen)?;

        // Parse optional collection-level vectoriser config tokens BEFORE declarations
        let mut vec_model = None;
        let mut vec_model_path = None;
        let mut vec_device = None;
        let mut vec_type = None;
        let mut vec_endpoint = None;
        let mut vec_auth = None;

        loop {
            match self.peek() {
                Some(Token::Model) if vec_model.is_none() => {
                    self.advance();
                    vec_model = Some(self.expect_string_lit()?);
                }
                Some(Token::ModelPath) => {
                    self.advance();
                    vec_model_path = Some(self.expect_string_lit()?);
                }
                Some(Token::TokenDevice) => {
                    self.advance();
                    vec_device = Some(self.expect_string_lit()?);
                }
                Some(Token::TokenVectoriser) => {
                    self.advance();
                    vec_type = Some(self.expect_string_lit()?);
                }
                Some(Token::TokenEndpoint) => {
                    self.advance();
                    vec_endpoint = Some(self.expect_string_lit()?);
                }
                Some(Token::TokenAuth) => {
                    self.advance();
                    vec_auth = Some(self.expect_string_lit()?);
                }
                _ => break,
            }
        }

        let vectoriser_config = if vec_model.is_some() || vec_model_path.is_some()
            || vec_device.is_some() || vec_type.is_some()
            || vec_endpoint.is_some() || vec_auth.is_some()
        {
            Some(VectoriserConfigDecl {
                model: vec_model,
                model_path: vec_model_path,
                device: vec_device,
                vectoriser_type: vec_type,
                endpoint: vec_endpoint,
                auth: vec_auth,
            })
        } else {
            None
        };

        let mut representations = Vec::new();
        let mut fields = Vec::new();
        let mut indexes = Vec::new();

        loop {
            match self.peek() {
                Some(Token::RParen) => {
                    self.advance();
                    break;
                }
                Some(Token::Representation) => {
                    representations.push(self.parse_representation_decl()?);
                }
                Some(Token::Field) => {
                    fields.push(self.parse_field_decl()?);
                }
                Some(Token::Index) => {
                    indexes.push(self.parse_index_decl()?);
                }
                Some(tok) => {
                    let tok_str = format!("{tok:?}");
                    let pos = self.tokens[self.pos].1.start;
                    return Err(ParseError::UnexpectedToken {
                        pos,
                        expected: "REPRESENTATION, FIELD, INDEX, or )".into(),
                        got: tok_str,
                    });
                }
                None => return Err(ParseError::UnexpectedEof("expected ) or declaration".into())),
            }

            // Consume optional comma between declarations
            if self.peek() == Some(&Token::Comma) {
                self.advance();
            }
        }

        self.expect(&Token::Semicolon)?;
        Ok(Statement::CreateCollection(CreateCollectionStmt {
            name,
            representations,
            fields,
            indexes,
            vectoriser_config,
        }))
    }

    fn parse_representation_decl(&mut self) -> Result<RepresentationDecl, ParseError> {
        self.advance(); // REPRESENTATION
        let name = self.expect_name()?;
        let mut model = None;
        let mut dimensions = None;
        let mut metric = Metric::Cosine;
        let mut sparse = false;
        let mut fields = Vec::new();

        // Parse optional attributes in any order
        loop {
            match self.peek() {
                Some(Token::Model) => {
                    self.advance();
                    model = Some(self.expect_string_lit()?);
                }
                Some(Token::Dimensions) => {
                    self.advance();
                    dimensions = Some(self.expect_int()? as usize);
                }
                Some(Token::TokenMetric) => {
                    self.advance();
                    match self.peek() {
                        Some(Token::Cosine) => { self.advance(); metric = Metric::Cosine; }
                        Some(Token::InnerProduct) => { self.advance(); metric = Metric::InnerProduct; }
                        _ => return Err(ParseError::InvalidSyntax("expected COSINE or INNER_PRODUCT".into())),
                    }
                }
                Some(Token::Sparse) => {
                    self.advance();
                    match self.peek() {
                        Some(Token::True) => { self.advance(); sparse = true; }
                        Some(Token::False) => { self.advance(); sparse = false; }
                        _ => return Err(ParseError::InvalidSyntax("expected true or false after SPARSE".into())),
                    }
                }
                Some(Token::Fields) => {
                    self.advance(); // FIELDS
                    self.expect(&Token::LParen)?;
                    fields.push(self.expect_ident()?);
                    while self.peek() == Some(&Token::Comma) {
                        self.advance();
                        fields.push(self.expect_ident()?);
                    }
                    self.expect(&Token::RParen)?;
                }
                _ => break,
            }
        }

        Ok(RepresentationDecl { name, model, dimensions, metric, sparse, fields })
    }

    fn parse_field_decl(&mut self) -> Result<FieldDecl, ParseError> {
        self.advance(); // FIELD
        let name = self.expect_ident()?;
        let field_type = self.parse_field_type()?;
        Ok(FieldDecl { name, field_type })
    }

    fn parse_field_type(&mut self) -> Result<FieldType, ParseError> {
        match self.advance() {
            Some((Token::Text, _)) => Ok(FieldType::Text),
            Some((Token::DateTime, _)) => Ok(FieldType::DateTime),
            Some((Token::TokenBool, _)) => Ok(FieldType::Bool),
            Some((Token::TokenInt, _)) => Ok(FieldType::Int),
            Some((Token::TokenFloat, _)) => Ok(FieldType::Float),
            Some((Token::EntityRef, _)) => {
                self.expect(&Token::LParen)?;
                let collection = self.expect_ident()?;
                self.expect(&Token::RParen)?;
                Ok(FieldType::EntityRef(collection))
            }
            Some((tok, pos)) => Err(ParseError::UnexpectedToken {
                pos,
                expected: "field type (TEXT, DATETIME, BOOL, INT, FLOAT, ENTITY_REF)".into(),
                got: format!("{tok:?}"),
            }),
            None => Err(ParseError::UnexpectedEof("expected field type".into())),
        }
    }

    fn parse_index_decl(&mut self) -> Result<IndexDecl, ParseError> {
        self.advance(); // INDEX
        let name = self.expect_ident()?;
        self.expect(&Token::On)?;
        self.expect(&Token::LParen)?;

        let mut fields = vec![self.expect_ident()?];
        while self.peek() == Some(&Token::Comma) {
            self.advance();
            fields.push(self.expect_ident()?);
        }
        self.expect(&Token::RParen)?;

        let partial_condition = if self.peek() == Some(&Token::Where) {
            self.advance();
            Some(self.parse_where_clause()?)
        } else {
            None
        };

        Ok(IndexDecl { name, fields, partial_condition })
    }

    fn parse_create_edge(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // EDGE
        let name = self.expect_ident()?;
        self.expect(&Token::From)?;
        let from_collection = self.expect_ident()?;
        self.expect(&Token::To)?;
        let to_collection = self.expect_ident()?;

        // Optional DECAY clause
        let decay_config = if self.peek() == Some(&Token::Decay) {
            self.advance(); // DECAY

            let decay_fn = match self.advance() {
                Some((Token::Exponential, _)) => DecayFnDecl::Exponential,
                Some((Token::Linear, _)) => DecayFnDecl::Linear,
                Some((Token::Step, _)) => DecayFnDecl::Step,
                Some((tok, pos)) => {
                    return Err(ParseError::UnexpectedToken {
                        pos,
                        expected: "EXPONENTIAL, LINEAR, or STEP".to_string(),
                        got: format!("{tok:?}"),
                    });
                }
                None => {
                    return Err(ParseError::UnexpectedEof(
                        "expected decay function (EXPONENTIAL, LINEAR, or STEP)".to_string(),
                    ));
                }
            };

            let mut rate = None;
            let mut floor = None;
            let mut prune = None;

            // Parse optional RATE, FLOOR, PRUNE in any order
            loop {
                match self.peek() {
                    Some(Token::Rate) => {
                        self.advance();
                        rate = Some(self.expect_float_or_int()?);
                    }
                    Some(Token::TokenFloor) => {
                        self.advance();
                        floor = Some(self.expect_float_or_int()?);
                    }
                    Some(Token::Prune) => {
                        self.advance();
                        prune = Some(self.expect_float_or_int()?);
                    }
                    _ => break,
                }
            }

            Some(DecayConfigDecl {
                decay_fn: Some(decay_fn),
                decay_rate: rate,
                floor,
                promote_threshold: None,
                prune_threshold: prune,
            })
        } else {
            None
        };

        // Optional INFER AUTO clause
        let inference_config = if self.peek() == Some(&Token::Infer) {
            self.advance(); // INFER
            self.expect(&Token::Auto)?;
            let mut confidence_floor = None;
            let mut limit = None;
            loop {
                match self.peek() {
                    Some(Token::Confidence) => {
                        self.advance(); // CONFIDENCE
                        self.expect(&Token::Gt)?;
                        confidence_floor = Some(self.expect_float_or_int()? as f32);
                    }
                    Some(Token::Limit) => {
                        self.advance(); // LIMIT
                        limit = Some(self.expect_int()? as usize);
                    }
                    _ => break,
                }
            }
            Some(InferenceConfigDecl {
                auto: true,
                confidence_floor,
                limit,
            })
        } else {
            None
        };

        self.expect(&Token::Semicolon)?;
        Ok(Statement::CreateEdgeType(CreateEdgeTypeStmt {
            name,
            from_collection,
            to_collection,
            decay_config,
            inference_config,
        }))
    }

    fn parse_insert(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // INSERT
        match self.peek() {
            Some(Token::Into) => self.parse_insert_entity(),
            Some(Token::Edge) => self.parse_insert_edge(),
            Some(tok) => {
                let tok_str = format!("{tok:?}");
                let pos = self.tokens[self.pos].1.start;
                Err(ParseError::UnexpectedToken {
                    pos,
                    expected: "INTO or EDGE".to_string(),
                    got: tok_str,
                })
            }
            None => Err(ParseError::UnexpectedEof("expected INTO or EDGE".to_string())),
        }
    }

    fn parse_insert_entity(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // INTO
        let collection = self.expect_ident()?;
        self.expect(&Token::LParen)?;

        let mut fields = vec![self.expect_ident()?];
        while self.peek() == Some(&Token::Comma) {
            self.advance();
            fields.push(self.expect_ident()?);
        }
        self.expect(&Token::RParen)?;

        self.expect(&Token::Values)?;
        self.expect(&Token::LParen)?;

        let mut values = vec![self.parse_literal()?];
        while self.peek() == Some(&Token::Comma) {
            self.advance();
            values.push(self.parse_literal()?);
        }
        self.expect(&Token::RParen)?;

        // Parse named representation vectors (no backward-compat shorthand per spec)
        let mut vectors = Vec::new();

        // Named representations
        while self.peek() == Some(&Token::Representation) {
            self.advance(); // REPRESENTATION
            let repr_name = self.expect_name()?;
            match self.peek() {
                Some(Token::Vector) => {
                    self.advance();
                    let vec = self.parse_float_list()?;
                    vectors.push((repr_name, VectorLiteral::Dense(vec)));
                }
                Some(Token::Sparse) => {
                    self.advance();
                    let vec = self.parse_sparse_vector_list()?;
                    vectors.push((repr_name, VectorLiteral::Sparse(vec)));
                }
                Some(tok) => {
                    let tok_str = format!("{tok:?}");
                    let pos = self.tokens[self.pos].1.start;
                    return Err(ParseError::UnexpectedToken {
                        pos,
                        expected: "VECTOR or SPARSE".into(),
                        got: tok_str,
                    });
                }
                None => return Err(ParseError::UnexpectedEof("expected VECTOR or SPARSE".into())),
            }
        }

        // Parse optional colocation clauses
        let mut collocate_with = None;
        let mut affinity_group = None;
        if self.peek() == Some(&Token::Collocate) {
            self.advance(); // COLLOCATE
            self.expect(&Token::With)?; // WITH
            self.expect(&Token::LParen)?;
            let mut ids = vec![];
            loop {
                let id = self.expect_string_lit()?;
                ids.push(id);
                if self.peek() != Some(&Token::Comma) {
                    break;
                }
                self.advance();
            }
            self.expect(&Token::RParen)?;
            collocate_with = Some(ids);
        } else if self.peek() == Some(&Token::Affinity) {
            self.advance(); // AFFINITY
            self.expect(&Token::Group)?; // GROUP
            let name = self.expect_string_lit()?;
            affinity_group = Some(name);
        }

        self.expect(&Token::Semicolon)?;
        Ok(Statement::Insert(InsertStmt {
            collection,
            fields,
            values,
            vectors,
            collocate_with,
            affinity_group,
        }))
    }

    fn parse_insert_edge(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // EDGE
        let edge_type = self.expect_ident()?;
        self.expect(&Token::From)?;
        let from_id = self.expect_string_lit()?;
        self.expect(&Token::To)?;
        let to_id = self.expect_string_lit()?;

        // Optional WITH (key = value, ...)
        let metadata = if self.peek() == Some(&Token::With) {
            self.advance(); // WITH
            self.parse_kv_list()?
        } else {
            Vec::new()
        };

        self.expect(&Token::Semicolon)?;
        Ok(Statement::InsertEdge(InsertEdgeStmt {
            edge_type,
            from_id,
            to_id,
            metadata,
        }))
    }

    fn parse_update(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // UPDATE
        let entity_id = self.expect_string_lit()?;
        self.expect(&Token::In)?;
        let collection = self.expect_ident()?;
        self.expect(&Token::Set)?;

        let mut assignments = Vec::new();
        loop {
            let field = self.expect_ident()?;
            self.expect(&Token::Eq)?;
            if self.peek() == Some(&Token::Null) {
                let pos = self.tokens.get(self.pos).map(|(_, s)| s.start).unwrap_or(0);
                return Err(ParseError::UnexpectedToken {
                    pos,
                    expected: "value (NULL not supported in UPDATE)".to_string(),
                    got: "NULL".into(),
                });
            }
            let value = self.parse_literal()?;
            assignments.push((field, value));
            if self.peek() == Some(&Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(&Token::Semicolon)?;
        Ok(Statement::Update(UpdateStmt {
            entity_id,
            collection,
            assignments,
        }))
    }

    fn parse_delete(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // DELETE
        if self.peek() == Some(&Token::Edge) {
            self.parse_delete_edge()
        } else {
            self.parse_delete_entity()
        }
    }

    fn parse_delete_edge(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // EDGE
        let edge_type = self.expect_ident()?;
        self.expect(&Token::From)?;
        let from_id = self.expect_string_lit()?;
        self.expect(&Token::To)?;
        let to_id = self.expect_string_lit()?;
        self.expect(&Token::Semicolon)?;
        Ok(Statement::DeleteEdge(DeleteEdgeStmt {
            edge_type,
            from_id,
            to_id,
        }))
    }

    fn parse_delete_entity(&mut self) -> Result<Statement, ParseError> {
        let entity_id = self.expect_string_lit()?;
        self.expect(&Token::From)?;
        let collection = self.expect_ident()?;
        self.expect(&Token::Semicolon)?;
        Ok(Statement::Delete(DeleteStmt {
            entity_id,
            collection,
        }))
    }

    fn parse_traverse(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // TRAVERSE
        let edge_type = self.expect_ident()?;
        self.expect(&Token::From)?;
        let from_id = self.expect_string_lit()?;

        // Optional DEPTH n (default 1)
        let depth = if self.peek() == Some(&Token::Depth) {
            self.advance();
            self.expect_int()? as usize
        } else {
            1
        };

        // Optional LIMIT n
        let limit = if self.peek() == Some(&Token::Limit) {
            self.advance();
            Some(self.expect_int()? as usize)
        } else {
            None
        };

        self.expect(&Token::Semicolon)?;
        Ok(Statement::Traverse(TraverseStmt {
            edge_type,
            from_id,
            depth,
            limit,
        }))
    }

    fn parse_kv_list(&mut self) -> Result<Vec<(String, Literal)>, ParseError> {
        self.expect(&Token::LParen)?;
        let mut pairs = Vec::new();
        let key = self.expect_ident()?;
        self.expect(&Token::Eq)?;
        let value = self.parse_literal()?;
        pairs.push((key, value));
        while self.peek() == Some(&Token::Comma) {
            self.advance();
            let key = self.expect_ident()?;
            self.expect(&Token::Eq)?;
            let value = self.parse_literal()?;
            pairs.push((key, value));
        }
        self.expect(&Token::RParen)?;
        Ok(pairs)
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

        let order_by = if self.peek() == Some(&Token::Order) {
            self.advance();
            self.parse_order_by()?
        } else {
            vec![]
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
            order_by,
            limit,
        }))
    }

    fn parse_order_by(&mut self) -> Result<Vec<OrderByClause>, ParseError> {
        let mut clauses = Vec::new();
        self.expect(&Token::By)?;
        loop {
            let field = self.expect_ident()?;
            let direction = match self.peek() {
                Some(Token::Asc) => { self.advance(); SortDirection::Asc }
                Some(Token::Desc) => { self.advance(); SortDirection::Desc }
                _ => SortDirection::Asc, // default
            };
            clauses.push(OrderByClause { field, direction });
            if self.peek() == Some(&Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(clauses)
    }

    fn parse_search(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // SEARCH
        let collection = self.expect_ident()?;

        // Optional WHERE clause (ScalarPreFilter)
        let filter = if self.peek() == Some(&Token::Where) {
            self.advance();
            Some(self.parse_where_clause()?)
        } else {
            None
        };

        // Parse NEAR VECTOR, NEAR SPARSE, and/or NEAR 'text query'
        let mut dense_vector = None;
        let mut sparse_vector = None;
        let mut query_text = None;

        while self.peek() == Some(&Token::Near) {
            self.advance(); // NEAR
            match self.peek() {
                Some(Token::Vector) => {
                    self.advance(); // VECTOR
                    dense_vector = Some(self.parse_float_list()?);
                }
                Some(Token::Sparse) => {
                    self.advance(); // SPARSE
                    sparse_vector = Some(self.parse_sparse_vector_list()?);
                }
                Some(Token::StringLit(_)) => {
                    if let Some((Token::StringLit(s), _)) = self.advance() {
                        query_text = Some(s);
                    }
                }
                Some(tok) => {
                    let tok_str = format!("{tok:?}");
                    let pos = self.tokens[self.pos].1.start;
                    return Err(ParseError::UnexpectedToken {
                        pos,
                        expected: "VECTOR, SPARSE, or string literal".into(),
                        got: tok_str,
                    });
                }
                None => return Err(ParseError::UnexpectedEof("expected VECTOR, SPARSE, or string literal".into())),
            }
        }

        // Parse optional USING repr_name
        let using_repr = if self.peek() == Some(&Token::Using) {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };

        if dense_vector.is_none() && sparse_vector.is_none() && query_text.is_none() {
            return Err(ParseError::InvalidSyntax(
                "SEARCH requires at least one NEAR VECTOR, NEAR SPARSE, or NEAR 'text query' clause".into(),
            ));
        }

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
            fields: FieldList::All,
            dense_vector,
            sparse_vector,
            filter,
            confidence,
            limit,
            query_text,
            using_repr,
        }))
    }

    fn parse_sparse_vector_list(&mut self) -> Result<Vec<(u32, f32)>, ParseError> {
        self.expect(&Token::LBracket)?;
        let mut entries = Vec::new();

        loop {
            if self.peek() == Some(&Token::RBracket) {
                break;
            }
            let dim = self.expect_int()? as u32;
            self.expect(&Token::Colon)?;
            let weight = self.parse_number_as_f64()? as f32;
            entries.push((dim, weight));

            if self.peek() == Some(&Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }

        self.expect(&Token::RBracket)?;
        Ok(entries)
    }

    fn parse_explain(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // EXPLAIN
        if self.peek() == Some(&Token::Tiers) {
            self.advance(); // TIERS
            let collection = self.expect_ident()?;
            self.expect(&Token::Semicolon)?;
            Ok(Statement::ExplainTiers(ExplainTiersStmt { collection }))
        } else if self.peek() == Some(&Token::History) {
            self.advance(); // HISTORY
            let entity_id = self.expect_string_lit()?;
            let limit = if self.peek() == Some(&Token::Limit) {
                self.advance(); // LIMIT
                Some(self.expect_int()? as usize)
            } else {
                None
            };
            self.expect(&Token::Semicolon)?;
            Ok(Statement::ExplainHistory(ExplainHistoryStmt {
                entity_id,
                limit,
            }))
        } else {
            let inner = self.parse_statement()?;
            Ok(Statement::Explain(Box::new(inner)))
        }
    }

    fn parse_demote(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // DEMOTE
        let entity_id = self.expect_string_lit()?;
        self.expect(&Token::From)?;
        let collection = self.expect_ident()?;
        self.expect(&Token::To)?;

        let target_tier = match self.peek() {
            Some(Token::Warm) => {
                self.advance();
                TierTarget::Warm
            }
            Some(Token::Archive) => {
                self.advance();
                TierTarget::Archive
            }
            _ => return Err(ParseError::InvalidSyntax("expected WARM or ARCHIVE".into())),
        };

        self.expect(&Token::Semicolon)?;
        Ok(Statement::Demote(DemoteStmt {
            entity_id,
            collection,
            target_tier,
        }))
    }

    fn parse_promote(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // PROMOTE
        let entity_id = self.expect_string_lit()?;
        self.expect(&Token::From)?;
        let collection = self.expect_ident()?;
        self.expect(&Token::Semicolon)?;
        Ok(Statement::Promote(PromoteStmt {
            entity_id,
            collection,
        }))
    }

    fn parse_confirm_edge(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // CONFIRM
        self.expect(&Token::Edge)?;
        self.expect(&Token::From)?;
        let from_id = self.expect_string_lit()?;
        self.expect(&Token::To)?;
        let to_id = self.expect_string_lit()?;
        self.expect(&Token::Type)?;
        let edge_type = self.expect_ident()?;
        self.expect(&Token::Confidence)?;
        let confidence = self.expect_float_or_int()? as f32;
        self.expect(&Token::Semicolon)?;
        Ok(Statement::ConfirmEdge(ConfirmEdgeStmt {
            from_id,
            to_id,
            edge_type,
            confidence,
        }))
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
        // Handle NOT prefix
        if self.peek() == Some(&Token::Not) {
            self.advance();
            let inner = self.parse_where_comparison()?;
            return Ok(WhereClause::Not(Box::new(inner)));
        }

        let field = self.expect_ident()?;

        // IS NULL / IS NOT NULL
        if self.peek() == Some(&Token::Is) {
            self.advance();
            if self.peek() == Some(&Token::Not) {
                self.advance();
                self.expect(&Token::Null)?;
                return Ok(WhereClause::IsNotNull(field));
            }
            self.expect(&Token::Null)?;
            return Ok(WhereClause::IsNull(field));
        }

        // IN (val1, val2, ...)
        if self.peek() == Some(&Token::In) {
            self.advance();
            self.expect(&Token::LParen)?;
            let mut values = vec![self.parse_literal()?];
            while self.peek() == Some(&Token::Comma) {
                self.advance();
                values.push(self.parse_literal()?);
            }
            self.expect(&Token::RParen)?;
            return Ok(WhereClause::In(field, values));
        }

        // LIKE 'pattern'
        if self.peek() == Some(&Token::Like) {
            self.advance();
            let pattern = self.expect_string_lit()?;
            return Ok(WhereClause::Like(field, pattern));
        }

        // Existing operators: =, !=, >, <, >=, <=
        let op = self.advance().ok_or_else(|| ParseError::UnexpectedEof("expected operator".into()))?;
        let lit = self.parse_literal()?;

        match op.0 {
            Token::Eq => Ok(WhereClause::Eq(field, lit)),
            Token::Neq => Ok(WhereClause::Neq(field, lit)),
            Token::Gt => Ok(WhereClause::Gt(field, lit)),
            Token::Lt => Ok(WhereClause::Lt(field, lit)),
            Token::Gte => Ok(WhereClause::Gte(field, lit)),
            Token::Lte => Ok(WhereClause::Lte(field, lit)),
            _ => Err(ParseError::UnexpectedToken {
                pos: op.1,
                expected: "comparison operator".into(),
                got: format!("{:?}", op.0),
            }),
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

    /// Parse: INFER EDGES FROM <string_lit> [VIA <ident> (, <ident>)*] RETURNING (TOP <int> | ALL) [CONFIDENCE > <float>] ;
    fn parse_infer(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // INFER
        self.expect(&Token::Edges)?;
        self.expect(&Token::From)?;
        let from_id = self.expect_string_lit()?;

        // Optional VIA clause: comma-separated edge type identifiers
        let edge_types = if self.peek() == Some(&Token::Via) {
            self.advance(); // VIA
            let mut types = vec![self.expect_ident()?];
            while self.peek() == Some(&Token::Comma) {
                self.advance(); // ,
                types.push(self.expect_ident()?);
            }
            types
        } else {
            Vec::new()
        };

        // RETURNING (TOP <int> | ALL)
        self.expect(&Token::Returning)?;
        let limit = if self.peek() == Some(&Token::Top) {
            self.advance(); // TOP
            Some(self.expect_int()? as usize)
        } else {
            self.expect(&Token::All)?;
            None
        };

        // Optional CONFIDENCE > <float>
        let confidence_floor = if self.peek() == Some(&Token::Confidence) {
            self.advance(); // CONFIDENCE
            self.expect(&Token::Gt)?;
            Some(self.parse_number_as_f64()? as f32)
        } else {
            None
        };

        self.expect(&Token::Semicolon)?;
        Ok(Statement::Infer(InferStmt {
            from_id,
            edge_types,
            limit,
            confidence_floor,
        }))
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
    fn parse_create_collection_block() {
        let stmt = parse("CREATE COLLECTION venues (
            REPRESENTATION identity MODEL 'jina-v4' DIMENSIONS 1024 METRIC COSINE,
            FIELD status TEXT,
            FIELD venue_id ENTITY_REF(venues),
            INDEX idx_status ON (status)
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                assert_eq!(c.name, "venues");
                assert_eq!(c.representations.len(), 1);
                assert_eq!(c.representations[0].name, "identity");
                assert_eq!(c.representations[0].dimensions, Some(1024));
                assert_eq!(c.representations[0].metric, Metric::Cosine);
                assert!(!c.representations[0].sparse);
                assert_eq!(c.fields.len(), 2);
                assert_eq!(c.fields[0].name, "status");
                assert_eq!(c.fields[0].field_type, FieldType::Text);
                assert_eq!(c.indexes.len(), 1);
                assert_eq!(c.indexes[0].name, "idx_status");
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_create_collection_sparse_repr() {
        let stmt = parse("CREATE COLLECTION docs (
            REPRESENTATION sparse_title MODEL 'splade-v3' METRIC INNER_PRODUCT SPARSE true
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                assert_eq!(c.representations[0].sparse, true);
                assert_eq!(c.representations[0].metric, Metric::InnerProduct);
                assert_eq!(c.representations[0].dimensions, None);
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_create_collection_partial_index() {
        let stmt = parse("CREATE COLLECTION events (
            FIELD publish_ready BOOL,
            INDEX idx_ready ON (publish_ready) WHERE publish_ready = true
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                assert!(c.indexes[0].partial_condition.is_some());
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_create_collection_compound_index() {
        let stmt = parse("CREATE COLLECTION events (
            FIELD venue_id TEXT,
            FIELD start_date DATETIME,
            INDEX idx_venue_start ON (venue_id, start_date)
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                assert_eq!(c.indexes[0].fields, vec!["venue_id", "start_date"]);
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_search_dense_only() {
        let stmt = parse("SEARCH venues NEAR VECTOR [0.1, 0.2, 0.3] LIMIT 20;").unwrap();
        match stmt {
            Statement::Search(s) => {
                assert!(s.dense_vector.is_some());
                assert!(s.sparse_vector.is_none());
                assert!(s.filter.is_none());
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn parse_search_sparse_only() {
        let stmt = parse("SEARCH venues NEAR SPARSE [1:0.8, 42:0.5, 1337:0.3] LIMIT 20;").unwrap();
        match stmt {
            Statement::Search(s) => {
                assert!(s.dense_vector.is_none());
                let sparse = s.sparse_vector.unwrap();
                assert_eq!(sparse.len(), 3);
                assert_eq!(sparse[0], (1, 0.8));
                assert_eq!(sparse[1], (42, 0.5));
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn parse_search_hybrid() {
        let stmt = parse("SEARCH venues NEAR VECTOR [0.1, 0.2] NEAR SPARSE [1:0.8] LIMIT 10;").unwrap();
        match stmt {
            Statement::Search(s) => {
                assert!(s.dense_vector.is_some());
                assert!(s.sparse_vector.is_some());
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn parse_search_with_where() {
        let stmt = parse("SEARCH venues WHERE h3_res4 = '89283' NEAR VECTOR [0.1, 0.2] LIMIT 10;").unwrap();
        match stmt {
            Statement::Search(s) => {
                assert!(s.filter.is_some());
                assert!(s.dense_vector.is_some());
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn parse_search_no_near_fails() {
        let result = parse("SEARCH venues LIMIT 10;");
        assert!(result.is_err());
    }

    #[test]
    fn parse_search_near_text() {
        let stmt = parse("SEARCH venues NEAR 'live jazz in Bristol' LIMIT 10;").unwrap();
        match stmt {
            Statement::Search(s) => {
                assert_eq!(s.query_text.unwrap(), "live jazz in Bristol");
                assert!(s.dense_vector.is_none());
                assert!(s.sparse_vector.is_none());
                assert!(s.using_repr.is_none());
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn parse_search_near_text_with_using() {
        let stmt = parse("SEARCH events NEAR 'jazz music' USING semantic LIMIT 5;").unwrap();
        match stmt {
            Statement::Search(s) => {
                assert_eq!(s.query_text.unwrap(), "jazz music");
                assert_eq!(s.using_repr.unwrap(), "semantic");
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn parse_search_near_text_with_where() {
        let stmt = parse("SEARCH venues WHERE city = 'Bristol' NEAR 'live jazz' LIMIT 10;").unwrap();
        match stmt {
            Statement::Search(s) => {
                assert!(s.filter.is_some());
                assert_eq!(s.query_text.unwrap(), "live jazz");
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn parse_insert_with_named_repr() {
        let stmt = parse("INSERT INTO venues (id, name) VALUES ('v1', 'Pub')
            REPRESENTATION identity VECTOR [0.1, 0.2, 0.3];").unwrap();
        match stmt {
            Statement::Insert(i) => {
                assert_eq!(i.vectors.len(), 1);
                assert_eq!(i.vectors[0].0, "identity");
                match &i.vectors[0].1 {
                    VectorLiteral::Dense(v) => assert_eq!(v.len(), 3),
                    _ => panic!("expected Dense"),
                }
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_with_sparse_repr() {
        let stmt = parse("INSERT INTO venues (id, name) VALUES ('v1', 'Pub')
            REPRESENTATION sparse_title SPARSE [1:0.8, 42:0.5];").unwrap();
        match stmt {
            Statement::Insert(i) => {
                assert_eq!(i.vectors.len(), 1);
                match &i.vectors[0].1 {
                    VectorLiteral::Sparse(s) => assert_eq!(s.len(), 2),
                    _ => panic!("expected Sparse"),
                }
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_multiple_reprs() {
        let stmt = parse("INSERT INTO venues (id) VALUES ('v1')
            REPRESENTATION identity VECTOR [0.1, 0.2]
            REPRESENTATION sparse SPARSE [1:0.5];").unwrap();
        match stmt {
            Statement::Insert(i) => assert_eq!(i.vectors.len(), 2),
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_no_vector() {
        let stmt = parse("INSERT INTO venues (id, name) VALUES ('v1', 'Pub');").unwrap();
        match stmt {
            Statement::Insert(i) => assert!(i.vectors.is_empty()),
            _ => panic!("expected Insert"),
        }
    }

    // Keep existing tests for FETCH, edge operations, TRAVERSE
    #[test]
    fn parse_fetch_all() {
        let stmt = parse("FETCH * FROM venues;").unwrap();
        assert_eq!(
            stmt,
            Statement::Fetch(FetchStmt {
                collection: "venues".to_string(),
                fields: FieldList::All,
                filter: None,
                order_by: vec![],
                limit: None,
            })
        );
    }

    #[test]
    fn parse_fetch_with_where() {
        let stmt = parse("FETCH name, city FROM venues WHERE city = 'London';").unwrap();
        match &stmt {
            Statement::Fetch(f) => {
                assert_eq!(f.collection, "venues");
                assert!(f.filter.is_some());
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_fetch_where_and() {
        let stmt = parse("FETCH * FROM venues WHERE city = 'London' AND status = 'active';").unwrap();
        match &stmt {
            Statement::Fetch(f) => {
                match &f.filter {
                    Some(WhereClause::And(_, _)) => {}
                    other => panic!("expected And clause, got {other:?}"),
                }
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_fetch_with_limit() {
        let stmt = parse("FETCH * FROM venues LIMIT 10;").unwrap();
        match &stmt {
            Statement::Fetch(f) => assert_eq!(f.limit, Some(10)),
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_error_missing_semicolon() {
        assert!(parse("FETCH * FROM venues").is_err());
    }

    #[test]
    fn parse_create_edge() {
        let stmt = parse("CREATE EDGE knows FROM people TO people;").unwrap();
        assert_eq!(
            stmt,
            Statement::CreateEdgeType(CreateEdgeTypeStmt {
                name: "knows".to_string(),
                from_collection: "people".to_string(),
                to_collection: "people".to_string(),
                decay_config: None,
                inference_config: None,
            })
        );
    }

    #[test]
    fn parse_insert_edge() {
        let stmt = parse("INSERT EDGE knows FROM 'v1' TO 'v2';").unwrap();
        match &stmt {
            Statement::InsertEdge(e) => {
                assert_eq!(e.edge_type, "knows");
                assert!(e.metadata.is_empty());
            }
            _ => panic!("expected InsertEdge"),
        }
    }

    #[test]
    fn parse_insert_edge_with_metadata() {
        let stmt = parse("INSERT EDGE knows FROM 'v1' TO 'v2' WITH (since = '2024');").unwrap();
        match &stmt {
            Statement::InsertEdge(e) => assert_eq!(e.metadata.len(), 1),
            _ => panic!("expected InsertEdge"),
        }
    }

    #[test]
    fn parse_delete_edge() {
        let stmt = parse("DELETE EDGE knows FROM 'v1' TO 'v2';").unwrap();
        assert_eq!(
            stmt,
            Statement::DeleteEdge(DeleteEdgeStmt {
                edge_type: "knows".to_string(),
                from_id: "v1".to_string(),
                to_id: "v2".to_string(),
            })
        );
    }

    #[test]
    fn parse_delete_entity() {
        let stmt = parse("DELETE 'e1' FROM venues;").unwrap();
        match stmt {
            Statement::Delete(d) => {
                assert_eq!(d.entity_id, "e1");
                assert_eq!(d.collection, "venues");
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn parse_traverse() {
        let stmt = parse("TRAVERSE knows FROM 'v1';").unwrap();
        assert_eq!(
            stmt,
            Statement::Traverse(TraverseStmt {
                edge_type: "knows".to_string(),
                from_id: "v1".to_string(),
                depth: 1,
                limit: None,
            })
        );
    }

    #[test]
    fn parse_traverse_with_depth_and_limit() {
        let stmt = parse("TRAVERSE knows FROM 'v1' DEPTH 1 LIMIT 10;").unwrap();
        match &stmt {
            Statement::Traverse(t) => {
                assert_eq!(t.depth, 1);
                assert_eq!(t.limit, Some(10));
            }
            _ => panic!("expected Traverse"),
        }
    }

    #[test]
    fn parse_create_affinity_group() {
        let stmt = parse("CREATE AFFINITY GROUP 'festival_cluster';").unwrap();
        match stmt {
            Statement::CreateAffinityGroup(s) => {
                assert_eq!(s.name, "festival_cluster");
            }
            _ => panic!("expected CreateAffinityGroup"),
        }
    }

    #[test]
    fn parse_insert_with_collocate_with() {
        let stmt = parse("INSERT INTO venues (id, name) VALUES ('v1', 'Glastonbury') COLLOCATE WITH ('v2', 'v3');").unwrap();
        match stmt {
            Statement::Insert(s) => {
                assert_eq!(s.collocate_with, Some(vec!["v2".into(), "v3".into()]));
                assert_eq!(s.affinity_group, None);
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_with_affinity_group() {
        let stmt = parse("INSERT INTO venues (id, name) VALUES ('v1', 'Glastonbury') AFFINITY GROUP 'festival_cluster';").unwrap();
        match stmt {
            Statement::Insert(s) => {
                assert_eq!(s.collocate_with, None);
                assert_eq!(s.affinity_group, Some("festival_cluster".into()));
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_alter_entity_drop_affinity() {
        let stmt = parse("ALTER ENTITY 'v1' DROP AFFINITY GROUP;").unwrap();
        match stmt {
            Statement::AlterEntityDropAffinity(s) => {
                assert_eq!(s.entity_id, "v1");
            }
            _ => panic!("expected AlterEntityDropAffinity"),
        }
    }

    #[test]
    fn parse_demote_warm() {
        let stmt = parse("DEMOTE 'entity1' FROM venues TO WARM;").unwrap();
        match stmt {
            Statement::Demote(d) => {
                assert_eq!(d.entity_id, "entity1");
                assert_eq!(d.collection, "venues");
                assert_eq!(d.target_tier, TierTarget::Warm);
            }
            _ => panic!("expected Demote"),
        }
    }

    #[test]
    fn parse_demote_archive() {
        let stmt = parse("DEMOTE 'entity1' FROM venues TO ARCHIVE;").unwrap();
        match stmt {
            Statement::Demote(d) => {
                assert_eq!(d.target_tier, TierTarget::Archive);
            }
            _ => panic!("expected Demote"),
        }
    }

    #[test]
    fn parse_promote() {
        let stmt = parse("PROMOTE 'entity1' FROM venues;").unwrap();
        match stmt {
            Statement::Promote(p) => {
                assert_eq!(p.entity_id, "entity1");
                assert_eq!(p.collection, "venues");
            }
            _ => panic!("expected Promote"),
        }
    }

    #[test]
    fn parse_explain_tiers() {
        let stmt = parse("EXPLAIN TIERS venues;").unwrap();
        match stmt {
            Statement::ExplainTiers(e) => {
                assert_eq!(e.collection, "venues");
            }
            _ => panic!("expected ExplainTiers"),
        }
    }

    #[test]
    fn parse_fetch_gte_filter() {
        let stmt = parse("FETCH * FROM venues WHERE score >= 80;").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                assert_eq!(f.filter, Some(WhereClause::Gte("score".into(), Literal::Int(80))));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_fetch_lte_filter() {
        let stmt = parse("FETCH * FROM venues WHERE score <= 20;").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                assert_eq!(f.filter, Some(WhereClause::Lte("score".into(), Literal::Int(20))));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_fetch_range_and() {
        let stmt = parse("FETCH * FROM venues WHERE score >= 10 AND score <= 90;").unwrap();
        match stmt {
            Statement::Fetch(f) => match f.filter {
                Some(WhereClause::And(left, right)) => {
                    assert!(matches!(*left, WhereClause::Gte(..)));
                    assert!(matches!(*right, WhereClause::Lte(..)));
                }
                _ => panic!("expected And(Gte, Lte)"),
            },
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_create_edge_with_decay() {
        let stmt = parse(
            "CREATE EDGE knows FROM people TO people DECAY EXPONENTIAL RATE 0.001 FLOOR 0.1 PRUNE 0.05;"
        ).unwrap();
        match stmt {
            Statement::CreateEdgeType(s) => {
                assert_eq!(s.name, "knows");
                let dc = s.decay_config.unwrap();
                assert!(matches!(dc.decay_fn, Some(DecayFnDecl::Exponential)));
                assert!((dc.decay_rate.unwrap() - 0.001).abs() < 1e-6);
                assert!((dc.floor.unwrap() - 0.1).abs() < 1e-6);
                assert!((dc.prune_threshold.unwrap() - 0.05).abs() < 1e-6);
            }
            _ => panic!("expected CreateEdgeType"),
        }
    }

    #[test]
    fn parse_create_edge_without_decay() {
        let stmt = parse("CREATE EDGE likes FROM people TO people;").unwrap();
        match stmt {
            Statement::CreateEdgeType(s) => {
                assert!(s.decay_config.is_none());
            }
            _ => panic!("expected CreateEdgeType"),
        }
    }

    #[test]
    fn parse_create_edge_decay_linear() {
        let stmt = parse(
            "CREATE EDGE knows FROM people TO people DECAY LINEAR RATE 0.01 FLOOR 0.2;"
        ).unwrap();
        match stmt {
            Statement::CreateEdgeType(s) => {
                let dc = s.decay_config.unwrap();
                assert!(matches!(dc.decay_fn, Some(DecayFnDecl::Linear)));
                assert!((dc.decay_rate.unwrap() - 0.01).abs() < 1e-6);
                assert!((dc.floor.unwrap() - 0.2).abs() < 1e-6);
                assert!(dc.prune_threshold.is_none());
            }
            _ => panic!("expected CreateEdgeType"),
        }
    }

    #[test]
    fn parse_create_edge_decay_step() {
        let stmt = parse(
            "CREATE EDGE knows FROM people TO people DECAY STEP RATE 3600;"
        ).unwrap();
        match stmt {
            Statement::CreateEdgeType(s) => {
                let dc = s.decay_config.unwrap();
                assert!(matches!(dc.decay_fn, Some(DecayFnDecl::Step)));
                assert!((dc.decay_rate.unwrap() - 3600.0).abs() < 1e-6);
                assert!(dc.floor.is_none());
            }
            _ => panic!("expected CreateEdgeType"),
        }
    }

    #[test]
    fn parse_create_edge_with_infer_auto() {
        let stmt = parse(
            "CREATE EDGE performs_at FROM acts TO venues DECAY EXPONENTIAL RATE 0.05 FLOOR 0.1 PRUNE 0.05 INFER AUTO CONFIDENCE > 0.75 LIMIT 5;"
        ).unwrap();
        match stmt {
            Statement::CreateEdgeType(s) => {
                let ic = s.inference_config.unwrap();
                assert!(ic.auto);
                assert_eq!(ic.confidence_floor, Some(0.75));
                assert_eq!(ic.limit, Some(5));
            }
            _ => panic!("expected CreateEdgeType"),
        }
    }

    #[test]
    fn parse_create_edge_infer_auto_no_params() {
        let stmt = parse("CREATE EDGE likes FROM users TO venues INFER AUTO;").unwrap();
        match stmt {
            Statement::CreateEdgeType(s) => {
                let ic = s.inference_config.unwrap();
                assert!(ic.auto);
                assert!(ic.confidence_floor.is_none());
                assert!(ic.limit.is_none());
            }
            _ => panic!("expected CreateEdgeType"),
        }
    }

    #[test]
    fn parse_create_edge_without_infer() {
        let stmt = parse("CREATE EDGE likes FROM users TO venues;").unwrap();
        match stmt {
            Statement::CreateEdgeType(s) => {
                assert!(s.inference_config.is_none());
            }
            _ => panic!("expected CreateEdgeType"),
        }
    }

    #[test]
    fn parse_update_single_field() {
        let stmt = parse("UPDATE 'v1' IN venues SET name = 'New Name';").unwrap();
        match stmt {
            Statement::Update(u) => {
                assert_eq!(u.entity_id, "v1");
                assert_eq!(u.collection, "venues");
                assert_eq!(u.assignments.len(), 1);
                assert_eq!(u.assignments[0].0, "name");
                assert_eq!(u.assignments[0].1, Literal::String("New Name".into()));
            }
            _ => panic!("expected UpdateStmt"),
        }
    }

    #[test]
    fn parse_update_multiple_fields() {
        let stmt = parse("UPDATE 'v1' IN venues SET name = 'X', score = 42, active = true;").unwrap();
        match stmt {
            Statement::Update(u) => {
                assert_eq!(u.assignments.len(), 3);
                assert_eq!(u.assignments[0], ("name".into(), Literal::String("X".into())));
                assert_eq!(u.assignments[1], ("score".into(), Literal::Int(42)));
                assert_eq!(u.assignments[2], ("active".into(), Literal::Bool(true)));
            }
            _ => panic!("expected UpdateStmt"),
        }
    }

    #[test]
    fn parse_update_rejects_null() {
        let err = parse("UPDATE 'v1' IN venues SET name = NULL;").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }));
    }

    #[test]
    fn parse_representation_with_fields() {
        let stmt = parse("CREATE COLLECTION events (
            FIELD name TEXT,
            FIELD description TEXT,
            REPRESENTATION semantic DIMENSIONS 384 FIELDS (name, description),
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                assert_eq!(c.representations.len(), 1);
                assert_eq!(c.representations[0].fields, vec!["name", "description"]);
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_representation_without_fields_is_passthrough() {
        let stmt = parse("CREATE COLLECTION venues (
            REPRESENTATION identity DIMENSIONS 384,
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                assert!(c.representations[0].fields.is_empty());
                assert!(c.vectoriser_config.is_none());
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_collection_with_vectoriser_config() {
        let stmt = parse("CREATE COLLECTION events (
            MODEL 'bge-small-en-v1.5'
            MODEL_PATH '/models/bge.onnx'
            DEVICE 'cpu'
            FIELD name TEXT,
            REPRESENTATION semantic DIMENSIONS 384 FIELDS (name),
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                let vc = c.vectoriser_config.unwrap();
                assert_eq!(vc.model.unwrap(), "bge-small-en-v1.5");
                assert_eq!(vc.model_path.unwrap(), "/models/bge.onnx");
                assert_eq!(vc.device.unwrap(), "cpu");
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_collection_external_vectoriser() {
        let stmt = parse("CREATE COLLECTION events (
            MODEL 'text-embedding-3-small'
            VECTORISER 'external'
            ENDPOINT 'https://api.openai.com/v1/embeddings'
            AUTH 'env:OPENAI_API_KEY'
            FIELD name TEXT,
            REPRESENTATION embed DIMENSIONS 1536 FIELDS (name),
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                let vc = c.vectoriser_config.unwrap();
                assert_eq!(vc.vectoriser_type.unwrap(), "external");
                assert_eq!(vc.endpoint.unwrap(), "https://api.openai.com/v1/embeddings");
                assert_eq!(vc.auth.unwrap(), "env:OPENAI_API_KEY");
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_existing_create_collection_syntax_unchanged() {
        let stmt = parse("CREATE COLLECTION venues (
            REPRESENTATION default MODEL 'jina-v4' DIMENSIONS 1024 METRIC COSINE,
            FIELD status TEXT,
            INDEX idx_status ON (status),
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                assert_eq!(c.name, "venues");
                assert_eq!(c.representations.len(), 1);
                assert_eq!(c.representations[0].model, Some("jina-v4".into()));
                assert!(c.representations[0].fields.is_empty());
                assert!(c.vectoriser_config.is_none());
                assert_eq!(c.fields.len(), 1);
                assert_eq!(c.indexes.len(), 1);
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_confirm_edge() {
        let stmt = parse("CONFIRM EDGE FROM 'e1' TO 'e2' TYPE performs_at CONFIDENCE 0.95;").unwrap();
        match stmt {
            Statement::ConfirmEdge(s) => {
                assert_eq!(s.from_id, "e1");
                assert_eq!(s.to_id, "e2");
                assert_eq!(s.edge_type, "performs_at");
                assert_eq!(s.confidence, 0.95);
            }
            _ => panic!("expected ConfirmEdge"),
        }
    }

    #[test]
    fn parse_confirm_edge_integer_confidence() {
        let stmt = parse("CONFIRM EDGE FROM 'a' TO 'b' TYPE likes CONFIDENCE 1;").unwrap();
        match stmt {
            Statement::ConfirmEdge(s) => {
                assert_eq!(s.from_id, "a");
                assert_eq!(s.to_id, "b");
                assert_eq!(s.edge_type, "likes");
                assert_eq!(s.confidence, 1.0);
            }
            _ => panic!("expected ConfirmEdge"),
        }
    }

    #[test]
    fn parse_explain_history() {
        let stmt = parse("EXPLAIN HISTORY 'ent1' LIMIT 50;").unwrap();
        match stmt {
            Statement::ExplainHistory(s) => {
                assert_eq!(s.entity_id, "ent1");
                assert_eq!(s.limit, Some(50));
            }
            _ => panic!("expected ExplainHistory"),
        }
    }

    #[test]
    fn parse_explain_history_no_limit() {
        let stmt = parse("EXPLAIN HISTORY 'ent1';").unwrap();
        match stmt {
            Statement::ExplainHistory(s) => {
                assert_eq!(s.entity_id, "ent1");
                assert!(s.limit.is_none());
            }
            _ => panic!("expected ExplainHistory"),
        }
    }

    #[test]
    fn parse_infer_basic() {
        let stmt = parse("INFER EDGES FROM 'ent1' RETURNING TOP 10;").unwrap();
        match stmt {
            Statement::Infer(s) => {
                assert_eq!(s.from_id, "ent1");
                assert!(s.edge_types.is_empty());
                assert_eq!(s.limit, Some(10));
                assert!(s.confidence_floor.is_none());
            }
            _ => panic!("expected Infer"),
        }
    }

    #[test]
    fn parse_infer_with_via_and_confidence() {
        let stmt = parse("INFER EDGES FROM 'ent1' VIA performs_at, headlined_by RETURNING TOP 5 CONFIDENCE > 0.80;").unwrap();
        match stmt {
            Statement::Infer(s) => {
                assert_eq!(s.edge_types, vec!["performs_at", "headlined_by"]);
                assert_eq!(s.limit, Some(5));
                assert_eq!(s.confidence_floor, Some(0.80));
            }
            _ => panic!("expected Infer"),
        }
    }

    #[test]
    fn parse_infer_all() {
        let stmt = parse("INFER EDGES FROM 'ent1' RETURNING ALL CONFIDENCE > 0.90;").unwrap();
        match stmt {
            Statement::Infer(s) => {
                assert!(s.limit.is_none());
                assert_eq!(s.confidence_floor, Some(0.90));
            }
            _ => panic!("expected Infer"),
        }
    }

    #[test]
    fn parse_where_not() {
        let stmt = parse("FETCH * FROM venues WHERE NOT name = 'hidden';").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                assert!(matches!(f.filter, Some(WhereClause::Not(_))));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_where_is_null() {
        let stmt = parse("FETCH * FROM venues WHERE description IS NULL;").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                assert_eq!(f.filter, Some(WhereClause::IsNull("description".into())));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_where_is_not_null() {
        let stmt = parse("FETCH * FROM venues WHERE description IS NOT NULL;").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                assert_eq!(f.filter, Some(WhereClause::IsNotNull("description".into())));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_where_in_list() {
        let stmt = parse("FETCH * FROM venues WHERE category IN ('music', 'theatre', 'comedy');").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                match f.filter {
                    Some(WhereClause::In(field, values)) => {
                        assert_eq!(field, "category");
                        assert_eq!(values.len(), 3);
                        assert_eq!(values[0], Literal::String("music".into()));
                    }
                    other => panic!("expected In, got {:?}", other),
                }
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_where_like() {
        let stmt = parse("FETCH * FROM venues WHERE name LIKE 'Jazz%';").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                assert_eq!(f.filter, Some(WhereClause::Like("name".into(), "Jazz%".into())));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_where_neq() {
        let stmt = parse("FETCH * FROM venues WHERE status != 'archived';").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                assert_eq!(f.filter, Some(WhereClause::Neq("status".into(), Literal::String("archived".into()))));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_fetch_order_by_asc() {
        let stmt = parse("FETCH * FROM venues ORDER BY name ASC;").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                assert_eq!(f.order_by.len(), 1);
                assert_eq!(f.order_by[0].field, "name");
                assert_eq!(f.order_by[0].direction, SortDirection::Asc);
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_fetch_order_by_desc() {
        let stmt = parse("FETCH * FROM venues ORDER BY created_at DESC LIMIT 20;").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                assert_eq!(f.order_by.len(), 1);
                assert_eq!(f.order_by[0].field, "created_at");
                assert_eq!(f.order_by[0].direction, SortDirection::Desc);
                assert_eq!(f.limit, Some(20));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_fetch_order_by_default_asc() {
        let stmt = parse("FETCH * FROM venues ORDER BY name;").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                assert_eq!(f.order_by[0].direction, SortDirection::Asc);
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_fetch_order_by_multiple() {
        let stmt = parse("FETCH * FROM venues ORDER BY city ASC, name DESC;").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                assert_eq!(f.order_by.len(), 2);
                assert_eq!(f.order_by[0].field, "city");
                assert_eq!(f.order_by[1].field, "name");
                assert_eq!(f.order_by[1].direction, SortDirection::Desc);
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_fetch_where_order_by() {
        let stmt = parse("FETCH * FROM venues WHERE city = 'London' ORDER BY name LIMIT 5;").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                assert!(f.filter.is_some());
                assert_eq!(f.order_by.len(), 1);
                assert_eq!(f.limit, Some(5));
            }
            _ => panic!("expected Fetch"),
        }
    }
}
