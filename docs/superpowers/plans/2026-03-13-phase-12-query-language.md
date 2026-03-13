# Phase 12: Query Language Completions — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fill the gaps between TronDB's current TQL and the grammar specification — ORDER BY, advanced WHERE operators, DROP statements, and query hints.

**Architecture:** Each feature follows the established pipeline: new tokens → AST nodes → parser functions → planner plan types → executor arms. All features are additive — no existing behavior changes. Proto/gRPC extensions added for each new plan type to maintain multi-node parity.

**Tech Stack:** Rust 2021, logos lexer, recursive descent parser, Fjall storage, DashMap indexes, tonic/protobuf gRPC.

**Build command:** `CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace`

**Scope note:** JOINs and TRAVERSE MATCH are deferred — they're large enough to warrant their own spec+plan cycles. This plan covers the four simpler sub-features.

---

## File Structure

| File | Role | Tasks |
|------|------|-------|
| `crates/trondb-tql/src/token.rs` | New keyword tokens | 1, 4, 7, 10 |
| `crates/trondb-tql/src/ast.rs` | New AST node types + WhereClause variants | 1, 4, 7, 10 |
| `crates/trondb-tql/src/parser.rs` | New parser functions | 2, 5, 8, 11 |
| `crates/trondb-core/src/planner.rs` | New plan types + strategy selection | 3, 6, 9, 12 |
| `crates/trondb-core/src/executor.rs` | New execution arms + filter eval | 3, 6, 9, 12 |
| `crates/trondb-core/src/error.rs` | New error variants (if needed) | 9 |
| `crates/trondb-core/src/store.rs` | Drop methods for Fjall cleanup | 9 |
| `crates/trondb-wal/src/record.rs` | New WAL record types for DROP | 9 |
| `crates/trondb-proto/proto/trondb.proto` | New proto messages | 3, 6, 9, 12 |
| `crates/trondb-proto/src/convert_plan.rs` | Bidirectional proto conversions | 3, 6, 9, 12 |

---

## Chunk 1: Advanced WHERE Operators

Adds NOT, IS NULL, IS NOT NULL, IN, LIKE, and != to WHERE clauses.

### Task 1: Tokens + AST for Advanced WHERE

**Files:**
- Modify: `crates/trondb-tql/src/token.rs`
- Modify: `crates/trondb-tql/src/ast.rs`

- [ ] **Step 1: Write failing tests for new tokens**

In `crates/trondb-tql/src/token.rs`, add to the `#[cfg(test)] mod tests` block:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- lex_not_keyword lex_is_keyword lex_like_keyword lex_advanced_where_full`
Expected: FAIL — `Token::Not`, `Token::Is`, `Token::Like` do not exist.

- [ ] **Step 3: Add new tokens to token.rs**

Add these token variants before the `// Identifiers` comment (after the existing keyword block):

```rust
#[token("NOT", priority = 10, ignore(ascii_case))]
Not,

#[token("IS", priority = 10, ignore(ascii_case))]
Is,

#[token("LIKE", priority = 10, ignore(ascii_case))]
Like,

#[token("%")]
Percent,
```

Note: `IN` already exists as `Token::In`. `NULL` already exists as `Token::Null`. `!=` already exists as `Token::Neq`.

- [ ] **Step 4: Add new WhereClause variants to ast.rs**

Extend the `WhereClause` enum:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum WhereClause {
    Eq(String, Literal),
    Neq(String, Literal),           // NEW: field != value
    Gt(String, Literal),
    Lt(String, Literal),
    Gte(String, Literal),
    Lte(String, Literal),
    And(Box<WhereClause>, Box<WhereClause>),
    Or(Box<WhereClause>, Box<WhereClause>),
    Not(Box<WhereClause>),          // NEW: NOT clause
    IsNull(String),                 // NEW: field IS NULL
    IsNotNull(String),              // NEW: field IS NOT NULL
    In(String, Vec<Literal>),       // NEW: field IN (val1, val2, ...)
    Like(String, String),           // NEW: field LIKE 'pattern'
}
```

- [ ] **Step 5: Run token tests to verify they pass**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- lex_not_keyword lex_is_keyword lex_like_keyword lex_advanced_where_full`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-tql/src/token.rs crates/trondb-tql/src/ast.rs
git commit -m "feat(tql): add tokens and AST nodes for advanced WHERE operators (NOT, IS NULL, IN, LIKE, !=)"
```

---

### Task 2: Parser for Advanced WHERE

**Files:**
- Modify: `crates/trondb-tql/src/parser.rs`

The parser uses recursive descent for WHERE clauses. Current flow:
- `parse_where_clause()` handles AND/OR (line ~962)
- `parse_where_comparison()` handles individual `field op literal` comparisons (line ~1007)

We need to extend `parse_where_comparison()` to handle:
- `NOT <clause>` — prefix unary
- `field IS NULL` / `field IS NOT NULL`
- `field IN (val1, val2, ...)`
- `field LIKE 'pattern'`
- `field != value`

- [ ] **Step 1: Write failing parser tests**

Add to `crates/trondb-tql/src/parser.rs` test module:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- parse_where_not parse_where_is_null parse_where_is_not_null parse_where_in_list parse_where_like parse_where_neq`
Expected: FAIL — parser doesn't recognize these constructs.

- [ ] **Step 3: Implement parser changes**

In `parser.rs`, modify `parse_where_comparison()` to handle the new operators. The function currently (around line 1007) does:

```rust
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
```

This replaces the existing `parse_where_comparison()` entirely.

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql`
Expected: ALL PASS (both new and existing parser tests).

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-tql/src/parser.rs
git commit -m "feat(tql): parse NOT, IS NULL, IS NOT NULL, IN, LIKE, != in WHERE clauses"
```

---

### Task 3: Planner + Executor for Advanced WHERE

**Files:**
- Modify: `crates/trondb-core/src/planner.rs`
- Modify: `crates/trondb-core/src/executor.rs`

The planner's `first_field_in_clause()` and `select_fetch_strategy()` need new arms. The executor's `entity_matches()` needs new evaluation logic. No new Plan types needed — WHERE is embedded in existing FetchPlan/SearchPlan.

- [ ] **Step 1: Write failing executor tests**

Add to `crates/trondb-core/src/executor.rs` test module:

```rust
#[tokio::test]
async fn fetch_where_neq() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "venues", 3).await;
    insert_entity(&exec, "venues", "v1", vec![("status", Literal::String("active".into()))], None).await;
    insert_entity(&exec, "venues", "v2", vec![("status", Literal::String("archived".into()))], None).await;
    insert_entity(&exec, "venues", "v3", vec![("status", Literal::String("active".into()))], None).await;

    let result = exec.execute(&Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: Some(WhereClause::Neq("status".into(), Literal::String("archived".into()))),
        limit: None,
        strategy: FetchStrategy::FullScan,
    })).await.unwrap();

    assert_eq!(result.rows.len(), 2);
}

#[tokio::test]
async fn fetch_where_is_null() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "venues", 3).await;
    insert_entity(&exec, "venues", "v1", vec![("name", Literal::String("Blue Note".into()))], None).await;
    insert_entity(&exec, "venues", "v2", vec![], None).await; // no name field

    let result = exec.execute(&Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: Some(WhereClause::IsNull("name".into())),
        limit: None,
        strategy: FetchStrategy::FullScan,
    })).await.unwrap();

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].values.get("id").unwrap(), &Value::String("v2".into()));
}

#[tokio::test]
async fn fetch_where_is_not_null() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "venues", 3).await;
    insert_entity(&exec, "venues", "v1", vec![("name", Literal::String("Blue Note".into()))], None).await;
    insert_entity(&exec, "venues", "v2", vec![], None).await;

    let result = exec.execute(&Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: Some(WhereClause::IsNotNull("name".into())),
        limit: None,
        strategy: FetchStrategy::FullScan,
    })).await.unwrap();

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].values.get("id").unwrap(), &Value::String("v1".into()));
}

#[tokio::test]
async fn fetch_where_in() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "venues", 3).await;
    insert_entity(&exec, "venues", "v1", vec![("category", Literal::String("music".into()))], None).await;
    insert_entity(&exec, "venues", "v2", vec![("category", Literal::String("theatre".into()))], None).await;
    insert_entity(&exec, "venues", "v3", vec![("category", Literal::String("food".into()))], None).await;

    let result = exec.execute(&Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: Some(WhereClause::In("category".into(), vec![
            Literal::String("music".into()),
            Literal::String("theatre".into()),
        ])),
        limit: None,
        strategy: FetchStrategy::FullScan,
    })).await.unwrap();

    assert_eq!(result.rows.len(), 2);
}

#[tokio::test]
async fn fetch_where_like() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "venues", 3).await;
    insert_entity(&exec, "venues", "v1", vec![("name", Literal::String("Jazz Cafe".into()))], None).await;
    insert_entity(&exec, "venues", "v2", vec![("name", Literal::String("Jazz Club Bristol".into()))], None).await;
    insert_entity(&exec, "venues", "v3", vec![("name", Literal::String("Rock Bar".into()))], None).await;

    let result = exec.execute(&Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: Some(WhereClause::Like("name".into(), "Jazz%".into())),
        limit: None,
        strategy: FetchStrategy::FullScan,
    })).await.unwrap();

    assert_eq!(result.rows.len(), 2);
}

#[tokio::test]
async fn fetch_where_not() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "venues", 3).await;
    insert_entity(&exec, "venues", "v1", vec![("active", Literal::Bool(true))], None).await;
    insert_entity(&exec, "venues", "v2", vec![("active", Literal::Bool(false))], None).await;

    let result = exec.execute(&Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: Some(WhereClause::Not(Box::new(
            WhereClause::Eq("active".into(), Literal::Bool(true))
        ))),
        limit: None,
        strategy: FetchStrategy::FullScan,
    })).await.unwrap();

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].values.get("id").unwrap(), &Value::String("v2".into()));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core -- fetch_where_neq fetch_where_is_null fetch_where_is_not_null fetch_where_in fetch_where_like fetch_where_not`
Expected: FAIL — `entity_matches()` doesn't handle new variants.

- [ ] **Step 3: Update planner helper functions**

In `crates/trondb-core/src/planner.rs`, update `first_field_in_clause()`:

```rust
fn first_field_in_clause(clause: &WhereClause) -> String {
    match clause {
        WhereClause::Eq(field, _) => field.clone(),
        WhereClause::Neq(field, _) => field.clone(),
        WhereClause::Gt(field, _) => field.clone(),
        WhereClause::Lt(field, _) => field.clone(),
        WhereClause::Gte(field, _) => field.clone(),
        WhereClause::Lte(field, _) => field.clone(),
        WhereClause::And(left, _) => first_field_in_clause(left),
        WhereClause::Or(left, _) => first_field_in_clause(left),
        WhereClause::Not(inner) => first_field_in_clause(inner),
        WhereClause::IsNull(field) => field.clone(),
        WhereClause::IsNotNull(field) => field.clone(),
        WhereClause::In(field, _) => field.clone(),
        WhereClause::Like(field, _) => field.clone(),
    }
}
```

Update `select_fetch_strategy()` — the new operators fall through to `FullScan` (no index optimization for NOT/IN/LIKE/IS NULL yet). The existing match already has a `_ => FetchStrategy::FullScan` fallthrough that handles this.

Update `range_bounds_from_clause()` in executor.rs — add wildcard arms for the new variants that return full-range bounds (same as existing `_ =>` arm).

- [ ] **Step 4: Implement entity_matches for new variants**

In `crates/trondb-core/src/executor.rs`, extend `entity_matches()`:

```rust
fn entity_matches(entity: &Entity, clause: &WhereClause) -> bool {
    match clause {
        // ... existing arms unchanged ...
        WhereClause::Neq(field, lit) => {
            let expected = literal_to_value(lit);
            if field == "id" {
                return Value::String(entity.id.to_string()) != expected;
            }
            entity.metadata.get(field)
                .map(|v| *v != expected)
                .unwrap_or(true) // missing field != value is true
        }
        WhereClause::Not(inner) => !entity_matches(entity, inner),
        WhereClause::IsNull(field) => {
            if field == "id" {
                return false; // id is never null
            }
            !entity.metadata.contains_key(field)
                || entity.metadata.get(field) == Some(&Value::Null)
        }
        WhereClause::IsNotNull(field) => {
            if field == "id" {
                return true; // id is never null
            }
            entity.metadata.get(field)
                .map(|v| *v != Value::Null)
                .unwrap_or(false)
        }
        WhereClause::In(field, values) => {
            let entity_val = if field == "id" {
                Value::String(entity.id.to_string())
            } else {
                match entity.metadata.get(field) {
                    Some(v) => v.clone(),
                    None => return false,
                }
            };
            values.iter().any(|lit| literal_to_value(lit) == entity_val)
        }
        WhereClause::Like(field, pattern) => {
            let val = if field == "id" {
                entity.id.to_string()
            } else {
                match entity.metadata.get(field) {
                    Some(Value::String(s)) => s.clone(),
                    _ => return false,
                }
            };
            like_match(&val, pattern)
        }
    }
}
```

Add the `like_match` helper function near the other helper functions:

```rust
/// SQL LIKE pattern matching. % = any sequence, _ = any single character.
fn like_match(value: &str, pattern: &str) -> bool {
    let mut v = value.chars().peekable();
    let mut p = pattern.chars().peekable();
    like_match_recursive(&mut v.collect::<Vec<_>>(), 0, &p.collect::<Vec<_>>(), 0)
}

fn like_match_recursive(value: &[char], vi: usize, pattern: &[char], pi: usize) -> bool {
    if pi == pattern.len() {
        return vi == value.len();
    }
    match pattern[pi] {
        '%' => {
            // % matches zero or more characters
            for i in vi..=value.len() {
                if like_match_recursive(value, i, pattern, pi + 1) {
                    return true;
                }
            }
            false
        }
        '_' => {
            // _ matches exactly one character
            vi < value.len() && like_match_recursive(value, vi + 1, pattern, pi + 1)
        }
        c => {
            vi < value.len() && value[vi] == c
                && like_match_recursive(value, vi + 1, pattern, pi + 1)
        }
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core -- fetch_where_neq fetch_where_is_null fetch_where_is_not_null fetch_where_in fetch_where_like fetch_where_not`
Expected: ALL PASS

- [ ] **Step 6: Run full workspace tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace`
Expected: 252+ pass, 0 fail (excluding known flaky `similarity_score_range`).

- [ ] **Step 7: Commit**

```bash
git add crates/trondb-core/src/planner.rs crates/trondb-core/src/executor.rs
git commit -m "feat: evaluate NOT, IS NULL, IS NOT NULL, IN, LIKE, != in WHERE clause execution"
```

---

## Chunk 2: ORDER BY

Adds ORDER BY clause to FETCH statements. Applied after filtering, before LIMIT.

### Task 4: Tokens + AST for ORDER BY

**Files:**
- Modify: `crates/trondb-tql/src/token.rs`
- Modify: `crates/trondb-tql/src/ast.rs`

- [ ] **Step 1: Write failing token tests**

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- lex_order_by_keywords lex_desc_keyword`
Expected: FAIL

- [ ] **Step 3: Add tokens and AST types**

In `token.rs`, add before the `// Identifiers` comment:

```rust
#[token("ORDER", priority = 10, ignore(ascii_case))]
Order,

#[token("BY", priority = 10, ignore(ascii_case))]
By,

#[token("ASC", priority = 10, ignore(ascii_case))]
Asc,

#[token("DESC", priority = 10, ignore(ascii_case))]
Desc,
```

In `ast.rs`, add a sort direction enum:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderByClause {
    pub field: String,
    pub direction: SortDirection,
}
```

Add `order_by` field to `FetchStmt`:

```rust
pub struct FetchStmt {
    pub collection: String,
    pub fields: FieldList,
    pub filter: Option<WhereClause>,
    pub order_by: Vec<OrderByClause>,  // NEW
    pub limit: Option<usize>,
}
```

- [ ] **Step 4: Fix compilation errors**

The `FetchStmt` change will cause compilation errors in parser.rs tests and planner.rs where `FetchStmt` is constructed. Add `order_by: vec![]` to every existing `FetchStmt` construction. There are approximately:
- ~5 places in parser.rs (parse_fetch + tests)
- ~6 places in planner.rs tests

- [ ] **Step 5: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- lex_order_by_keywords lex_desc_keyword`
Expected: PASS

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace`
Expected: ALL PASS (existing behavior preserved with empty `order_by`)

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-tql/src/token.rs crates/trondb-tql/src/ast.rs crates/trondb-tql/src/parser.rs crates/trondb-core/src/planner.rs
git commit -m "feat(tql): add ORDER BY tokens, AST types, and SortDirection enum"
```

---

### Task 5: Parser for ORDER BY

**Files:**
- Modify: `crates/trondb-tql/src/parser.rs`

- [ ] **Step 1: Write failing parser tests**

```rust
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
            assert_eq!(f.order_by[0].direction, SortDirection::Asc); // default
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- parse_fetch_order_by`
Expected: FAIL

- [ ] **Step 3: Implement parse_order_by and update parse_fetch**

Add a helper method to `Parser`:

```rust
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
```

In `parse_fetch()`, add ORDER BY parsing between the WHERE clause and LIMIT. Current structure (approximately):

```
FETCH fields FROM collection [WHERE clause] [LIMIT n] ;
```

Change to:

```
FETCH fields FROM collection [WHERE clause] [ORDER BY field [ASC|DESC], ...] [LIMIT n] ;
```

After WHERE parsing and before LIMIT parsing, add:

```rust
let order_by = if self.peek() == Some(&Token::Order) {
    self.advance();
    self.parse_order_by()?
} else {
    vec![]
};
```

And pass `order_by` into the `FetchStmt` construction.

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql`
Expected: ALL PASS

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-tql/src/parser.rs
git commit -m "feat(tql): parse ORDER BY clause in FETCH statements"
```

---

### Task 6: Planner + Executor for ORDER BY

**Files:**
- Modify: `crates/trondb-core/src/planner.rs`
- Modify: `crates/trondb-core/src/executor.rs`

- [ ] **Step 1: Add order_by to FetchPlan**

In `planner.rs`, add to `FetchPlan`:

```rust
pub struct FetchPlan {
    pub collection: String,
    pub fields: FieldList,
    pub filter: Option<WhereClause>,
    pub order_by: Vec<trondb_tql::OrderByClause>,  // NEW
    pub limit: Option<usize>,
    pub strategy: FetchStrategy,
}
```

Update the `plan()` function's `Statement::Fetch` arm to pass through `order_by`:

```rust
order_by: s.order_by.clone(),
```

Fix all existing `FetchPlan` constructions in planner.rs and executor.rs tests — add `order_by: vec![]`.

- [ ] **Step 2: Write failing executor tests**

```rust
#[tokio::test]
async fn fetch_order_by_asc() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "venues", 3).await;
    insert_entity(&exec, "venues", "v1", vec![("name", Literal::String("Charlie".into()))], None).await;
    insert_entity(&exec, "venues", "v2", vec![("name", Literal::String("Alpha".into()))], None).await;
    insert_entity(&exec, "venues", "v3", vec![("name", Literal::String("Bravo".into()))], None).await;

    let result = exec.execute(&Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: None,
        order_by: vec![trondb_tql::OrderByClause {
            field: "name".into(),
            direction: trondb_tql::SortDirection::Asc,
        }],
        limit: None,
        strategy: FetchStrategy::FullScan,
    })).await.unwrap();

    assert_eq!(result.rows.len(), 3);
    assert_eq!(result.rows[0].values.get("name").unwrap(), &Value::String("Alpha".into()));
    assert_eq!(result.rows[1].values.get("name").unwrap(), &Value::String("Bravo".into()));
    assert_eq!(result.rows[2].values.get("name").unwrap(), &Value::String("Charlie".into()));
}

#[tokio::test]
async fn fetch_order_by_desc_with_limit() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "venues", 3).await;
    insert_entity(&exec, "venues", "v1", vec![("score", Literal::Int(10))], None).await;
    insert_entity(&exec, "venues", "v2", vec![("score", Literal::Int(30))], None).await;
    insert_entity(&exec, "venues", "v3", vec![("score", Literal::Int(20))], None).await;

    let result = exec.execute(&Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: None,
        order_by: vec![trondb_tql::OrderByClause {
            field: "score".into(),
            direction: trondb_tql::SortDirection::Desc,
        }],
        limit: Some(2),
        strategy: FetchStrategy::FullScan,
    })).await.unwrap();

    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0].values.get("score").unwrap(), &Value::Int(30));
    assert_eq!(result.rows[1].values.get("score").unwrap(), &Value::Int(20));
}
```

- [ ] **Step 3: Implement ORDER BY in executor**

In the executor's `Plan::Fetch` arm, after filtering and before LIMIT truncation, add sorting. This applies to all three strategy paths (FieldIndexLookup, FieldIndexRange, FullScan).

Add a helper function:

```rust
fn sort_rows(rows: &mut [Row], order_by: &[trondb_tql::OrderByClause]) {
    if order_by.is_empty() {
        return;
    }
    rows.sort_by(|a, b| {
        for clause in order_by {
            let a_val = a.values.get(&clause.field);
            let b_val = b.values.get(&clause.field);
            let cmp = compare_values(a_val, b_val);
            let cmp = match clause.direction {
                trondb_tql::SortDirection::Asc => cmp,
                trondb_tql::SortDirection::Desc => cmp.reverse(),
            };
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
        }
        std::cmp::Ordering::Equal
    });
}

fn compare_values(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) | (Some(Value::Null), Some(Value::Null)) => std::cmp::Ordering::Equal,
        (None | Some(Value::Null), _) => std::cmp::Ordering::Greater, // NULLs sort last
        (_, None | Some(Value::Null)) => std::cmp::Ordering::Less,
        (Some(Value::Int(x)), Some(Value::Int(y))) => x.cmp(y),
        (Some(Value::Float(x)), Some(Value::Float(y))) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
        (Some(Value::Int(x)), Some(Value::Float(y))) => (*x as f64).partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
        (Some(Value::Float(x)), Some(Value::Int(y))) => x.partial_cmp(&(*y as f64)).unwrap_or(std::cmp::Ordering::Equal),
        (Some(Value::String(x)), Some(Value::String(y))) => x.cmp(y),
        (Some(Value::Bool(x)), Some(Value::Bool(y))) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    }
}
```

In each fetch strategy branch, call `sort_rows(&mut rows, &p.order_by);` before `rows.truncate(limit)`.

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core -- fetch_order_by`
Expected: PASS

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace`
Expected: ALL PASS

- [ ] **Step 5: Update EXPLAIN for ORDER BY**

In `explain_plan()`, in the `Plan::Fetch` arm, add after the limit line:

```rust
if !p.order_by.is_empty() {
    let order_str = p.order_by.iter()
        .map(|o| format!("{} {}", o.field, match o.direction {
            trondb_tql::SortDirection::Asc => "ASC",
            trondb_tql::SortDirection::Desc => "DESC",
        }))
        .collect::<Vec<_>>()
        .join(", ");
    props.push(("order_by", order_str));
}
```

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/src/planner.rs crates/trondb-core/src/executor.rs
git commit -m "feat: ORDER BY execution with multi-field sort and NULL-last semantics"
```

---

## Chunk 3: DROP Statements

Adds DROP COLLECTION and DROP EDGE TYPE with cascading cleanup.

### Task 7: Tokens + AST for DROP

**Files:**
- Modify: `crates/trondb-tql/src/ast.rs`

Note: `Token::Drop` already exists in token.rs (line 148). We only need new AST types.

- [ ] **Step 1: Add AST types**

In `ast.rs`, add:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct DropCollectionStmt {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropEdgeTypeStmt {
    pub name: String,
}
```

Add to the `Statement` enum:

```rust
DropCollection(DropCollectionStmt),
DropEdgeType(DropEdgeTypeStmt),
```

- [ ] **Step 2: Commit**

```bash
git add crates/trondb-tql/src/ast.rs
git commit -m "feat(tql): add DropCollection and DropEdgeType AST nodes"
```

---

### Task 8: Parser for DROP

**Files:**
- Modify: `crates/trondb-tql/src/parser.rs`

- [ ] **Step 1: Write failing parser tests**

```rust
#[test]
fn parse_drop_collection() {
    let stmt = parse("DROP COLLECTION venues;").unwrap();
    match stmt {
        Statement::DropCollection(d) => {
            assert_eq!(d.name, "venues");
        }
        _ => panic!("expected DropCollection"),
    }
}

#[test]
fn parse_drop_edge_type() {
    let stmt = parse("DROP EDGE TYPE 'performs_at';").unwrap();
    match stmt {
        Statement::DropEdgeType(d) => {
            assert_eq!(d.name, "performs_at");
        }
        _ => panic!("expected DropEdgeType"),
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- parse_drop_collection parse_drop_edge_type`
Expected: FAIL

- [ ] **Step 3: Implement parse_drop**

Add a `parse_drop()` method and wire it into `parse_statement()` when `Token::Drop` is seen:

```rust
fn parse_drop(&mut self) -> Result<Statement, ParseError> {
    self.advance(); // DROP
    match self.peek() {
        Some(Token::Collection) => {
            self.advance();
            let name = self.expect_ident()?;
            self.expect(&Token::Semicolon)?;
            Ok(Statement::DropCollection(DropCollectionStmt { name }))
        }
        Some(Token::Edge) => {
            self.advance(); // EDGE
            self.expect(&Token::Type)?;
            let name = self.expect_string_lit()?;
            self.expect(&Token::Semicolon)?;
            Ok(Statement::DropEdgeType(DropEdgeTypeStmt { name }))
        }
        other => Err(ParseError::UnexpectedToken {
            pos: self.pos,
            expected: "COLLECTION or EDGE".into(),
            got: format!("{:?}", other),
        }),
    }
}
```

In `parse_statement()`, currently `Token::Drop` is not dispatched (it was only used in ALTER ENTITY DROP AFFINITY GROUP). Add a new arm — but be careful: the current code routes `ALTER` → parse_alter which handles `ALTER ENTITY DROP`. `Token::Drop` at statement level (first token) means `DROP COLLECTION` or `DROP EDGE TYPE`.

Add to `parse_statement()` dispatch:

```rust
Some(Token::Drop) => self.parse_drop(),
```

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql`
Expected: ALL PASS

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-tql/src/parser.rs
git commit -m "feat(tql): parse DROP COLLECTION and DROP EDGE TYPE statements"
```

---

### Task 9: Planner + Executor + WAL + Store for DROP

**Files:**
- Modify: `crates/trondb-wal/src/record.rs` — new record types
- Modify: `crates/trondb-core/src/store.rs` — drop methods
- Modify: `crates/trondb-core/src/planner.rs` — new plan types
- Modify: `crates/trondb-core/src/executor.rs` — drop execution

This is the largest task in the plan because DROP requires cascading cleanup across multiple subsystems.

- [ ] **Step 1: Add WAL record types**

In `crates/trondb-wal/src/record.rs`, add to `RecordType`:

```rust
SchemaDropColl     = 0x52,
SchemaDropEdgeType = 0x53,
```

- [ ] **Step 2: Add store drop methods**

In `crates/trondb-core/src/store.rs`, add:

```rust
pub fn drop_collection(&self, name: &str) -> Result<(), EngineError> {
    // Delete schema metadata
    let schema_key = format!("{COLLECTION_PREFIX}{name}");
    self.meta.remove(schema_key.as_bytes())
        .map_err(|e| EngineError::Storage(e.to_string()))?;

    // Delete all entities in the collection's main partition
    if let Some(partition) = self.keyspace.open_partition(
        &format!("{name}"),
        fjall::PartitionCreateOptions::default()
    ).ok() {
        // Iterate and delete all keys
        for item in partition.iter() {
            if let Ok((key, _)) = item {
                partition.remove(&key)
                    .map_err(|e| EngineError::Storage(e.to_string()))?;
            }
        }
    }

    // Delete warm and archive tier partitions
    for prefix in &[format!("warm.{name}"), format!("archive.{name}")] {
        if let Ok(partition) = self.keyspace.open_partition(
            prefix,
            fjall::PartitionCreateOptions::default()
        ) {
            for item in partition.iter() {
                if let Ok((key, _)) = item {
                    let _ = partition.remove(&key);
                }
            }
        }
    }

    // Delete field index partitions (fidx.{collection}.*)
    // These are cleaned up by the executor via field_indexes DashMap

    Ok(())
}

pub fn drop_edge_type(&self, name: &str) -> Result<(), EngineError> {
    // Delete edge type metadata
    let et_key = format!("{EDGE_TYPE_PREFIX}{name}");
    self.meta.remove(et_key.as_bytes())
        .map_err(|e| EngineError::Storage(e.to_string()))?;

    // Delete edge partition
    let partition_name = format!("edges.{name}");
    if let Ok(partition) = self.keyspace.open_partition(
        &partition_name,
        fjall::PartitionCreateOptions::default()
    ) {
        for item in partition.iter() {
            if let Ok((key, _)) = item {
                let _ = partition.remove(&key);
            }
        }
    }

    Ok(())
}
```

- [ ] **Step 3: Add plan types**

In `planner.rs`, add:

```rust
#[derive(Debug, Clone)]
pub struct DropCollectionPlan {
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct DropEdgeTypePlan {
    pub name: String,
}
```

Add to `Plan` enum:

```rust
DropCollection(DropCollectionPlan),
DropEdgeType(DropEdgeTypePlan),
```

Add to `plan()` function:

```rust
Statement::DropCollection(s) => Ok(Plan::DropCollection(DropCollectionPlan {
    name: s.name.clone(),
})),
Statement::DropEdgeType(s) => Ok(Plan::DropEdgeType(DropEdgeTypePlan {
    name: s.name.clone(),
})),
```

- [ ] **Step 4: Write failing executor tests**

```rust
#[tokio::test]
async fn drop_collection_removes_everything() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "venues", 3).await;
    insert_entity(&exec, "venues", "v1", vec![("name", Literal::String("Test".into()))], Some(vec![1.0, 0.0, 0.0])).await;
    insert_entity(&exec, "venues", "v2", vec![("name", Literal::String("Test2".into()))], Some(vec![0.0, 1.0, 0.0])).await;

    // Verify entities exist
    let result = exec.execute(&Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: None,
        order_by: vec![],
        limit: None,
        strategy: FetchStrategy::FullScan,
    })).await.unwrap();
    assert_eq!(result.rows.len(), 2);

    // Drop collection
    exec.execute(&Plan::DropCollection(DropCollectionPlan {
        name: "venues".into(),
    })).await.unwrap();

    // Verify collection is gone
    let err = exec.execute(&Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: None,
        order_by: vec![],
        limit: None,
        strategy: FetchStrategy::FullScan,
    })).await;
    assert!(err.is_err());
}

#[tokio::test]
async fn drop_edge_type_removes_edges() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "acts", 3).await;
    create_collection(&exec, "venues", 3).await;
    insert_entity(&exec, "acts", "a1", vec![], None).await;
    insert_entity(&exec, "venues", "v1", vec![], None).await;

    // Create edge type and insert edge
    exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
        name: "performs_at".into(),
        from_collection: "acts".into(),
        to_collection: "venues".into(),
        decay_config: None,
        inference_config: None,
    })).await.unwrap();

    exec.execute(&Plan::InsertEdge(InsertEdgePlan {
        edge_type: "performs_at".into(),
        from_id: "a1".into(),
        to_id: "v1".into(),
        metadata: vec![],
    })).await.unwrap();

    // Drop edge type
    exec.execute(&Plan::DropEdgeType(DropEdgeTypePlan {
        name: "performs_at".into(),
    })).await.unwrap();

    // Verify traverse fails (edge type gone)
    let err = exec.execute(&Plan::Traverse(TraversePlan {
        edge_type: "performs_at".into(),
        from_id: "a1".into(),
        depth: 1,
        limit: None,
    })).await;
    assert!(err.is_err());
}
```

- [ ] **Step 5: Implement DROP COLLECTION execution**

In executor.rs, add a new `Plan::DropCollection` arm:

```rust
Plan::DropCollection(p) => {
    // Validate collection exists
    if !self.store.has_collection(&p.name) {
        return Err(EngineError::CollectionNotFound(p.name.clone()));
    }

    // WAL log the drop
    let payload = rmp_serde::to_vec_named(&p.name)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    self.wal.append(RecordType::SchemaDropColl, &p.name, &payload).await?;

    // Remove HNSW indexes for this collection
    let hnsw_keys: Vec<String> = self.indexes.iter()
        .filter(|e| e.key().starts_with(&format!("{}:", p.name)))
        .map(|e| e.key().clone())
        .collect();
    for key in hnsw_keys {
        self.indexes.remove(&key);
    }

    // Remove sparse indexes
    let sparse_keys: Vec<String> = self.sparse_indexes.iter()
        .filter(|e| e.key().starts_with(&format!("{}:", p.name)))
        .map(|e| e.key().clone())
        .collect();
    for key in sparse_keys {
        self.sparse_indexes.remove(&key);
    }

    // Remove field indexes
    let fidx_keys: Vec<String> = self.field_indexes.iter()
        .filter(|e| e.key().starts_with(&format!("{}:", p.name)))
        .map(|e| e.key().clone())
        .collect();
    for key in fidx_keys {
        self.field_indexes.remove(&key);
    }

    // Remove location table entries for all entities in this collection
    // (entity_collections gives us entity→collection mapping)
    let entity_ids: Vec<LogicalId> = self.entity_collections.iter()
        .filter(|e| e.value() == &p.name)
        .map(|e| e.key().clone())
        .collect();
    for eid in &entity_ids {
        self.location.remove_entity(eid);
        self.entity_collections.remove(eid);
    }

    // Remove from Fjall
    self.store.drop_collection(&p.name)?;

    // Remove schema
    self.schemas.remove(&p.name);

    Ok(QueryResult {
        columns: vec!["status".into()],
        rows: vec![Row {
            values: HashMap::from([
                ("status".into(), Value::String(format!("dropped collection '{}'", p.name))),
            ]),
            score: None,
        }],
        stats: QueryStats {
            elapsed: start.elapsed(),
            entities_scanned: 0,
            mode: QueryMode::Deterministic,
            tier: "Fjall".into(),
        },
    })
}
```

- [ ] **Step 6: Implement DROP EDGE TYPE execution**

```rust
Plan::DropEdgeType(p) => {
    // Validate edge type exists
    if !self.edge_types.contains_key(&p.name) {
        return Err(EngineError::InvalidQuery(format!("edge type '{}' not found", p.name)));
    }

    // WAL log
    let payload = rmp_serde::to_vec_named(&p.name)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    self.wal.append(RecordType::SchemaDropEdgeType, &p.name, &payload).await?;

    // Remove all adjacency entries for this edge type
    self.adjacency.remove_edge_type(&p.name);

    // Remove from Fjall
    self.store.drop_edge_type(&p.name)?;

    // Remove edge type metadata
    self.edge_types.remove(&p.name);

    Ok(QueryResult {
        columns: vec!["status".into()],
        rows: vec![Row {
            values: HashMap::from([
                ("status".into(), Value::String(format!("dropped edge type '{}'", p.name))),
            ]),
            score: None,
        }],
        stats: QueryStats {
            elapsed: start.elapsed(),
            entities_scanned: 0,
            mode: QueryMode::Deterministic,
            tier: "Fjall".into(),
        },
    })
}
```

Note: `AdjacencyIndex::remove_edge_type(&self, name: &str)` may not exist yet. If not, add it to `crates/trondb-core/src/edge.rs`:

```rust
pub fn remove_edge_type(&self, edge_type: &str) {
    // Remove all forward entries for this edge type
    self.forward.retain(|k, _| k.1 != edge_type);
    // Remove all backward entries for this edge type
    self.backward.retain(|k, _| k.1 != edge_type);
}
```

- [ ] **Step 7: Add EXPLAIN support for DROP plans**

In `explain_plan()`, add:

```rust
Plan::DropCollection(p) => {
    props.push(("mode", "Deterministic".into()));
    props.push(("verb", "DROP COLLECTION".into()));
    props.push(("collection", p.name.clone()));
}
Plan::DropEdgeType(p) => {
    props.push(("mode", "Deterministic".into()));
    props.push(("verb", "DROP EDGE TYPE".into()));
    props.push(("edge_type", p.name.clone()));
}
```

- [ ] **Step 8: Add WAL replay handlers for DROP**

In `replay_wal_records()`, add:

```rust
RecordType::SchemaDropColl => {
    let name: String = rmp_serde::from_slice(&record.payload)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    self.store.drop_collection(&name)?;
    self.schemas.remove(&name);
    // HNSW/sparse/field indexes are RAM-only, rebuilt from scratch on startup
    replayed += 1;
}
RecordType::SchemaDropEdgeType => {
    let name: String = rmp_serde::from_slice(&record.payload)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    self.store.drop_edge_type(&name)?;
    self.edge_types.remove(&name);
    replayed += 1;
}
```

- [ ] **Step 9: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core -- drop_collection drop_edge_type`
Expected: PASS

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace`
Expected: ALL PASS

- [ ] **Step 10: Commit**

```bash
git add crates/trondb-wal/src/record.rs crates/trondb-core/src/store.rs crates/trondb-core/src/edge.rs crates/trondb-core/src/planner.rs crates/trondb-core/src/executor.rs
git commit -m "feat: DROP COLLECTION and DROP EDGE TYPE with cascading cleanup and WAL support"
```

---

## Chunk 4: Query Hints

Adds `/*+ HINT */` comment-style hints that influence the planner. Hints are advisory — the planner honours them unless doing so would produce incorrect results.

### Task 10: Tokens + AST for Query Hints

**Files:**
- Modify: `crates/trondb-tql/src/token.rs`
- Modify: `crates/trondb-tql/src/ast.rs`

The hint syntax is `/*+ HINT_NAME [VALUE] */` — a block comment with a `+` prefix. The lexer currently skips `--` line comments. We need to recognize `/*+ ... */` as a hint token.

- [ ] **Step 1: Add hint token**

In `token.rs`, add a regex token for hints:

```rust
#[regex(r"/\*\+[^*]*\*/", callback = |lex| {
    let s = lex.slice();
    // Strip /*+ and */
    s[3..s.len()-2].trim().to_string()
})]
Hint(String),
```

Also add to the skip pattern — regular `/* */` block comments (without `+`) should be skipped:

```rust
#[logos(skip r"/\*[^+][^*]*\*/")]
```

Note: this is a simple approach. If the current lexer doesn't handle block comments, we only need the `/*+ ... */` pattern as a token.

- [ ] **Step 2: Add AST types**

In `ast.rs`, add:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum QueryHint {
    NoPromote,          // /*+ NO_PROMOTE */ — don't promote from warm to hot
    NoPrefilter,        // /*+ NO_PREFILTER */ — skip scalar pre-filter
    ForceFullScan,      // /*+ FORCE_FULL_SCAN */ — skip index lookup
    MaxAcu(f64),        // /*+ MAX_ACU(200) */ — ACU budget (future use)
    Timeout(u64),       // /*+ TIMEOUT(5000) */ — query timeout in ms (future use)
}
```

Add `hints` field to `FetchStmt` and `SearchStmt`:

```rust
pub struct FetchStmt {
    pub collection: String,
    pub fields: FieldList,
    pub filter: Option<WhereClause>,
    pub order_by: Vec<OrderByClause>,
    pub limit: Option<usize>,
    pub hints: Vec<QueryHint>,       // NEW
}

pub struct SearchStmt {
    pub collection: String,
    pub fields: FieldList,
    pub dense_vector: Option<Vec<f64>>,
    pub sparse_vector: Option<Vec<(u32, f32)>>,
    pub filter: Option<WhereClause>,
    pub confidence: Option<f64>,
    pub limit: Option<usize>,
    pub query_text: Option<String>,
    pub using_repr: Option<String>,
    pub hints: Vec<QueryHint>,       // NEW
}
```

Fix all `FetchStmt` and `SearchStmt` constructions to add `hints: vec![]`.

- [ ] **Step 3: Write token tests**

```rust
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
```

- [ ] **Step 4: Run and verify**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- lex_hint`
Expected: PASS (after implementation)

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-tql/src/token.rs crates/trondb-tql/src/ast.rs crates/trondb-tql/src/parser.rs crates/trondb-core/src/planner.rs
git commit -m "feat(tql): add query hint token, AST types, and QueryHint enum"
```

---

### Task 11: Parser for Query Hints

**Files:**
- Modify: `crates/trondb-tql/src/parser.rs`

- [ ] **Step 1: Write failing parser tests**

```rust
#[test]
fn parse_fetch_with_no_promote_hint() {
    let stmt = parse("FETCH /*+ NO_PROMOTE */ * FROM venues WHERE id = 'v1';").unwrap();
    match stmt {
        Statement::Fetch(f) => {
            assert_eq!(f.hints, vec![QueryHint::NoPromote]);
        }
        _ => panic!("expected Fetch"),
    }
}

#[test]
fn parse_search_with_no_prefilter_hint() {
    let stmt = parse("SEARCH /*+ NO_PREFILTER */ venues NEAR VECTOR [1.0, 0.0] LIMIT 10;").unwrap();
    match stmt {
        Statement::Search(s) => {
            assert_eq!(s.hints, vec![QueryHint::NoPrefilter]);
        }
        _ => panic!("expected Search"),
    }
}

#[test]
fn parse_multiple_hints() {
    let stmt = parse("FETCH /*+ NO_PROMOTE */ /*+ FORCE_FULL_SCAN */ * FROM venues;").unwrap();
    match stmt {
        Statement::Fetch(f) => {
            assert_eq!(f.hints.len(), 2);
            assert!(f.hints.contains(&QueryHint::NoPromote));
            assert!(f.hints.contains(&QueryHint::ForceFullScan));
        }
        _ => panic!("expected Fetch"),
    }
}
```

- [ ] **Step 2: Implement hint parsing**

Add a helper to `Parser`:

```rust
fn parse_hints(&mut self) -> Vec<QueryHint> {
    let mut hints = Vec::new();
    while let Some(Token::Hint(raw)) = self.peek() {
        let raw = raw.clone();
        self.advance();
        match raw.trim() {
            "NO_PROMOTE" => hints.push(QueryHint::NoPromote),
            "NO_PREFILTER" => hints.push(QueryHint::NoPrefilter),
            "FORCE_FULL_SCAN" => hints.push(QueryHint::ForceFullScan),
            s if s.starts_with("MAX_ACU(") && s.ends_with(')') => {
                if let Ok(v) = s[8..s.len()-1].parse::<f64>() {
                    hints.push(QueryHint::MaxAcu(v));
                }
            }
            s if s.starts_with("TIMEOUT(") && s.ends_with(')') => {
                if let Ok(v) = s[8..s.len()-1].parse::<u64>() {
                    hints.push(QueryHint::Timeout(v));
                }
            }
            _ => {} // unknown hints are ignored (advisory)
        }
    }
    hints
}
```

In `parse_fetch()`, call `let hints = self.parse_hints();` immediately after advancing past `Token::Fetch`, and pass into `FetchStmt`.

In `parse_search()`, call `let hints = self.parse_hints();` immediately after advancing past `Token::Search`, and pass into `SearchStmt`.

- [ ] **Step 3: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- parse_fetch_with_no_promote parse_search_with_no_prefilter parse_multiple_hints`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-tql/src/parser.rs
git commit -m "feat(tql): parse query hints from /*+ HINT */ syntax"
```

---

### Task 12: Planner + Executor for Query Hints

**Files:**
- Modify: `crates/trondb-core/src/planner.rs`
- Modify: `crates/trondb-core/src/executor.rs`

- [ ] **Step 1: Add hints to FetchPlan and SearchPlan**

In `planner.rs`:

```rust
pub struct FetchPlan {
    // ... existing fields ...
    pub hints: Vec<trondb_tql::QueryHint>,  // NEW
}

pub struct SearchPlan {
    // ... existing fields ...
    pub hints: Vec<trondb_tql::QueryHint>,  // NEW
}
```

Fix all `FetchPlan` and `SearchPlan` constructions to add `hints: vec![]`.

- [ ] **Step 2: Apply hints in planner**

In the `Statement::Fetch` arm of `plan()`, apply `FORCE_FULL_SCAN`:

```rust
let strategy = if s.hints.contains(&trondb_tql::QueryHint::ForceFullScan) {
    FetchStrategy::FullScan
} else {
    select_fetch_strategy(&s.filter, schema.as_ref())
};
```

In the `Statement::Search` arm, apply `NO_PREFILTER`:

```rust
let pre_filter = if s.hints.contains(&trondb_tql::QueryHint::NoPrefilter) {
    None
} else {
    select_pre_filter(&s.filter, schema.as_ref())?
};
```

Pass `hints` through to the plans:

```rust
hints: s.hints.clone(),
```

- [ ] **Step 3: Write failing executor test**

```rust
#[tokio::test]
async fn fetch_force_full_scan_hint() {
    use crate::types::{StoredField, StoredIndex, StoredRepresentation, FieldType};

    let (exec, _dir) = setup_executor().await;

    // Create collection with an indexed field
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
        fields: vec![trondb_tql::FieldDecl {
            name: "city".into(),
            field_type: trondb_tql::FieldType::Text,
        }],
        indexes: vec![trondb_tql::IndexDecl {
            name: "idx_city".into(),
            fields: vec!["city".into()],
            partial_condition: None,
        }],
        vectoriser_config: None,
    })).await.unwrap();

    insert_entity(&exec, "venues", "v1", vec![("city", Literal::String("London".into()))], None).await;

    // Without hint: would use FieldIndexLookup. With FORCE_FULL_SCAN: uses FullScan.
    // We can verify via EXPLAIN
    let explain_result = exec.execute(&Plan::Explain(Box::new(Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
        order_by: vec![],
        limit: None,
        strategy: FetchStrategy::FullScan, // hint forced this
        hints: vec![trondb_tql::QueryHint::ForceFullScan],
    })))).await.unwrap();

    let strategy_row = explain_result.rows.iter()
        .find(|r| r.values.get("property") == Some(&Value::String("strategy".into())))
        .unwrap();
    assert_eq!(strategy_row.values.get("value").unwrap(), &Value::String("FullScan".into()));
}
```

- [ ] **Step 4: Add hints to EXPLAIN output**

In `explain_plan()`, for both Fetch and Search arms, add:

```rust
if !p.hints.is_empty() {
    let hints_str = p.hints.iter()
        .map(|h| format!("{:?}", h))
        .collect::<Vec<_>>()
        .join(", ");
    props.push(("hints", hints_str));
}
```

- [ ] **Step 5: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace`
Expected: ALL PASS

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/src/planner.rs crates/trondb-core/src/executor.rs
git commit -m "feat: query hints influence planner strategy (NO_PROMOTE, NO_PREFILTER, FORCE_FULL_SCAN)"
```

---

## Chunk 5: Proto/gRPC + Integration

Extends the protobuf definitions and wire conversions for multi-node parity. Updates CLAUDE.md.

### Task 13: Proto definitions for new plan types

**Files:**
- Modify: `crates/trondb-proto/proto/trondb.proto`
- Modify: `crates/trondb-proto/src/convert_plan.rs`

- [ ] **Step 1: Add proto messages**

In `trondb.proto`, add new plan messages:

```protobuf
message DropCollectionPlan {
    string name = 1;
}

message DropEdgeTypePlan {
    string name = 1;
}
```

Add to `PlanRequest` oneof:

```protobuf
DropCollectionPlan drop_collection = 20;
DropEdgeTypePlan drop_edge_type = 21;
```

Note: We do NOT add proto messages for ORDER BY or query hints — these are embedded within the existing FetchPlan/SearchPlan. Add optional fields to existing messages:

In `FetchPlan` proto message, add:

```protobuf
repeated OrderByClause order_by = 6;
repeated string hints = 7;
```

Add `OrderByClause` message:

```protobuf
message OrderByClause {
    string field = 1;
    string direction = 2;  // "ASC" or "DESC"
}
```

In `SearchPlan` proto message, add:

```protobuf
repeated string hints = 12;
```

- [ ] **Step 2: Add convert_plan.rs conversions**

Add bidirectional conversions for `DropCollectionPlan` and `DropEdgeTypePlan`:

```rust
// From<&Plan> for PlanRequest
Plan::DropCollection(p) => PlanRequest {
    plan: Some(plan_request::Plan::DropCollection(proto::DropCollectionPlan {
        name: p.name.clone(),
    })),
},
Plan::DropEdgeType(p) => PlanRequest {
    plan: Some(plan_request::Plan::DropEdgeType(proto::DropEdgeTypePlan {
        name: p.name.clone(),
    })),
},

// TryFrom<PlanRequest> for Plan
Some(plan_request::Plan::DropCollection(p)) => {
    Ok(Plan::DropCollection(planner::DropCollectionPlan {
        name: p.name,
    }))
}
Some(plan_request::Plan::DropEdgeType(p)) => {
    Ok(Plan::DropEdgeType(planner::DropEdgeTypePlan {
        name: p.name,
    }))
}
```

Add ORDER BY and hints conversions in the existing FetchPlan/SearchPlan conversion logic.

- [ ] **Step 3: Write round-trip tests**

```rust
#[test]
fn roundtrip_drop_collection() {
    let plan = Plan::DropCollection(planner::DropCollectionPlan {
        name: "venues".into(),
    });
    let proto: PlanRequest = (&plan).into();
    let back: Plan = proto.try_into().unwrap();
    match back {
        Plan::DropCollection(p) => assert_eq!(p.name, "venues"),
        _ => panic!("expected DropCollection"),
    }
}

#[test]
fn roundtrip_drop_edge_type() {
    let plan = Plan::DropEdgeType(planner::DropEdgeTypePlan {
        name: "performs_at".into(),
    });
    let proto: PlanRequest = (&plan).into();
    let back: Plan = proto.try_into().unwrap();
    match back {
        Plan::DropEdgeType(p) => assert_eq!(p.name, "performs_at"),
        _ => panic!("expected DropEdgeType"),
    }
}
```

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace`
Expected: ALL PASS

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-proto/proto/trondb.proto crates/trondb-proto/src/convert_plan.rs
git commit -m "feat(proto): add DropCollection, DropEdgeType proto messages and ORDER BY/hints wire format"
```

---

### Task 14: Update CLAUDE.md + Integration Tests

**Files:**
- Modify: `CLAUDE.md`
- Modify: `crates/trondb-core/src/executor.rs` (integration test)

- [ ] **Step 1: Write end-to-end integration test**

Add to executor.rs tests:

```rust
#[tokio::test]
async fn end_to_end_phase_12_query_language() {
    let (exec, _dir) = setup_executor().await;

    // Create collection with indexed field
    exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
        name: "events".into(),
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
            trondb_tql::FieldDecl { name: "category".into(), field_type: trondb_tql::FieldType::Text },
            trondb_tql::FieldDecl { name: "score".into(), field_type: trondb_tql::FieldType::Int },
        ],
        indexes: vec![trondb_tql::IndexDecl {
            name: "idx_category".into(),
            fields: vec!["category".into()],
            partial_condition: None,
        }],
        vectoriser_config: None,
    })).await.unwrap();

    // Insert test data
    insert_entity(&exec, "events", "e1", vec![
        ("name", Literal::String("Jazz Night".into())),
        ("category", Literal::String("music".into())),
        ("score", Literal::Int(90)),
    ], None).await;
    insert_entity(&exec, "events", "e2", vec![
        ("name", Literal::String("Comedy Hour".into())),
        ("category", Literal::String("comedy".into())),
        ("score", Literal::Int(75)),
    ], None).await;
    insert_entity(&exec, "events", "e3", vec![
        ("name", Literal::String("Jazz Festival".into())),
        ("category", Literal::String("music".into())),
        ("score", Literal::Int(95)),
    ], None).await;
    insert_entity(&exec, "events", "e4", vec![
        ("name", Literal::String("Rock Concert".into())),
        ("category", Literal::String("music".into())),
        ("score", Literal::Int(80)),
    ], None).await;

    // Test 1: IN operator
    let result = exec.execute(&Plan::Fetch(FetchPlan {
        collection: "events".into(),
        fields: FieldList::All,
        filter: Some(WhereClause::In("category".into(), vec![
            Literal::String("music".into()),
            Literal::String("comedy".into()),
        ])),
        order_by: vec![],
        limit: None,
        strategy: FetchStrategy::FullScan,
        hints: vec![],
    })).await.unwrap();
    assert_eq!(result.rows.len(), 4);

    // Test 2: LIKE + ORDER BY DESC + LIMIT
    let result = exec.execute(&Plan::Fetch(FetchPlan {
        collection: "events".into(),
        fields: FieldList::All,
        filter: Some(WhereClause::Like("name".into(), "Jazz%".into())),
        order_by: vec![trondb_tql::OrderByClause {
            field: "score".into(),
            direction: trondb_tql::SortDirection::Desc,
        }],
        limit: Some(1),
        strategy: FetchStrategy::FullScan,
        hints: vec![],
    })).await.unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].values.get("name").unwrap(), &Value::String("Jazz Festival".into()));

    // Test 3: NOT + != combined
    let result = exec.execute(&Plan::Fetch(FetchPlan {
        collection: "events".into(),
        fields: FieldList::All,
        filter: Some(WhereClause::And(
            Box::new(WhereClause::Neq("category".into(), Literal::String("comedy".into()))),
            Box::new(WhereClause::Not(Box::new(
                WhereClause::Like("name".into(), "Rock%".into())
            ))),
        )),
        order_by: vec![],
        limit: None,
        strategy: FetchStrategy::FullScan,
        hints: vec![],
    })).await.unwrap();
    assert_eq!(result.rows.len(), 2); // Jazz Night + Jazz Festival

    // Test 4: DROP COLLECTION
    exec.execute(&Plan::DropCollection(DropCollectionPlan {
        name: "events".into(),
    })).await.unwrap();

    // Verify gone
    let err = exec.execute(&Plan::Fetch(FetchPlan {
        collection: "events".into(),
        fields: FieldList::All,
        filter: None,
        order_by: vec![],
        limit: None,
        strategy: FetchStrategy::FullScan,
        hints: vec![],
    })).await;
    assert!(err.is_err());
}
```

- [ ] **Step 2: Run test**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core -- end_to_end_phase_12`
Expected: PASS

- [ ] **Step 3: Update CLAUDE.md**

Change the subtitle from "Phase 11: Inference Pipeline" to "Phase 12: Query Language Completions".

Add a new section under Inference Pipeline:

```markdown
- Query Language Completions (Phase 12)
  - Advanced WHERE: NOT, IS NULL, IS NOT NULL, IN (list), LIKE (pattern), != (not equal)
  - LIKE uses SQL semantics: % = any sequence, _ = any single character
  - WHERE evaluation: all operators in entity_matches() with recursive descent
  - ORDER BY: multi-field sort with ASC/DESC, NULL-last semantics
    - Syntax: FETCH ... ORDER BY field [ASC|DESC], ... [LIMIT n]
    - Applied after filtering, before LIMIT truncation
  - DROP COLLECTION: cascading cleanup (HNSW, sparse, field indexes, location table, Fjall, schema)
  - DROP EDGE TYPE: cascading cleanup (adjacency index, Fjall edge partition, edge type metadata)
  - WAL record types: SchemaDropColl (0x52), SchemaDropEdgeType (0x53)
  - Query hints: /*+ HINT */ syntax, advisory to planner
    - NO_PROMOTE: skip warm→hot promotion on access
    - NO_PREFILTER: skip scalar pre-filter on SEARCH WHERE
    - FORCE_FULL_SCAN: bypass field index lookup
    - MAX_ACU(n): ACU budget (future use, Phase 13)
    - TIMEOUT(ms): query timeout (future use)
    - Hints shown in EXPLAIN output
```

- [ ] **Step 4: Run full workspace tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace`
Expected: ALL PASS

- [ ] **Step 5: Commit**

```bash
git add CLAUDE.md crates/trondb-core/src/executor.rs
git commit -m "feat: Phase 12 integration tests and CLAUDE.md update for query language completions"
```

---

## Execution Order and Parallelization

Tasks must be executed mostly sequentially due to shared file dependencies:

| Batch | Tasks | Rationale |
|-------|-------|-----------|
| 1 | Task 1 → Task 2 → Task 3 | Advanced WHERE: tokens → parser → executor (sequential chain) |
| 2 | Task 4 → Task 5 → Task 6 | ORDER BY: tokens → parser → executor (sequential chain) |
| 3 | Task 7 → Task 8 → Task 9 | DROP: AST → parser → executor+WAL (sequential chain) |
| 4 | Task 10 → Task 11 → Task 12 | Query hints: tokens → parser → executor (sequential chain) |
| 5 | Task 13 | Proto/gRPC (depends on all plan types existing) |
| 6 | Task 14 | Integration + CLAUDE.md (depends on everything) |

Within each batch, tasks are strictly sequential. Batches 1–4 could theoretically overlap but all touch parser.rs/executor.rs, so sequential is safer.
