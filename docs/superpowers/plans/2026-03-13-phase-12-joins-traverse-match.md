# Phase 12b: JOINs + TRAVERSE MATCH — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add structural and probabilistic JOINs across edge-linked collections, and upgrade TRAVERSE to support Cypher-inspired MATCH pattern syntax with directed/undirected edges, typed edge filters, and variable depth ranges.

**Architecture:** Follows the established pipeline: new tokens → AST nodes → parser functions → planner plan types → executor arms → proto messages → CLAUDE.md. JOINs are a new statement variant on FETCH. TRAVERSE MATCH extends the existing TraverseStmt/TraversePlan with an optional pattern. Both features are additive — existing TRAVERSE and FETCH behavior unchanged.

**Tech Stack:** Rust 2021, logos lexer, recursive descent parser, Fjall storage, DashMap indexes (AdjacencyIndex), tonic/protobuf gRPC.

**Build command:** `CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace`

**Known flaky test:** `similarity_score_range` — pre-existing, ignore.

---

## File Structure

| File | Role | Chunks |
|------|------|--------|
| `crates/trondb-tql/src/token.rs` | New keyword + symbol tokens | 1, 4 |
| `crates/trondb-tql/src/ast.rs` | New AST types (FetchJoinStmt, TraverseMatchStmt, etc.) | 1, 4 |
| `crates/trondb-tql/src/parser.rs` | parse_fetch_join(), parse_traverse_match() | 2, 5 |
| `crates/trondb-core/src/planner.rs` | JoinPlan, TraverseMatchPlan, plan() arms | 3, 6 |
| `crates/trondb-core/src/executor.rs` | Plan::Join and Plan::TraverseMatch execution | 3, 6 |
| `crates/trondb-core/src/error.rs` | New error variants | 3 |
| `crates/trondb-proto/proto/trondb.proto` | New proto messages | 3, 6 |
| `crates/trondb-proto/src/convert_plan.rs` | Bidirectional proto conversions | 3, 6 |
| `CLAUDE.md` | Document new features | 3, 6 |

---

## Chunk 1: JOIN Tokens + AST (trondb-tql)

Adds tokens and AST types for structural and probabilistic JOINs.

### Task 1: New Tokens for JOINs

**Files:**
- Modify: `crates/trondb-tql/src/token.rs`

- [ ] **Step 1: Write failing tests for new tokens**

In `crates/trondb-tql/src/token.rs`, add to the `#[cfg(test)] mod tests` block:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- lex_join_keywords lex_dot_token lex_structural_join lex_probabilistic_join`
Expected: FAIL — `Token::Join`, `Token::Inner`, `Token::Left`, `Token::Right`, `Token::Full`, `Token::As`, `Token::Dot` do not exist.

- [ ] **Step 3: Add new tokens**

In `crates/trondb-tql/src/token.rs`, add these token variants before the `// Identifiers` comment:

```rust
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
```

And in the symbols section (after the existing `#[token(":")]` line):

```rust
#[token(".")]
Dot,
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- lex_join_keywords lex_dot_token lex_structural_join lex_probabilistic_join`
Expected: PASS.

- [ ] **Step 5: Commit**

`git add crates/trondb-tql/src/token.rs && git commit -m "feat(tql): add JOIN, AS, Dot tokens for Phase 12b"`

### Task 2: AST Types for JOINs

**Files:**
- Modify: `crates/trondb-tql/src/ast.rs`

- [ ] **Step 1: Add JOIN AST types**

In `crates/trondb-tql/src/ast.rs`, add the new types after the existing `DropEdgeTypeStmt` struct:

```rust
// --- JOINs ---

/// A qualified field reference: alias.field (e.g., `e.name`)
#[derive(Debug, Clone, PartialEq)]
pub struct QualifiedField {
    pub alias: String,
    pub field: String,
}

/// Join type
#[derive(Debug, Clone, PartialEq)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
}

/// A single JOIN clause
#[derive(Debug, Clone, PartialEq)]
pub struct JoinClause {
    pub join_type: JoinType,
    pub collection: String,
    pub alias: String,
    pub on_left: QualifiedField,
    pub on_right: QualifiedField,
    /// If Some, this is a probabilistic join with a confidence threshold
    pub confidence_threshold: Option<f64>,
}

/// FETCH ... FROM ... AS ... JOIN ... statement
#[derive(Debug, Clone, PartialEq)]
pub struct FetchJoinStmt {
    pub fields: JoinFieldList,
    pub from_collection: String,
    pub from_alias: String,
    pub joins: Vec<JoinClause>,
    pub filter: Option<WhereClause>,
    pub order_by: Vec<OrderByClause>,
    pub limit: Option<usize>,
    pub hints: Vec<QueryHint>,
}

/// Field list for JOINs — supports qualified (alias.field) and star
#[derive(Debug, Clone, PartialEq)]
pub enum JoinFieldList {
    All,
    Named(Vec<QualifiedField>),
}
```

- [ ] **Step 2: Add FetchJoin variant to Statement enum**

In `crates/trondb-tql/src/ast.rs`, add to the `Statement` enum:

```rust
FetchJoin(FetchJoinStmt),
```

- [ ] **Step 3: Export new types from lib.rs (they're auto-exported via `pub use ast::*`)**

No changes needed — `pub use ast::*` in `lib.rs` already re-exports everything.

- [ ] **Step 4: Run compilation check**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo check -p trondb-tql`
Expected: PASS (no tests yet use the new types, just verifying they compile).

- [ ] **Step 5: Commit**

`git add crates/trondb-tql/src/ast.rs && git commit -m "feat(tql): add JOIN AST types — JoinClause, FetchJoinStmt, QualifiedField"`

---

## Chunk 2: JOIN Parser (trondb-tql)

### Task 3: Parse Qualified Field References and JOIN Syntax

**Files:**
- Modify: `crates/trondb-tql/src/parser.rs`

- [ ] **Step 1: Write failing parser tests**

In `crates/trondb-tql/src/parser.rs`, add to `#[cfg(test)] mod tests`:

```rust
#[test]
fn parse_structural_inner_join() {
    let stmt = parse(
        "FETCH e.name, v.address FROM entities AS e INNER JOIN venues AS v ON e.venue_id = v.id WHERE e.type = 'event';"
    ).unwrap();
    match stmt {
        Statement::FetchJoin(j) => {
            assert_eq!(j.from_collection, "entities");
            assert_eq!(j.from_alias, "e");
            assert_eq!(j.joins.len(), 1);
            assert_eq!(j.joins[0].join_type, JoinType::Inner);
            assert_eq!(j.joins[0].collection, "venues");
            assert_eq!(j.joins[0].alias, "v");
            assert_eq!(j.joins[0].on_left, QualifiedField { alias: "e".into(), field: "venue_id".into() });
            assert_eq!(j.joins[0].on_right, QualifiedField { alias: "v".into(), field: "id".into() });
            assert!(j.joins[0].confidence_threshold.is_none());
            assert!(j.filter.is_some());
            match &j.fields {
                JoinFieldList::Named(fields) => {
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0], QualifiedField { alias: "e".into(), field: "name".into() });
                    assert_eq!(fields[1], QualifiedField { alias: "v".into(), field: "address".into() });
                }
                _ => panic!("expected Named fields"),
            }
        }
        _ => panic!("expected FetchJoin"),
    }
}

#[test]
fn parse_probabilistic_join() {
    let stmt = parse(
        "FETCH e.name, a.name, _edge.confidence FROM entities AS e INNER JOIN acts AS a ON e.id = a.entity_id CONFIDENCE > 0.75;"
    ).unwrap();
    match stmt {
        Statement::FetchJoin(j) => {
            assert_eq!(j.joins[0].confidence_threshold, Some(0.75));
            match &j.fields {
                JoinFieldList::Named(fields) => {
                    assert_eq!(fields.len(), 3);
                    assert_eq!(fields[2], QualifiedField { alias: "_edge".into(), field: "confidence".into() });
                }
                _ => panic!("expected Named fields"),
            }
        }
        _ => panic!("expected FetchJoin"),
    }
}

#[test]
fn parse_left_join() {
    let stmt = parse(
        "FETCH * FROM entities AS e LEFT JOIN venues AS v ON e.venue_id = v.id;"
    ).unwrap();
    match stmt {
        Statement::FetchJoin(j) => {
            assert_eq!(j.joins[0].join_type, JoinType::Left);
            assert_eq!(j.fields, JoinFieldList::All);
        }
        _ => panic!("expected FetchJoin"),
    }
}

#[test]
fn parse_right_join() {
    let stmt = parse(
        "FETCH * FROM entities AS e RIGHT JOIN venues AS v ON e.venue_id = v.id;"
    ).unwrap();
    match stmt {
        Statement::FetchJoin(j) => {
            assert_eq!(j.joins[0].join_type, JoinType::Right);
        }
        _ => panic!("expected FetchJoin"),
    }
}

#[test]
fn parse_full_join() {
    let stmt = parse(
        "FETCH * FROM entities AS e FULL JOIN venues AS v ON e.venue_id = v.id;"
    ).unwrap();
    match stmt {
        Statement::FetchJoin(j) => {
            assert_eq!(j.joins[0].join_type, JoinType::Full);
        }
        _ => panic!("expected FetchJoin"),
    }
}

#[test]
fn parse_join_with_limit() {
    let stmt = parse(
        "FETCH e.name FROM entities AS e INNER JOIN venues AS v ON e.venue_id = v.id LIMIT 10;"
    ).unwrap();
    match stmt {
        Statement::FetchJoin(j) => {
            assert_eq!(j.limit, Some(10));
        }
        _ => panic!("expected FetchJoin"),
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- parse_structural_inner_join parse_probabilistic_join parse_left_join parse_right_join parse_full_join parse_join_with_limit`
Expected: FAIL — `FetchJoin` variant and parse function don't exist.

- [ ] **Step 3: Add qualified field parsing helper**

In `crates/trondb-tql/src/parser.rs`, add this method to `impl Parser` (before `parse_field_list`):

```rust
/// Parse a qualified field: `alias.field` or `_edge.confidence`
/// Returns QualifiedField if a dot follows the ident, otherwise None.
fn try_parse_qualified_field(&mut self) -> Result<Option<QualifiedField>, ParseError> {
    // Check if current token is Ident followed by Dot
    if let Some(Token::Ident(_)) = self.peek() {
        // Lookahead: is the next-next token a Dot?
        if self.pos + 1 < self.tokens.len() {
            if self.tokens[self.pos + 1].0 == Token::Dot {
                let alias = self.expect_ident()?;
                self.expect(&Token::Dot)?;
                let field = self.expect_ident()?;
                return Ok(Some(QualifiedField { alias, field }));
            }
        }
    }
    Ok(None)
}

/// Parse a join field list: `*` or `alias.field, alias.field, ...`
fn parse_join_field_list(&mut self) -> Result<JoinFieldList, ParseError> {
    if self.peek() == Some(&Token::Star) {
        self.advance();
        return Ok(JoinFieldList::All);
    }
    let mut fields = Vec::new();
    // First field
    match self.try_parse_qualified_field()? {
        Some(qf) => fields.push(qf),
        None => {
            // Unqualified field in a JOIN context — error
            let ident = self.expect_ident()?;
            return Err(ParseError::InvalidQuery(format!(
                "JOIN fields must be qualified (alias.field), got bare '{ident}'"
            )));
        }
    }
    while self.peek() == Some(&Token::Comma) {
        self.advance();
        match self.try_parse_qualified_field()? {
            Some(qf) => fields.push(qf),
            None => {
                let ident = self.expect_ident()?;
                return Err(ParseError::InvalidQuery(format!(
                    "JOIN fields must be qualified (alias.field), got bare '{ident}'"
                )));
            }
        }
    }
    Ok(JoinFieldList::Named(fields))
}
```

- [ ] **Step 4: Add ParseError::InvalidQuery variant if missing**

Check `crates/trondb-tql/src/error.rs`. If `InvalidQuery` doesn't exist, add:

```rust
#[error("invalid query: {0}")]
InvalidQuery(String),
```

- [ ] **Step 5: Add parse_fetch_join method**

In `crates/trondb-tql/src/parser.rs`, add to `impl Parser`:

```rust
/// Parse a FETCH ... FROM ... AS ... JOIN statement.
/// Called from parse_fetch when we detect `AS` after the FROM collection.
fn parse_fetch_join(
    &mut self,
    hints: Vec<QueryHint>,
    fields_raw: JoinFieldList,
    from_collection: String,
) -> Result<Statement, ParseError> {
    // We've already consumed: FETCH fields FROM collection
    // Now expect: AS alias
    self.expect(&Token::As)?;
    let from_alias = self.expect_ident()?;

    // Parse one or more JOIN clauses
    let mut joins = Vec::new();
    loop {
        let join_type = match self.peek() {
            Some(Token::Inner) => {
                self.advance();
                self.expect(&Token::Join)?;
                JoinType::Inner
            }
            Some(Token::Left) => {
                self.advance();
                self.expect(&Token::Join)?;
                JoinType::Left
            }
            Some(Token::Right) => {
                self.advance();
                self.expect(&Token::Join)?;
                JoinType::Right
            }
            Some(Token::Full) => {
                self.advance();
                self.expect(&Token::Join)?;
                JoinType::Full
            }
            Some(Token::Join) => {
                self.advance();
                JoinType::Inner // bare JOIN = INNER JOIN
            }
            _ => break,
        };

        let join_collection = self.expect_ident()?;
        self.expect(&Token::As)?;
        let join_alias = self.expect_ident()?;

        self.expect(&Token::On)?;

        // ON left_alias.left_field = right_alias.right_field
        let on_left = self.try_parse_qualified_field()?.ok_or_else(|| {
            ParseError::InvalidQuery("expected qualified field in ON clause (alias.field)".into())
        })?;
        self.expect(&Token::Eq)?;
        let on_right = self.try_parse_qualified_field()?.ok_or_else(|| {
            ParseError::InvalidQuery("expected qualified field in ON clause (alias.field)".into())
        })?;

        // Optional CONFIDENCE > threshold (probabilistic join)
        let confidence_threshold = if self.peek() == Some(&Token::Confidence) {
            self.advance();
            self.expect(&Token::Gt)?;
            Some(self.expect_float_or_int()?)
        } else {
            None
        };

        joins.push(JoinClause {
            join_type,
            collection: join_collection,
            alias: join_alias,
            on_left,
            on_right,
            confidence_threshold,
        });
    }

    if joins.is_empty() {
        return Err(ParseError::InvalidQuery("expected at least one JOIN clause".into()));
    }

    // Optional WHERE
    let filter = if self.peek() == Some(&Token::Where) {
        self.advance();
        Some(self.parse_where_clause()?)
    } else {
        None
    };

    // Optional ORDER BY
    let order_by = if self.peek() == Some(&Token::Order) {
        self.advance();
        self.parse_order_by()?
    } else {
        vec![]
    };

    // Optional LIMIT
    let limit = if self.peek() == Some(&Token::Limit) {
        self.advance();
        Some(self.expect_int()? as usize)
    } else {
        None
    };

    self.expect(&Token::Semicolon)?;
    Ok(Statement::FetchJoin(FetchJoinStmt {
        fields: fields_raw,
        from_collection,
        from_alias,
        joins,
        filter,
        order_by,
        limit,
        hints,
    }))
}
```

- [ ] **Step 6: Modify parse_fetch to detect JOIN syntax**

In `crates/trondb-tql/src/parser.rs`, modify the existing `parse_fetch` method. After parsing `fields` and `FROM collection`, add a lookahead for `AS`:

Replace the existing `parse_fetch` body with:

```rust
fn parse_fetch(&mut self) -> Result<Statement, ParseError> {
    self.advance(); // FETCH
    let hints = self.parse_hints();

    // Try to parse join-style qualified fields first, fall back to plain field list
    let saved_pos = self.pos;
    let join_fields_result = self.parse_join_field_list();

    self.expect(&Token::From)?;
    let collection = self.expect_ident()?;

    // If the next token is AS, this is a JOIN statement
    if self.peek() == Some(&Token::As) {
        // Must have successfully parsed join field list
        let join_fields = match join_fields_result {
            Ok(jf) => jf,
            Err(e) => return Err(e),
        };
        return self.parse_fetch_join(hints, join_fields, collection);
    }

    // Not a JOIN — convert join field list back to plain field list
    // Reset and re-parse as plain fields since we already consumed them
    // Actually, JoinFieldList::All maps to FieldList::All, and Named qualified fields
    // with no alias work too. But simpler: reset and re-parse.
    // We need to backtrack. Let's use a different approach:
    // Reset to saved position and re-parse as plain field list.
    self.pos = saved_pos;
    let fields = self.parse_field_list()?;
    self.expect(&Token::From)?;
    let _collection2 = self.expect_ident()?;

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
        hints,
    }))
}
```

**IMPORTANT:** The backtracking approach above is clunky. A cleaner approach: parse field list normally, then after FROM + collection, check for AS. If AS found, convert the FieldList to JoinFieldList by re-interpreting field names containing `.` as qualified. But since the lexer now splits `e.name` into `Ident("e"), Dot, Ident("name")`, `parse_field_list` will fail on the Dot.

**Better approach — two-pass detection:**

```rust
fn parse_fetch(&mut self) -> Result<Statement, ParseError> {
    self.advance(); // FETCH
    let hints = self.parse_hints();

    // Save position for potential backtrack
    let saved_pos = self.pos;

    // Peek ahead: if first field token is followed by a Dot, this is a JOIN query
    let is_join_style = match self.peek() {
        Some(Token::Star) => {
            // Could be either — check after FROM for AS
            // Parse * then FROM collection, then check for AS
            false // will detect via AS after collection
        }
        Some(Token::Ident(_)) => {
            // Check if next-next is Dot (qualified field)
            self.pos + 1 < self.tokens.len() && self.tokens[self.pos + 1].0 == Token::Dot
        }
        _ => false,
    };

    if is_join_style {
        let join_fields = self.parse_join_field_list()?;
        self.expect(&Token::From)?;
        let collection = self.expect_ident()?;
        return self.parse_fetch_join(hints, join_fields, collection);
    }

    // Standard FETCH — parse plain field list
    let fields = self.parse_field_list()?;
    self.expect(&Token::From)?;
    let collection = self.expect_ident()?;

    // Even with *, check for AS (JOIN with SELECT *)
    if self.peek() == Some(&Token::As) {
        let join_fields = match fields {
            FieldList::All => JoinFieldList::All,
            FieldList::Named(names) => {
                // Convert unqualified names — shouldn't happen if detection above is correct
                return Err(ParseError::InvalidQuery(
                    "JOIN fields must be qualified (alias.field)".into(),
                ));
            }
        };
        return self.parse_fetch_join(hints, join_fields, collection);
    }

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
        hints,
    }))
}
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- parse_structural_inner_join parse_probabilistic_join parse_left_join parse_right_join parse_full_join parse_join_with_limit`
Expected: PASS.

- [ ] **Step 8: Run full tql test suite to verify no regressions**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql`
Expected: PASS — all existing FETCH tests still work.

- [ ] **Step 9: Commit**

`git add crates/trondb-tql/src/ && git commit -m "feat(tql): parse structural and probabilistic JOIN syntax"`

---

## Chunk 3: JOIN Planner + Executor + Proto

### Task 4: JOIN Plan Type + Planner

**Files:**
- Modify: `crates/trondb-core/src/planner.rs`
- Modify: `crates/trondb-core/src/error.rs`

- [ ] **Step 1: Add JoinPlan to planner.rs**

In `crates/trondb-core/src/planner.rs`, add the new plan struct after `DropEdgeTypePlan`:

```rust
#[derive(Debug, Clone)]
pub struct JoinPlan {
    pub fields: trondb_tql::JoinFieldList,
    pub from_collection: String,
    pub from_alias: String,
    pub joins: Vec<trondb_tql::JoinClause>,
    pub filter: Option<WhereClause>,
    pub order_by: Vec<OrderByClause>,
    pub limit: Option<usize>,
    pub hints: Vec<QueryHint>,
}
```

- [ ] **Step 2: Add Plan::Join variant**

In the `Plan` enum, add:

```rust
Join(JoinPlan),
```

- [ ] **Step 3: Add planner arm for FetchJoin**

In the `plan()` function's match, add:

```rust
Statement::FetchJoin(s) => Ok(Plan::Join(JoinPlan {
    fields: s.fields.clone(),
    from_collection: s.from_collection.clone(),
    from_alias: s.from_alias.clone(),
    joins: s.joins.clone(),
    filter: s.filter.clone(),
    order_by: s.order_by.clone(),
    limit: s.limit,
    hints: s.hints.clone(),
})),
```

- [ ] **Step 4: Add planner test**

```rust
#[test]
fn plan_join() {
    use trondb_tql::{FetchJoinStmt, JoinClause, JoinFieldList, JoinType, QualifiedField};

    let stmt = Statement::FetchJoin(FetchJoinStmt {
        fields: JoinFieldList::Named(vec![
            QualifiedField { alias: "e".into(), field: "name".into() },
            QualifiedField { alias: "v".into(), field: "address".into() },
        ]),
        from_collection: "entities".into(),
        from_alias: "e".into(),
        joins: vec![JoinClause {
            join_type: JoinType::Inner,
            collection: "venues".into(),
            alias: "v".into(),
            on_left: QualifiedField { alias: "e".into(), field: "venue_id".into() },
            on_right: QualifiedField { alias: "v".into(), field: "id".into() },
            confidence_threshold: None,
        }],
        filter: None,
        order_by: vec![],
        limit: Some(10),
        hints: vec![],
    });
    let p = plan(&stmt, &empty_schemas()).unwrap();
    match p {
        Plan::Join(jp) => {
            assert_eq!(jp.from_collection, "entities");
            assert_eq!(jp.joins.len(), 1);
            assert_eq!(jp.limit, Some(10));
        }
        _ => panic!("expected JoinPlan"),
    }
}
```

- [ ] **Step 5: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core -- plan_join`
Expected: PASS.

- [ ] **Step 6: Commit**

`git add crates/trondb-core/src/planner.rs && git commit -m "feat(planner): add JoinPlan type and planner arm"`

### Task 5: JOIN Executor

**Files:**
- Modify: `crates/trondb-core/src/executor.rs`
- Modify: `crates/trondb-core/src/error.rs`

- [ ] **Step 1: Add error variant**

In `crates/trondb-core/src/error.rs`, add:

```rust
#[error("join error: {0}")]
JoinError(String),
```

- [ ] **Step 2: Add Plan::Join execution arm**

In `crates/trondb-core/src/executor.rs`, add the `Plan::Join` arm to the `execute()` match. The algorithm:

1. Scan the `from_collection` (full scan or field-index if filter targets it)
2. For each entity in the left side, look up the join field value
3. For structural joins: use field value as ID to fetch from right collection
4. For probabilistic joins: use AdjacencyIndex edge lookup, filter by confidence
5. Combine left + right entity fields into a single Row with alias-qualified keys
6. Apply WHERE filter, ORDER BY, LIMIT

```rust
Plan::Join(p) => {
    // Build alias → collection mapping
    let mut alias_to_collection: HashMap<String, String> = HashMap::new();
    alias_to_collection.insert(p.from_alias.clone(), p.from_collection.clone());
    for join in &p.joins {
        alias_to_collection.insert(join.alias.clone(), join.collection.clone());
    }

    // Fetch all entities from the left (FROM) collection
    let left_entities: Vec<Entity> = self.store
        .scan(&p.from_collection)?
        .collect();

    let mut rows = Vec::new();
    let limit = p.limit.unwrap_or(usize::MAX);

    for left_entity in &left_entities {
        // For each join clause, resolve the right-hand entity
        // Start with the left entity's fields
        let mut combined_row: HashMap<String, Value> = HashMap::new();

        // Add left entity fields with alias prefix
        combined_row.insert(
            format!("{}.id", p.from_alias),
            Value::String(left_entity.id.to_string()),
        );
        for (k, v) in &left_entity.metadata {
            combined_row.insert(format!("{}.{}", p.from_alias, k), v.clone());
        }

        let mut matched = true;

        for join in &p.joins {
            // Get the left-side join field value
            let left_field_val = if join.on_left.field == "id" {
                Some(Value::String(left_entity.id.to_string()))
            } else {
                left_entity.metadata.get(&join.on_left.field).cloned()
            };

            let right_entity = match &left_field_val {
                Some(Value::String(ref id_str)) => {
                    if join.confidence_threshold.is_some() {
                        // Probabilistic join: look up via AdjacencyIndex
                        // The left field value is an entity ID; find edges to right collection
                        let source_id = if join.on_left.field == "id" {
                            LogicalId::from_string(id_str)
                        } else {
                            LogicalId::from_string(id_str)
                        };

                        // Search all edge types between the two collections for matching edges
                        let mut best_match: Option<(Entity, f32)> = None;
                        let threshold = join.confidence_threshold.unwrap_or(0.0) as f32;

                        for et_entry in self.edge_types.iter() {
                            let et = et_entry.value();
                            if et.from_collection == *alias_to_collection.get(&join.on_left.alias).unwrap_or(&String::new())
                                && et.to_collection == join.collection
                            {
                                let entries = self.adjacency.get(&source_id, &et.name);
                                for entry in &entries {
                                    if entry.confidence >= threshold {
                                        // Check if ON right side matches
                                        if let Ok(right_ent) = self.store.get(&join.collection, &entry.to_id) {
                                            let right_val = if join.on_right.field == "id" {
                                                Value::String(right_ent.id.to_string())
                                            } else {
                                                right_ent.metadata.get(&join.on_right.field)
                                                    .cloned().unwrap_or(Value::Null)
                                            };
                                            // For probabilistic join, the ON clause matches if the edge exists
                                            // (the ON fields describe the relationship, edges ARE the relationship)
                                            if best_match.as_ref().map(|(_, c)| entry.confidence > *c).unwrap_or(true) {
                                                best_match = Some((right_ent, entry.confidence));
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        best_match
                    } else {
                        // Structural join: use the field value as a lookup key
                        // The ON right field tells us what field to match in the right collection
                        if join.on_right.field == "id" {
                            // Direct ID lookup
                            self.store.get(&join.collection, &LogicalId::from_string(id_str))
                                .ok().map(|e| (e, 1.0f32))
                        } else {
                            // Need to scan right collection for matching field value
                            let mut found = None;
                            if let Ok(iter) = self.store.scan(&join.collection) {
                                for right_ent in iter {
                                    let right_val = if join.on_right.field == "id" {
                                        Some(Value::String(right_ent.id.to_string()))
                                    } else {
                                        right_ent.metadata.get(&join.on_right.field).cloned()
                                    };
                                    if right_val.as_ref() == left_field_val.as_ref() {
                                        found = Some((right_ent, 1.0f32));
                                        break;
                                    }
                                }
                            }
                            found
                        }
                    }
                }
                _ => None,
            };

            match (right_entity, &join.join_type) {
                (Some((right_ent, confidence)), _) => {
                    // Add right entity fields with alias prefix
                    combined_row.insert(
                        format!("{}.id", join.alias),
                        Value::String(right_ent.id.to_string()),
                    );
                    for (k, v) in &right_ent.metadata {
                        combined_row.insert(format!("{}.{}", join.alias, k), v.clone());
                    }
                    // Add _edge.confidence if requested
                    if join.confidence_threshold.is_some() {
                        combined_row.insert(
                            "_edge.confidence".into(),
                            Value::Float(confidence as f64),
                        );
                    }
                }
                (None, JoinType::Inner) => {
                    matched = false;
                    break;
                }
                (None, JoinType::Left) | (None, JoinType::Full) => {
                    // Left/Full: include left row with NULLs for right side
                    combined_row.insert(format!("{}.id", join.alias), Value::Null);
                }
                (None, JoinType::Right) => {
                    // Right join: skip this left entity (right entities without matches
                    // handled separately below)
                    matched = false;
                    break;
                }
            }
        }

        if !matched {
            continue;
        }

        // Apply WHERE filter on combined row
        if let Some(ref filter) = p.filter {
            if !join_row_matches(&combined_row, filter) {
                continue;
            }
        }

        // Project requested fields
        let projected = project_join_row(&combined_row, &p.fields);
        rows.push(projected);

        if rows.len() >= limit {
            break;
        }
    }

    // For RIGHT and FULL joins, also scan unmatched right entities
    for join in &p.joins {
        if matches!(join.join_type, JoinType::Right | JoinType::Full) {
            if let Ok(iter) = self.store.scan(&join.collection) {
                for right_ent in iter {
                    // Check if this right entity was already matched
                    let right_id_key = format!("{}.id", join.alias);
                    let right_id_str = right_ent.id.to_string();
                    let already_matched = rows.iter().any(|r| {
                        r.values.get(&right_id_key)
                            .map(|v| matches!(v, Value::String(s) if s == &right_id_str))
                            .unwrap_or(false)
                    });
                    if already_matched {
                        continue;
                    }

                    let mut combined_row: HashMap<String, Value> = HashMap::new();
                    // NULLs for left side
                    combined_row.insert(format!("{}.id", p.from_alias), Value::Null);
                    // Right side fields
                    combined_row.insert(
                        format!("{}.id", join.alias),
                        Value::String(right_ent.id.to_string()),
                    );
                    for (k, v) in &right_ent.metadata {
                        combined_row.insert(format!("{}.{}", join.alias, k), v.clone());
                    }

                    if let Some(ref filter) = p.filter {
                        if !join_row_matches(&combined_row, filter) {
                            continue;
                        }
                    }

                    let projected = project_join_row(&combined_row, &p.fields);
                    rows.push(projected);

                    if rows.len() >= limit {
                        break;
                    }
                }
            }
        }
    }

    let scanned = rows.len();

    Ok(QueryResult {
        columns: build_join_columns(&rows, &p.fields),
        rows,
        stats: QueryStats {
            elapsed: start.elapsed(),
            entities_scanned: scanned,
            mode: QueryMode::Deterministic,
            tier: "Fjall".into(),
        },
    })
}
```

- [ ] **Step 3: Add helper functions**

Add these free functions near the other helpers (after `entity_to_row`):

```rust
fn join_row_matches(combined: &HashMap<String, Value>, clause: &WhereClause) -> bool {
    // WHERE clauses in JOIN context use qualified field names (e.g., "e.type")
    // The combined row already has alias-qualified keys
    match clause {
        WhereClause::Eq(field, lit) => {
            let expected = literal_to_value(lit);
            combined.get(field).map(|v| *v == expected).unwrap_or(false)
        }
        WhereClause::Neq(field, lit) => {
            let expected = literal_to_value(lit);
            combined.get(field).map(|v| *v != expected).unwrap_or(true)
        }
        WhereClause::Gt(field, lit) => {
            let threshold = literal_to_value(lit);
            combined.get(field).map(|v| value_gt(v, &threshold)).unwrap_or(false)
        }
        WhereClause::Lt(field, lit) => {
            let threshold = literal_to_value(lit);
            combined.get(field).map(|v| value_lt(v, &threshold)).unwrap_or(false)
        }
        WhereClause::Gte(field, lit) => {
            let threshold = literal_to_value(lit);
            combined.get(field).map(|v| value_gt(v, &threshold) || v == &threshold).unwrap_or(false)
        }
        WhereClause::Lte(field, lit) => {
            let threshold = literal_to_value(lit);
            combined.get(field).map(|v| value_lt(v, &threshold) || v == &threshold).unwrap_or(false)
        }
        WhereClause::And(l, r) => join_row_matches(combined, l) && join_row_matches(combined, r),
        WhereClause::Or(l, r) => join_row_matches(combined, l) || join_row_matches(combined, r),
        WhereClause::Not(inner) => !join_row_matches(combined, inner),
        WhereClause::IsNull(field) => {
            combined.get(field).map(|v| matches!(v, Value::Null)).unwrap_or(true)
        }
        WhereClause::IsNotNull(field) => {
            combined.get(field).map(|v| !matches!(v, Value::Null)).unwrap_or(false)
        }
        WhereClause::In(field, values) => {
            let fv = combined.get(field);
            values.iter().any(|lit| {
                let expected = literal_to_value(lit);
                fv.map(|v| *v == expected).unwrap_or(false)
            })
        }
        WhereClause::Like(field, pattern) => {
            combined.get(field).and_then(|v| match v {
                Value::String(s) => Some(like_match(s, pattern)),
                _ => None,
            }).unwrap_or(false)
        }
    }
}

fn project_join_row(combined: &HashMap<String, Value>, fields: &trondb_tql::JoinFieldList) -> Row {
    let values = match fields {
        trondb_tql::JoinFieldList::All => combined.clone(),
        trondb_tql::JoinFieldList::Named(qfs) => {
            let mut projected = HashMap::new();
            for qf in qfs {
                let key = format!("{}.{}", qf.alias, qf.field);
                let val = combined.get(&key).cloned().unwrap_or(Value::Null);
                projected.insert(key, val);
            }
            projected
        }
    };
    Row { values, score: None }
}

fn build_join_columns(rows: &[Row], fields: &trondb_tql::JoinFieldList) -> Vec<String> {
    match fields {
        trondb_tql::JoinFieldList::Named(qfs) => {
            qfs.iter().map(|qf| format!("{}.{}", qf.alias, qf.field)).collect()
        }
        trondb_tql::JoinFieldList::All => {
            let mut cols = Vec::new();
            for row in rows {
                for key in row.values.keys() {
                    if !cols.contains(key) {
                        cols.push(key.clone());
                    }
                }
            }
            cols.sort();
            cols
        }
    }
}
```

- [ ] **Step 4: Add import for JoinType**

At the top of executor.rs, update the trondb_tql import:

```rust
use trondb_tql::{FieldList, JoinFieldList, JoinType, Literal, QualifiedField, VectorLiteral, WhereClause};
```

- [ ] **Step 5: Add EXPLAIN arm for Plan::Join**

In the `explain_plan()` function (or wherever EXPLAIN formatting lives), add:

```rust
Plan::Join(p) => {
    props.push(("mode", "Deterministic".into()));
    props.push(("verb", "FETCH JOIN".into()));
    props.push(("from_collection", p.from_collection.clone()));
    props.push(("from_alias", p.from_alias.clone()));
    for (i, join) in p.joins.iter().enumerate() {
        props.push((
            &format!("join_{i}"),
            format!("{:?} {} AS {}", join.join_type, join.collection, join.alias),
        ));
    }
    props.push(("tier", "Fjall".into()));
}
```

- [ ] **Step 6: Write executor integration tests**

```rust
#[tokio::test]
async fn join_inner_structural() {
    let (exec, _dir) = setup_executor().await;

    // Create collections
    exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
        name: "people".into(),
        representations: vec![trondb_tql::RepresentationDecl {
            name: "default".into(),
            model: None,
            dimensions: Some(3),
            metric: trondb_tql::Metric::Cosine,
            sparse: false,
            fields: vec![],
        }],
        fields: vec![
            trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
            trondb_tql::FieldDecl { name: "venue_id".into(), field_type: trondb_tql::FieldType::Text },
        ],
        indexes: vec![],
        vectoriser_config: None,
    })).await.unwrap();

    exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
        name: "venues".into(),
        representations: vec![trondb_tql::RepresentationDecl {
            name: "default".into(),
            model: None,
            dimensions: Some(3),
            metric: trondb_tql::Metric::Cosine,
            sparse: false,
            fields: vec![],
        }],
        fields: vec![
            trondb_tql::FieldDecl { name: "address".into(), field_type: trondb_tql::FieldType::Text },
        ],
        indexes: vec![],
        vectoriser_config: None,
    })).await.unwrap();

    // Insert venue
    exec.execute(&Plan::Insert(InsertPlan {
        collection: "venues".into(),
        fields: vec!["id".into(), "address".into()],
        values: vec![Literal::String("v1".into()), Literal::String("123 Main St".into())],
        vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
        collocate_with: None,
        affinity_group: None,
    })).await.unwrap();

    // Insert person referencing venue
    exec.execute(&Plan::Insert(InsertPlan {
        collection: "people".into(),
        fields: vec!["id".into(), "name".into(), "venue_id".into()],
        values: vec![
            Literal::String("p1".into()),
            Literal::String("Alice".into()),
            Literal::String("v1".into()),
        ],
        vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![0.0, 1.0, 0.0]))],
        collocate_with: None,
        affinity_group: None,
    })).await.unwrap();

    // Execute INNER JOIN
    use trondb_tql::{JoinClause, JoinType, QualifiedField};
    let result = exec.execute(&Plan::Join(JoinPlan {
        fields: trondb_tql::JoinFieldList::Named(vec![
            QualifiedField { alias: "p".into(), field: "name".into() },
            QualifiedField { alias: "v".into(), field: "address".into() },
        ]),
        from_collection: "people".into(),
        from_alias: "p".into(),
        joins: vec![JoinClause {
            join_type: JoinType::Inner,
            collection: "venues".into(),
            alias: "v".into(),
            on_left: QualifiedField { alias: "p".into(), field: "venue_id".into() },
            on_right: QualifiedField { alias: "v".into(), field: "id".into() },
            confidence_threshold: None,
        }],
        filter: None,
        order_by: vec![],
        limit: None,
        hints: vec![],
    })).await.unwrap();

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0].values.get("p.name"),
        Some(&Value::String("Alice".into()))
    );
    assert_eq!(
        result.rows[0].values.get("v.address"),
        Some(&Value::String("123 Main St".into()))
    );
}

#[tokio::test]
async fn join_inner_no_match() {
    let (exec, _dir) = setup_executor().await;

    // Create collections
    exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
        name: "people".into(),
        representations: vec![trondb_tql::RepresentationDecl {
            name: "default".into(),
            model: None,
            dimensions: Some(3),
            metric: trondb_tql::Metric::Cosine,
            sparse: false,
            fields: vec![],
        }],
        fields: vec![
            trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
            trondb_tql::FieldDecl { name: "venue_id".into(), field_type: trondb_tql::FieldType::Text },
        ],
        indexes: vec![],
        vectoriser_config: None,
    })).await.unwrap();

    exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
        name: "venues".into(),
        representations: vec![trondb_tql::RepresentationDecl {
            name: "default".into(),
            model: None,
            dimensions: Some(3),
            metric: trondb_tql::Metric::Cosine,
            sparse: false,
            fields: vec![],
        }],
        fields: vec![],
        indexes: vec![],
        vectoriser_config: None,
    })).await.unwrap();

    // Insert person with non-existent venue_id
    exec.execute(&Plan::Insert(InsertPlan {
        collection: "people".into(),
        fields: vec!["id".into(), "name".into(), "venue_id".into()],
        values: vec![
            Literal::String("p1".into()),
            Literal::String("Alice".into()),
            Literal::String("nonexistent".into()),
        ],
        vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![0.0, 1.0, 0.0]))],
        collocate_with: None,
        affinity_group: None,
    })).await.unwrap();

    // INNER JOIN should return 0 rows (no matching venue)
    use trondb_tql::{JoinClause, JoinType, QualifiedField};
    let result = exec.execute(&Plan::Join(JoinPlan {
        fields: trondb_tql::JoinFieldList::All,
        from_collection: "people".into(),
        from_alias: "p".into(),
        joins: vec![JoinClause {
            join_type: JoinType::Inner,
            collection: "venues".into(),
            alias: "v".into(),
            on_left: QualifiedField { alias: "p".into(), field: "venue_id".into() },
            on_right: QualifiedField { alias: "v".into(), field: "id".into() },
            confidence_threshold: None,
        }],
        filter: None,
        order_by: vec![],
        limit: None,
        hints: vec![],
    })).await.unwrap();

    assert_eq!(result.rows.len(), 0);
}

#[tokio::test]
async fn join_left_includes_unmatched() {
    let (exec, _dir) = setup_executor().await;

    // Create collections (same as above)
    exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
        name: "people".into(),
        representations: vec![trondb_tql::RepresentationDecl {
            name: "default".into(),
            model: None,
            dimensions: Some(3),
            metric: trondb_tql::Metric::Cosine,
            sparse: false,
            fields: vec![],
        }],
        fields: vec![
            trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
            trondb_tql::FieldDecl { name: "venue_id".into(), field_type: trondb_tql::FieldType::Text },
        ],
        indexes: vec![],
        vectoriser_config: None,
    })).await.unwrap();

    exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
        name: "venues".into(),
        representations: vec![trondb_tql::RepresentationDecl {
            name: "default".into(),
            model: None,
            dimensions: Some(3),
            metric: trondb_tql::Metric::Cosine,
            sparse: false,
            fields: vec![],
        }],
        fields: vec![],
        indexes: vec![],
        vectoriser_config: None,
    })).await.unwrap();

    // Insert person with no matching venue
    exec.execute(&Plan::Insert(InsertPlan {
        collection: "people".into(),
        fields: vec!["id".into(), "name".into(), "venue_id".into()],
        values: vec![
            Literal::String("p1".into()),
            Literal::String("Alice".into()),
            Literal::String("nonexistent".into()),
        ],
        vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![0.0, 1.0, 0.0]))],
        collocate_with: None,
        affinity_group: None,
    })).await.unwrap();

    // LEFT JOIN should return 1 row with NULLs for venue fields
    use trondb_tql::{JoinClause, JoinType, QualifiedField};
    let result = exec.execute(&Plan::Join(JoinPlan {
        fields: trondb_tql::JoinFieldList::Named(vec![
            QualifiedField { alias: "p".into(), field: "name".into() },
            QualifiedField { alias: "v".into(), field: "id".into() },
        ]),
        from_collection: "people".into(),
        from_alias: "p".into(),
        joins: vec![JoinClause {
            join_type: JoinType::Left,
            collection: "venues".into(),
            alias: "v".into(),
            on_left: QualifiedField { alias: "p".into(), field: "venue_id".into() },
            on_right: QualifiedField { alias: "v".into(), field: "id".into() },
            confidence_threshold: None,
        }],
        filter: None,
        order_by: vec![],
        limit: None,
        hints: vec![],
    })).await.unwrap();

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].values.get("v.id"), Some(&Value::Null));
}
```

- [ ] **Step 7: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core -- join_inner_structural join_inner_no_match join_left_includes_unmatched`
Expected: PASS.

- [ ] **Step 8: Commit**

`git add crates/trondb-core/src/ && git commit -m "feat(executor): implement JOIN execution — INNER, LEFT, RIGHT, FULL, probabilistic"`

### Task 6: JOIN Proto Messages

**Files:**
- Modify: `crates/trondb-proto/proto/trondb.proto`
- Modify: `crates/trondb-proto/src/convert_plan.rs`

- [ ] **Step 1: Add proto messages**

In `crates/trondb-proto/proto/trondb.proto`, add to the `PlanRequest` oneof:

```protobuf
JoinPlanProto join = 22;
```

Add new message definitions:

```protobuf
message QualifiedFieldProto {
    string alias = 1;
    string field = 2;
}

message JoinClauseProto {
    JoinTypeProto join_type = 1;
    string collection = 2;
    string alias = 3;
    QualifiedFieldProto on_left = 4;
    QualifiedFieldProto on_right = 5;
    optional double confidence_threshold = 6;
}

enum JoinTypeProto {
    JOIN_TYPE_INNER = 0;
    JOIN_TYPE_LEFT = 1;
    JOIN_TYPE_RIGHT = 2;
    JOIN_TYPE_FULL = 3;
}

message JoinFieldListProto {
    bool all = 1;
    repeated QualifiedFieldProto names = 2;
}

message JoinPlanProto {
    JoinFieldListProto fields = 1;
    string from_collection = 2;
    string from_alias = 3;
    repeated JoinClauseProto joins = 4;
    optional WhereClauseProto filter = 5;
    repeated OrderByClauseProto order_by = 6;
    optional uint64 limit = 7;
    repeated string hints = 8;
}
```

- [ ] **Step 2: Add bidirectional conversions**

In `crates/trondb-proto/src/convert_plan.rs`, add conversions for `Plan::Join ↔ JoinPlanProto`. Follow the same pattern as existing conversions.

For `Plan → Proto`:
```rust
Plan::Join(p) => {
    let join_fields = match &p.fields {
        trondb_tql::JoinFieldList::All => pb::JoinFieldListProto {
            all: true,
            names: vec![],
        },
        trondb_tql::JoinFieldList::Named(qfs) => pb::JoinFieldListProto {
            all: false,
            names: qfs.iter().map(|qf| pb::QualifiedFieldProto {
                alias: qf.alias.clone(),
                field: qf.field.clone(),
            }).collect(),
        },
    };

    let joins = p.joins.iter().map(|j| pb::JoinClauseProto {
        join_type: match j.join_type {
            trondb_tql::JoinType::Inner => pb::JoinTypeProto::Inner.into(),
            trondb_tql::JoinType::Left => pb::JoinTypeProto::Left.into(),
            trondb_tql::JoinType::Right => pb::JoinTypeProto::Right.into(),
            trondb_tql::JoinType::Full => pb::JoinTypeProto::Full.into(),
        },
        collection: j.collection.clone(),
        alias: j.alias.clone(),
        on_left: Some(pb::QualifiedFieldProto {
            alias: j.on_left.alias.clone(),
            field: j.on_left.field.clone(),
        }),
        on_right: Some(pb::QualifiedFieldProto {
            alias: j.on_right.alias.clone(),
            field: j.on_right.field.clone(),
        }),
        confidence_threshold: j.confidence_threshold,
    }).collect();

    pb::plan_request::Plan::Join(pb::JoinPlanProto {
        fields: Some(join_fields),
        from_collection: p.from_collection.clone(),
        from_alias: p.from_alias.clone(),
        joins,
        filter: p.filter.as_ref().map(where_clause_to_proto),
        order_by: p.order_by.iter().map(order_by_to_proto).collect(),
        limit: p.limit.map(|l| l as u64),
        hints: p.hints.iter().map(hint_to_string).collect(),
    })
}
```

For `Proto → Plan`: reverse the mapping following the same pattern.

- [ ] **Step 3: Run proto compilation + tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-proto`
Expected: PASS.

- [ ] **Step 4: Commit**

`git add crates/trondb-proto/ && git commit -m "feat(proto): add JoinPlanProto messages and bidirectional conversions"`

---

## Chunk 4: TRAVERSE MATCH Tokens + AST (trondb-tql)

Adds tokens and AST types for the Cypher-inspired MATCH pattern syntax.

### Task 7: New Tokens for TRAVERSE MATCH

**Files:**
- Modify: `crates/trondb-tql/src/token.rs`

- [ ] **Step 1: Write failing tests**

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- lex_match_keyword lex_arrow_tokens lex_dotdot_token lex_traverse_match_full`
Expected: FAIL — `Token::Match`, `Token::Dash`, `Token::Arrow`, `Token::DotDot` don't exist.

- [ ] **Step 3: Add new tokens**

In `crates/trondb-tql/src/token.rs`, add:

Keywords (before `// Identifiers`):
```rust
#[token("MATCH", priority = 10, ignore(ascii_case))]
Match,
```

Symbols (in the symbols section — **order matters**: multi-char before single-char):
```rust
#[token("->")]
Arrow,

#[token("..")]
DotDot,

#[token("-")]
Dash,
```

**IMPORTANT:** `Arrow` (`->`) and `DotDot` (`..`) must be declared BEFORE `Dash` (`-`) and `Dot` (`.`) to ensure longest-match. logos handles this via priority/ordering, but to be safe, place `Arrow` before `Dash` and `DotDot` before `Dot` in the enum definition.

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- lex_match_keyword lex_arrow_tokens lex_dotdot_token lex_traverse_match_full`
Expected: PASS.

- [ ] **Step 5: Verify no regressions (negative float literals)**

The `-` in `-42` (negative int literal) is part of the IntLit regex `r"-?[0-9]+"`, so `Token::Dash` shouldn't interfere. But verify:

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql`
Expected: PASS — all existing tests still pass.

**Potential issue:** If the `-` before a number gets lexed as `Dash` instead of part of IntLit, existing tests will fail. The IntLit regex has priority 2 and `Dash` has default priority. logos should prefer the longer match (IntLit includes the minus). If tests fail, increase IntLit priority to 4.

- [ ] **Step 6: Commit**

`git add crates/trondb-tql/src/token.rs && git commit -m "feat(tql): add MATCH, Arrow, DotDot, Dash tokens for TRAVERSE MATCH"`

### Task 8: AST Types for TRAVERSE MATCH

**Files:**
- Modify: `crates/trondb-tql/src/ast.rs`

- [ ] **Step 1: Add TRAVERSE MATCH AST types**

In `crates/trondb-tql/src/ast.rs`, add after the JOIN types:

```rust
// --- TRAVERSE MATCH ---

/// Direction of an edge pattern in a MATCH clause
#[derive(Debug, Clone, PartialEq)]
pub enum EdgeDirection {
    /// `(a)-[e:TYPE]->(b)` — left to right
    Forward,
    /// `(a)<-[e:TYPE]-(b)` — right to left
    Backward,
    /// `(a)-[e:TYPE]-(b)` — either direction
    Undirected,
}

/// An edge pattern: `-[variable:TYPE]->` or `-[variable:TYPE]-` etc.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgePattern {
    /// Optional variable binding for the edge (e.g., `e` in `-[e:TYPE]->`)
    pub variable: Option<String>,
    /// Optional edge type filter (e.g., `RELATED_TO`)
    pub edge_type: Option<String>,
    /// Direction of the edge
    pub direction: EdgeDirection,
}

/// A MATCH pattern: `(a)-[e:TYPE]->(b)`
#[derive(Debug, Clone, PartialEq)]
pub struct MatchPattern {
    pub source_var: String,
    pub edge: EdgePattern,
    pub target_var: String,
}

/// TRAVERSE ... MATCH ... statement
#[derive(Debug, Clone, PartialEq)]
pub struct TraverseMatchStmt {
    pub from_id: String,
    pub pattern: MatchPattern,
    pub min_depth: usize,
    pub max_depth: usize,
    pub confidence_threshold: Option<f64>,
    pub limit: Option<usize>,
}
```

- [ ] **Step 2: Add TraverseMatch variant to Statement enum**

```rust
TraverseMatch(TraverseMatchStmt),
```

- [ ] **Step 3: Run compilation check**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo check -p trondb-tql`
Expected: PASS.

- [ ] **Step 4: Commit**

`git add crates/trondb-tql/src/ast.rs && git commit -m "feat(tql): add TRAVERSE MATCH AST types — MatchPattern, EdgePattern, EdgeDirection"`

---

## Chunk 5: TRAVERSE MATCH Parser (trondb-tql)

### Task 9: Parse TRAVERSE MATCH Syntax

**Files:**
- Modify: `crates/trondb-tql/src/parser.rs`

- [ ] **Step 1: Write failing parser tests**

```rust
#[test]
fn parse_traverse_match_forward() {
    let stmt = parse(
        "TRAVERSE FROM 'ent_abc123' MATCH (a)-[e:RELATED_TO]->(b) DEPTH 1..3 CONFIDENCE > 0.70;"
    ).unwrap();
    match stmt {
        Statement::TraverseMatch(t) => {
            assert_eq!(t.from_id, "ent_abc123");
            assert_eq!(t.pattern.source_var, "a");
            assert_eq!(t.pattern.target_var, "b");
            assert_eq!(t.pattern.edge.variable, Some("e".into()));
            assert_eq!(t.pattern.edge.edge_type, Some("RELATED_TO".into()));
            assert_eq!(t.pattern.edge.direction, EdgeDirection::Forward);
            assert_eq!(t.min_depth, 1);
            assert_eq!(t.max_depth, 3);
            assert_eq!(t.confidence_threshold, Some(0.70));
        }
        _ => panic!("expected TraverseMatch"),
    }
}

#[test]
fn parse_traverse_match_undirected() {
    let stmt = parse(
        "TRAVERSE FROM 'x' MATCH (a)-[e:KNOWS]-(b) DEPTH 1..2;"
    ).unwrap();
    match stmt {
        Statement::TraverseMatch(t) => {
            assert_eq!(t.pattern.edge.direction, EdgeDirection::Undirected);
            assert_eq!(t.pattern.edge.edge_type, Some("KNOWS".into()));
            assert_eq!(t.min_depth, 1);
            assert_eq!(t.max_depth, 2);
            assert!(t.confidence_threshold.is_none());
        }
        _ => panic!("expected TraverseMatch"),
    }
}

#[test]
fn parse_traverse_match_no_edge_type() {
    let stmt = parse(
        "TRAVERSE FROM 'x' MATCH (a)-[e]->(b) DEPTH 1..5;"
    ).unwrap();
    match stmt {
        Statement::TraverseMatch(t) => {
            assert_eq!(t.pattern.edge.variable, Some("e".into()));
            assert!(t.pattern.edge.edge_type.is_none());
        }
        _ => panic!("expected TraverseMatch"),
    }
}

#[test]
fn parse_traverse_match_with_limit() {
    let stmt = parse(
        "TRAVERSE FROM 'x' MATCH (a)-[e:KNOWS]->(b) DEPTH 1..3 LIMIT 10;"
    ).unwrap();
    match stmt {
        Statement::TraverseMatch(t) => {
            assert_eq!(t.limit, Some(10));
        }
        _ => panic!("expected TraverseMatch"),
    }
}

#[test]
fn parse_old_traverse_still_works() {
    // Existing TRAVERSE syntax must continue to work
    let stmt = parse("TRAVERSE knows FROM 'v1' DEPTH 3;").unwrap();
    match stmt {
        Statement::Traverse(t) => {
            assert_eq!(t.edge_type, "knows");
            assert_eq!(t.from_id, "v1");
            assert_eq!(t.depth, 3);
        }
        _ => panic!("expected Traverse (legacy)"),
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- parse_traverse_match_forward parse_traverse_match_undirected parse_traverse_match_no_edge_type parse_traverse_match_with_limit parse_old_traverse_still_works`
Expected: FAIL for the new tests; the old one should pass.

- [ ] **Step 3: Modify parse_traverse to handle MATCH syntax**

Replace the existing `parse_traverse` method:

```rust
fn parse_traverse(&mut self) -> Result<Statement, ParseError> {
    self.advance(); // TRAVERSE

    // Detect TRAVERSE MATCH vs legacy TRAVERSE
    // Legacy: TRAVERSE edge_type FROM 'id' ...
    // New:    TRAVERSE FROM 'id' MATCH (a)-[e:TYPE]->(b) ...
    if self.peek() == Some(&Token::From) {
        // New TRAVERSE MATCH syntax
        return self.parse_traverse_match();
    }

    // Legacy syntax: TRAVERSE edge_type FROM 'id' [DEPTH n] [LIMIT n];
    let edge_type = self.expect_ident()?;
    self.expect(&Token::From)?;
    let from_id = self.expect_string_lit()?;

    let depth = if self.peek() == Some(&Token::Depth) {
        self.advance();
        self.expect_int()? as usize
    } else {
        1
    };

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

fn parse_traverse_match(&mut self) -> Result<Statement, ParseError> {
    // Already consumed: TRAVERSE
    // Now: FROM 'id' MATCH (a)-[e:TYPE]->(b) DEPTH min..max [CONFIDENCE > threshold] [LIMIT n];
    self.expect(&Token::From)?;
    let from_id = self.expect_string_lit()?;
    self.expect(&Token::Match)?;

    // Parse pattern: (source_var) edge_pattern (target_var)
    // (a)-[e:TYPE]->(b)  or  (a)-[e:TYPE]-(b)  or  (a)<-[e:TYPE]-(b)
    self.expect(&Token::LParen)?;
    let source_var = self.expect_ident()?;
    self.expect(&Token::RParen)?;

    // Parse edge pattern
    // Expect: - [ var : TYPE ] -> or - [ var : TYPE ] - or <- [ var : TYPE ] -
    // For forward: -[...]->(target)
    // For undirected: -[...]-(target)
    // For backward: <-[...]-(target)  (future — for now support forward + undirected)

    self.expect(&Token::Dash)?;
    self.expect(&Token::LBracket)?;

    // Optional variable and/or edge type
    let mut variable = None;
    let mut edge_type = None;

    if let Some(Token::Ident(_)) = self.peek() {
        let ident = self.expect_ident()?;
        if self.peek() == Some(&Token::Colon) {
            // variable:TYPE
            self.advance(); // :
            variable = Some(ident);
            edge_type = Some(self.expect_ident()?);
        } else {
            // Just a variable, no type
            variable = Some(ident);
        }
    }

    self.expect(&Token::RBracket)?;

    // Determine direction: -> (forward) or - (undirected)
    let direction = if self.peek() == Some(&Token::Arrow) {
        self.advance();
        EdgeDirection::Forward
    } else if self.peek() == Some(&Token::Dash) {
        self.advance();
        EdgeDirection::Undirected
    } else {
        return Err(ParseError::InvalidQuery(
            "expected -> or - after edge pattern".into(),
        ));
    };

    self.expect(&Token::LParen)?;
    let target_var = self.expect_ident()?;
    self.expect(&Token::RParen)?;

    // DEPTH min..max
    self.expect(&Token::Depth)?;
    let min_depth = self.expect_int()? as usize;
    self.expect(&Token::DotDot)?;
    let max_depth = self.expect_int()? as usize;

    // Optional CONFIDENCE > threshold
    let confidence_threshold = if self.peek() == Some(&Token::Confidence) {
        self.advance();
        self.expect(&Token::Gt)?;
        Some(self.expect_float_or_int()?)
    } else {
        None
    };

    // Optional LIMIT
    let limit = if self.peek() == Some(&Token::Limit) {
        self.advance();
        Some(self.expect_int()? as usize)
    } else {
        None
    };

    self.expect(&Token::Semicolon)?;

    Ok(Statement::TraverseMatch(TraverseMatchStmt {
        from_id,
        pattern: MatchPattern {
            source_var,
            edge: EdgePattern {
                variable,
                edge_type,
                direction,
            },
            target_var,
        },
        min_depth,
        max_depth,
        confidence_threshold,
        limit,
    }))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- parse_traverse_match_forward parse_traverse_match_undirected parse_traverse_match_no_edge_type parse_traverse_match_with_limit parse_old_traverse_still_works`
Expected: PASS.

- [ ] **Step 5: Run full tql test suite**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql`
Expected: PASS.

- [ ] **Step 6: Commit**

`git add crates/trondb-tql/src/ && git commit -m "feat(tql): parse TRAVERSE MATCH syntax with edge patterns and depth ranges"`

---

## Chunk 6: TRAVERSE MATCH Planner + Executor + Proto

### Task 10: TraverseMatch Plan Type + Planner

**Files:**
- Modify: `crates/trondb-core/src/planner.rs`

- [ ] **Step 1: Add TraverseMatchPlan**

```rust
#[derive(Debug, Clone)]
pub struct TraverseMatchPlan {
    pub from_id: String,
    pub pattern: trondb_tql::MatchPattern,
    pub min_depth: usize,
    pub max_depth: usize,
    pub confidence_threshold: Option<f64>,
    pub limit: Option<usize>,
}
```

- [ ] **Step 2: Add Plan::TraverseMatch variant**

```rust
TraverseMatch(TraverseMatchPlan),
```

- [ ] **Step 3: Add planner arm**

```rust
Statement::TraverseMatch(s) => Ok(Plan::TraverseMatch(TraverseMatchPlan {
    from_id: s.from_id.clone(),
    pattern: s.pattern.clone(),
    min_depth: s.min_depth,
    max_depth: s.max_depth,
    confidence_threshold: s.confidence_threshold,
    limit: s.limit,
})),
```

- [ ] **Step 4: Add planner test**

```rust
#[test]
fn plan_traverse_match() {
    use trondb_tql::{TraverseMatchStmt, MatchPattern, EdgePattern, EdgeDirection};

    let stmt = Statement::TraverseMatch(TraverseMatchStmt {
        from_id: "ent_abc".into(),
        pattern: MatchPattern {
            source_var: "a".into(),
            edge: EdgePattern {
                variable: Some("e".into()),
                edge_type: Some("RELATED_TO".into()),
                direction: EdgeDirection::Forward,
            },
            target_var: "b".into(),
        },
        min_depth: 1,
        max_depth: 3,
        confidence_threshold: Some(0.70),
        limit: None,
    });
    let p = plan(&stmt, &empty_schemas()).unwrap();
    match p {
        Plan::TraverseMatch(tp) => {
            assert_eq!(tp.from_id, "ent_abc");
            assert_eq!(tp.min_depth, 1);
            assert_eq!(tp.max_depth, 3);
            assert_eq!(tp.confidence_threshold, Some(0.70));
        }
        _ => panic!("expected TraverseMatchPlan"),
    }
}
```

- [ ] **Step 5: Run test**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core -- plan_traverse_match`
Expected: PASS.

- [ ] **Step 6: Commit**

`git add crates/trondb-core/src/planner.rs && git commit -m "feat(planner): add TraverseMatchPlan type and planner arm"`

### Task 11: TraverseMatch Executor

**Files:**
- Modify: `crates/trondb-core/src/executor.rs`

- [ ] **Step 1: Add Plan::TraverseMatch execution arm**

The algorithm extends the existing BFS traversal with:
- Direction-aware edge lookup (forward, backward, or both)
- Optional edge type filter
- Variable depth range (min..max)
- Confidence threshold filtering

```rust
Plan::TraverseMatch(p) => {
    let max_depth = p.max_depth.min(10); // cap at 10
    let min_depth = p.min_depth;
    let confidence_threshold = p.confidence_threshold.unwrap_or(0.0) as f32;

    let from_id = LogicalId::from_string(&p.from_id);
    let mut visited: HashSet<LogicalId> = HashSet::new();
    visited.insert(from_id.clone());

    let mut frontier = vec![from_id];
    let mut rows = Vec::new();
    let limit = p.limit.unwrap_or(usize::MAX);

    // Collect matching edge type names
    let edge_type_filter: Option<String> = p.pattern.edge.edge_type.clone();

    for hop in 0..max_depth {
        if frontier.is_empty() || rows.len() >= limit {
            break;
        }

        let mut next_frontier = Vec::new();
        let include_at_depth = hop + 1 >= min_depth;

        for node_id in &frontier {
            // Collect neighbor entries based on direction
            let mut neighbors: Vec<(LogicalId, f32, String)> = Vec::new(); // (to_id, confidence, collection)

            // Get all edge types (or just the filtered one)
            let edge_types_to_check: Vec<EdgeType> = if let Some(ref et_name) = edge_type_filter {
                match self.store.get_edge_type(et_name) {
                    Ok(et) => vec![et],
                    Err(_) => vec![],
                }
            } else {
                // All edge types
                self.edge_types.iter().map(|e| e.value().clone()).collect()
            };

            for et in &edge_types_to_check {
                match p.pattern.edge.direction {
                    trondb_tql::EdgeDirection::Forward => {
                        let entries = self.adjacency.get(node_id, &et.name);
                        for entry in &entries {
                            let effective_conf = if entry.created_at > 0 {
                                let now_ms = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap()
                                    .as_millis() as u64;
                                let elapsed = now_ms.saturating_sub(entry.created_at);
                                crate::edge::effective_confidence(entry.confidence, elapsed, &et.decay_config)
                            } else {
                                entry.confidence
                            };
                            if effective_conf >= confidence_threshold {
                                neighbors.push((entry.to_id.clone(), effective_conf, et.to_collection.clone()));
                            }
                        }
                    }
                    trondb_tql::EdgeDirection::Backward => {
                        let source_ids = self.adjacency.get_backward(node_id, &et.name);
                        for source_id in &source_ids {
                            // Get the actual edge entry for confidence
                            let entries = self.adjacency.get(source_id, &et.name);
                            for entry in &entries {
                                if entry.to_id == *node_id {
                                    let effective_conf = if entry.created_at > 0 {
                                        let now_ms = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap()
                                            .as_millis() as u64;
                                        let elapsed = now_ms.saturating_sub(entry.created_at);
                                        crate::edge::effective_confidence(entry.confidence, elapsed, &et.decay_config)
                                    } else {
                                        entry.confidence
                                    };
                                    if effective_conf >= confidence_threshold {
                                        neighbors.push((source_id.clone(), effective_conf, et.from_collection.clone()));
                                    }
                                }
                            }
                        }
                    }
                    trondb_tql::EdgeDirection::Undirected => {
                        // Forward
                        let entries = self.adjacency.get(node_id, &et.name);
                        for entry in &entries {
                            let effective_conf = entry.confidence; // simplified
                            if effective_conf >= confidence_threshold {
                                neighbors.push((entry.to_id.clone(), effective_conf, et.to_collection.clone()));
                            }
                        }
                        // Backward
                        let source_ids = self.adjacency.get_backward(node_id, &et.name);
                        for source_id in &source_ids {
                            let entries = self.adjacency.get(source_id, &et.name);
                            for entry in &entries {
                                if entry.to_id == *node_id && entry.confidence >= confidence_threshold {
                                    neighbors.push((source_id.clone(), entry.confidence, et.from_collection.clone()));
                                }
                            }
                        }
                    }
                }
            }

            for (neighbor_id, confidence, collection) in &neighbors {
                if visited.contains(neighbor_id) {
                    continue;
                }
                visited.insert(neighbor_id.clone());

                if include_at_depth {
                    if let Ok(entity) = self.store.get(collection, neighbor_id) {
                        let mut row = entity_to_row(&entity, &FieldList::All);
                        // Add _edge.confidence and _edge.type to the row
                        row.values.insert("_edge.confidence".into(), Value::Float(confidence as f64));
                        if let Some(ref et_name) = edge_type_filter {
                            row.values.insert("_edge.type".into(), Value::String(et_name.clone()));
                        }
                        row.values.insert("_depth".into(), Value::Int((hop + 1) as i64));
                        rows.push(row);
                        if rows.len() >= limit {
                            break;
                        }
                    }
                }
                next_frontier.push(neighbor_id.clone());
            }
            if rows.len() >= limit {
                break;
            }
        }

        frontier = next_frontier;
    }

    let scanned = rows.len();

    Ok(QueryResult {
        columns: build_columns(&rows, &FieldList::All),
        rows,
        stats: QueryStats {
            elapsed: start.elapsed(),
            entities_scanned: scanned,
            mode: QueryMode::Deterministic,
            tier: "Ram".into(),
        },
    })
}
```

- [ ] **Step 2: Add EXPLAIN arm**

```rust
Plan::TraverseMatch(p) => {
    props.push(("mode", "Deterministic".into()));
    props.push(("verb", "TRAVERSE MATCH".into()));
    props.push(("from_id", p.from_id.clone()));
    if let Some(ref et) = p.pattern.edge.edge_type {
        props.push(("edge_type", et.clone()));
    }
    props.push(("direction", format!("{:?}", p.pattern.edge.direction)));
    props.push(("depth", format!("{}..{}", p.min_depth, p.max_depth)));
    if let Some(conf) = p.confidence_threshold {
        props.push(("confidence_threshold", format!("{conf}")));
    }
    props.push(("tier", "Ram".into()));
}
```

- [ ] **Step 3: Write executor integration tests**

```rust
#[tokio::test]
async fn traverse_match_forward() {
    let (exec, _dir) = setup_executor().await;

    // Create people collection
    exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
        name: "people".into(),
        representations: vec![trondb_tql::RepresentationDecl {
            name: "default".into(),
            model: None,
            dimensions: Some(3),
            metric: trondb_tql::Metric::Cosine,
            sparse: false,
            fields: vec![],
        }],
        fields: vec![
            trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
        ],
        indexes: vec![],
        vectoriser_config: None,
    })).await.unwrap();

    // Insert people a -> b -> c
    for (id, name) in &[("a", "Alice"), ("b", "Bob"), ("c", "Charlie")] {
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "people".into(),
            fields: vec!["id".into(), "name".into()],
            values: vec![Literal::String(id.to_string()), Literal::String(name.to_string())],
            vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
            collocate_with: None,
            affinity_group: None,
        })).await.unwrap();
    }

    // Create edge type
    exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
        name: "knows".into(),
        from_collection: "people".into(),
        to_collection: "people".into(),
        decay_config: None,
        inference_config: None,
    })).await.unwrap();

    // Insert edges: a->b, b->c
    exec.execute(&Plan::InsertEdge(InsertEdgePlan {
        edge_type: "knows".into(),
        from_id: "a".into(),
        to_id: "b".into(),
        metadata: vec![],
    })).await.unwrap();
    exec.execute(&Plan::InsertEdge(InsertEdgePlan {
        edge_type: "knows".into(),
        from_id: "b".into(),
        to_id: "c".into(),
        metadata: vec![],
    })).await.unwrap();

    // TRAVERSE MATCH from a, depth 1..2 — should get b (depth 1) and c (depth 2)
    use trondb_tql::{MatchPattern, EdgePattern, EdgeDirection};
    let result = exec.execute(&Plan::TraverseMatch(TraverseMatchPlan {
        from_id: "a".into(),
        pattern: MatchPattern {
            source_var: "a".into(),
            edge: EdgePattern {
                variable: Some("e".into()),
                edge_type: Some("knows".into()),
                direction: EdgeDirection::Forward,
            },
            target_var: "b".into(),
        },
        min_depth: 1,
        max_depth: 2,
        confidence_threshold: None,
        limit: None,
    })).await.unwrap();

    assert_eq!(result.rows.len(), 2);
    // Both should have _edge.confidence and _depth
    for row in &result.rows {
        assert!(row.values.contains_key("_edge.confidence"));
        assert!(row.values.contains_key("_depth"));
    }
}

#[tokio::test]
async fn traverse_match_min_depth_filter() {
    let (exec, _dir) = setup_executor().await;

    // Same setup: a -> b -> c
    exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
        name: "people".into(),
        representations: vec![trondb_tql::RepresentationDecl {
            name: "default".into(),
            model: None,
            dimensions: Some(3),
            metric: trondb_tql::Metric::Cosine,
            sparse: false,
            fields: vec![],
        }],
        fields: vec![
            trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
        ],
        indexes: vec![],
        vectoriser_config: None,
    })).await.unwrap();

    for (id, name) in &[("a", "Alice"), ("b", "Bob"), ("c", "Charlie")] {
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "people".into(),
            fields: vec!["id".into(), "name".into()],
            values: vec![Literal::String(id.to_string()), Literal::String(name.to_string())],
            vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
            collocate_with: None,
            affinity_group: None,
        })).await.unwrap();
    }

    exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
        name: "knows".into(),
        from_collection: "people".into(),
        to_collection: "people".into(),
        decay_config: None,
        inference_config: None,
    })).await.unwrap();

    exec.execute(&Plan::InsertEdge(InsertEdgePlan {
        edge_type: "knows".into(), from_id: "a".into(), to_id: "b".into(), metadata: vec![],
    })).await.unwrap();
    exec.execute(&Plan::InsertEdge(InsertEdgePlan {
        edge_type: "knows".into(), from_id: "b".into(), to_id: "c".into(), metadata: vec![],
    })).await.unwrap();

    // TRAVERSE MATCH from a, depth 2..3 — should only get c (depth 2), not b (depth 1)
    use trondb_tql::{MatchPattern, EdgePattern, EdgeDirection};
    let result = exec.execute(&Plan::TraverseMatch(TraverseMatchPlan {
        from_id: "a".into(),
        pattern: MatchPattern {
            source_var: "a".into(),
            edge: EdgePattern {
                variable: Some("e".into()),
                edge_type: Some("knows".into()),
                direction: EdgeDirection::Forward,
            },
            target_var: "b".into(),
        },
        min_depth: 2,
        max_depth: 3,
        confidence_threshold: None,
        limit: None,
    })).await.unwrap();

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0].values.get("name"),
        Some(&Value::String("Charlie".into()))
    );
}
```

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core -- traverse_match_forward traverse_match_min_depth_filter`
Expected: PASS.

- [ ] **Step 5: Commit**

`git add crates/trondb-core/src/ && git commit -m "feat(executor): implement TRAVERSE MATCH with direction, depth range, confidence filter"`

### Task 12: TraverseMatch Proto Messages

**Files:**
- Modify: `crates/trondb-proto/proto/trondb.proto`
- Modify: `crates/trondb-proto/src/convert_plan.rs`

- [ ] **Step 1: Add proto messages**

In `crates/trondb-proto/proto/trondb.proto`, add to `PlanRequest` oneof:

```protobuf
TraverseMatchPlanProto traverse_match = 23;
```

Add new message definitions:

```protobuf
enum EdgeDirectionProto {
    EDGE_DIRECTION_FORWARD = 0;
    EDGE_DIRECTION_BACKWARD = 1;
    EDGE_DIRECTION_UNDIRECTED = 2;
}

message EdgePatternProto {
    optional string variable = 1;
    optional string edge_type = 2;
    EdgeDirectionProto direction = 3;
}

message MatchPatternProto {
    string source_var = 1;
    EdgePatternProto edge = 2;
    string target_var = 3;
}

message TraverseMatchPlanProto {
    string from_id = 1;
    MatchPatternProto pattern = 2;
    uint64 min_depth = 3;
    uint64 max_depth = 4;
    optional double confidence_threshold = 5;
    optional uint64 limit = 6;
}
```

- [ ] **Step 2: Add bidirectional conversions**

In `crates/trondb-proto/src/convert_plan.rs`, add conversions for `Plan::TraverseMatch ↔ TraverseMatchPlanProto`.

- [ ] **Step 3: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-proto`
Expected: PASS.

- [ ] **Step 4: Commit**

`git add crates/trondb-proto/ && git commit -m "feat(proto): add TraverseMatchPlanProto messages and bidirectional conversions"`

---

## Chunk 7: Full Integration Tests + CLAUDE.md

### Task 13: End-to-End Integration Tests

**Files:**
- Modify: `crates/trondb-core/src/executor.rs` (test section)

- [ ] **Step 1: Write end-to-end parse → plan → execute tests**

These tests go through the full pipeline: TQL string → parse → plan → execute.

```rust
#[tokio::test]
async fn e2e_inner_join() {
    let (exec, _dir) = setup_executor().await;

    // Setup: create collections, insert data (using Plan directly for setup)
    // ... (same setup as join_inner_structural above)

    // Parse the TQL
    let stmt = trondb_tql::parse(
        "FETCH p.name, v.address FROM people AS p INNER JOIN venues AS v ON p.venue_id = v.id;"
    ).unwrap();

    // Plan it
    let p = crate::planner::plan(&stmt, &exec.schemas).unwrap();
    assert!(matches!(p, Plan::Join(_)));

    // Execute
    let result = exec.execute(&p).await.unwrap();
    assert_eq!(result.rows.len(), 1);
}

#[tokio::test]
async fn e2e_traverse_match() {
    let (exec, _dir) = setup_executor().await;

    // Setup: create collection, insert entities, create edges
    // ... (same setup as traverse_match_forward above)

    // Parse the TQL
    let stmt = trondb_tql::parse(
        "TRAVERSE FROM 'a' MATCH (x)-[e:knows]->(y) DEPTH 1..2;"
    ).unwrap();

    let p = crate::planner::plan(&stmt, &exec.schemas).unwrap();
    assert!(matches!(p, Plan::TraverseMatch(_)));

    let result = exec.execute(&p).await.unwrap();
    assert_eq!(result.rows.len(), 2);
}
```

- [ ] **Step 2: Run full workspace tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace`
Expected: PASS (ignore known flaky `similarity_score_range`).

- [ ] **Step 3: Commit**

`git add crates/trondb-core/src/executor.rs && git commit -m "test: add end-to-end integration tests for JOIN and TRAVERSE MATCH"`

### Task 14: Update CLAUDE.md

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add JOIN documentation to CLAUDE.md**

Under the `Query Language Completions (Phase 12)` section, add:

```markdown
- JOINs: structural and probabilistic joins across edge-linked collections
  - Syntax: FETCH alias.field, ... FROM collection AS alias [INNER|LEFT|RIGHT|FULL] JOIN collection AS alias ON alias.field = alias.field [CONFIDENCE > threshold]
  - Join types: INNER (default), LEFT, RIGHT, FULL
  - Structural joins: field-value matching (e.g., venue_id = id)
  - Probabilistic joins: edge-based with CONFIDENCE threshold, adds _edge.confidence to results
  - Qualified field references: alias.field in SELECT, ON, WHERE clauses
  - Row keys are alias-qualified: "e.name", "v.address", "_edge.confidence"
  - WHERE filter applied to alias-qualified combined row
  - New AST types: FetchJoinStmt, JoinClause, JoinFieldList, QualifiedField, JoinType
  - New Plan type: Plan::Join(JoinPlan)
  - New tokens: Join, Inner, Left, Right, Full, As, Dot
```

- [ ] **Step 2: Add TRAVERSE MATCH documentation**

```markdown
- TRAVERSE MATCH: Cypher-inspired pattern matching for graph traversal
  - Syntax: TRAVERSE FROM 'id' MATCH (a)-[e:TYPE]->(b) DEPTH min..max [CONFIDENCE > threshold] [LIMIT n]
  - Edge patterns: (a)-[e:TYPE]->(b) forward, (a)-[e:TYPE]-(b) undirected
  - Optional edge variable binding and edge type filter
  - Variable depth range: min..max (capped at 10)
  - Confidence threshold filters low-confidence edges
  - Results include _edge.confidence, _edge.type, _depth metadata
  - Legacy TRAVERSE syntax still supported (no MATCH keyword)
  - New AST types: TraverseMatchStmt, MatchPattern, EdgePattern, EdgeDirection
  - New Plan type: Plan::TraverseMatch(TraverseMatchPlan)
  - New tokens: Match, Arrow (->), DotDot (..), Dash (-)
```

- [ ] **Step 3: Commit**

`git add CLAUDE.md && git commit -m "docs: document JOIN and TRAVERSE MATCH features in CLAUDE.md"`

---

## Summary of Commit Sequence

1. `feat(tql): add JOIN, AS, Dot tokens for Phase 12b`
2. `feat(tql): add JOIN AST types — JoinClause, FetchJoinStmt, QualifiedField`
3. `feat(tql): parse structural and probabilistic JOIN syntax`
4. `feat(planner): add JoinPlan type and planner arm`
5. `feat(executor): implement JOIN execution — INNER, LEFT, RIGHT, FULL, probabilistic`
6. `feat(proto): add JoinPlanProto messages and bidirectional conversions`
7. `feat(tql): add MATCH, Arrow, DotDot, Dash tokens for TRAVERSE MATCH`
8. `feat(tql): add TRAVERSE MATCH AST types — MatchPattern, EdgePattern, EdgeDirection`
9. `feat(tql): parse TRAVERSE MATCH syntax with edge patterns and depth ranges`
10. `feat(planner): add TraverseMatchPlan type and planner arm`
11. `feat(executor): implement TRAVERSE MATCH with direction, depth range, confidence filter`
12. `feat(proto): add TraverseMatchPlanProto messages and bidirectional conversions`
13. `test: add end-to-end integration tests for JOIN and TRAVERSE MATCH`
14. `docs: document JOIN and TRAVERSE MATCH features in CLAUDE.md`

## Design Decisions and Notes

### JOIN Design
- **Not a relational JOIN.** TronDB JOINs follow edges between collections, not arbitrary table cross-products. The ON clause matches entity field values (structural) or edge existence (probabilistic).
- **Alias-qualified keys.** Row values use `alias.field` keys (e.g., `p.name`, `v.address`) to disambiguate fields from different collections. This is a departure from the flat `HashMap<String, Value>` used by regular FETCH.
- **WHERE in JOIN context.** The WHERE clause field names must also be alias-qualified (e.g., `e.type = 'event'`). The `join_row_matches` helper handles this by looking up alias-qualified keys in the combined row.
- **Probabilistic JOIN efficiency.** For probabilistic joins, we iterate all edge types between the two collections. This is O(edge_types * edges). For large graphs, a future optimization would be to index edges by collection pair.
- **Full scan for right-side matching.** When the ON right-hand field is not `id`, we scan the right collection. A future optimization: use field indexes if available.

### TRAVERSE MATCH Design
- **Backward compatibility.** The legacy `TRAVERSE edge_type FROM 'id' DEPTH n` syntax is preserved. The new MATCH syntax is detected by the presence of `FROM` immediately after `TRAVERSE` (legacy has `edge_type` first).
- **Direction awareness.** Forward uses the forward adjacency index, backward uses the backward index, undirected checks both.
- **Depth range.** `DEPTH 1..3` means include results from depth 1, 2, and 3. The BFS always traverses to max_depth, but only includes results at `>= min_depth`.
- **Result metadata.** Each result row includes `_edge.confidence`, `_edge.type`, and `_depth` for observability.
- **Max depth cap.** Same as existing TRAVERSE: capped at 10 to prevent runaway traversals.
