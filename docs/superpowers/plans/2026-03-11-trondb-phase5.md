# Phase 5: Structural Edges + TRAVERSE Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build structural edges with schema-first edge types, Fjall persistence, RAM adjacency index, and the TRAVERSE query verb (single-hop).

**Architecture:** Edge types declared via `CREATE EDGE`, edges stored in Fjall with a DashMap-backed AdjacencyIndex in RAM for fast TRAVERSE. Rebuilt from Fjall on startup (same pattern as HNSW). Structural edges have confidence=1.0, no decay.

**Tech Stack:** Existing trondb workspace (Fjall, DashMap, logos, rmp_serde). No new dependencies.

**Spec:** `docs/superpowers/specs/2026-03-11-trondb-phase5-design.md`

---

## Chunk 1: TQL Grammar

### Task 1: Add tokens, AST types, and parser for edge statements

**Files:**
- Modify: `crates/trondb-tql/src/token.rs` (add keywords)
- Modify: `crates/trondb-tql/src/ast.rs` (add statement types)
- Modify: `crates/trondb-tql/src/parser.rs` (add parse methods)
- Modify: `crates/trondb-tql/src/lib.rs` (re-export new types)

**Note on syntax simplification:** The design spec uses `CREATE EDGE TYPE <name>`, but adding `TYPE` as a reserved keyword would break any field named `type`. Instead, use `CREATE EDGE <name> FROM <coll> TO <coll>;` — the semantics are clear without TYPE, and this matches OrientDB conventions. The WAL record is still `SchemaCreateEdgeType` internally.

- [ ] **Step 1: Add new keyword tokens**

In `crates/trondb-tql/src/token.rs`, add these tokens after the existing keyword tokens (before `// Identifiers`):

```rust
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
```

- [ ] **Step 2: Add AST types**

In `crates/trondb-tql/src/ast.rs`, add to the `Statement` enum:

```rust
    CreateEdgeType(CreateEdgeTypeStmt),
    InsertEdge(InsertEdgeStmt),
    DeleteEdge(DeleteEdgeStmt),
    Traverse(TraverseStmt),
```

Add these structs after the existing statement structs:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct CreateEdgeTypeStmt {
    pub name: String,
    pub from_collection: String,
    pub to_collection: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertEdgeStmt {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
    pub metadata: Vec<(String, Literal)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteEdgeStmt {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TraverseStmt {
    pub edge_type: String,
    pub from_id: String,
    pub depth: usize,
    pub limit: Option<usize>,
}
```

- [ ] **Step 3: Update parser — parse_create branching**

In `crates/trondb-tql/src/parser.rs`, the current `parse_create` method always expects `CREATE COLLECTION`. Change it to branch:

```rust
    fn parse_create(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // CREATE
        match self.peek() {
            Some(Token::Collection) => self.parse_create_collection(),
            Some(Token::Edge) => self.parse_create_edge(),
            Some(tok) => {
                let tok_str = format!("{tok:?}");
                let pos = self.tokens[self.pos].1.start;
                Err(ParseError::UnexpectedToken {
                    pos,
                    expected: "COLLECTION or EDGE".to_string(),
                    got: tok_str,
                })
            }
            None => Err(ParseError::UnexpectedEof("expected COLLECTION or EDGE".to_string())),
        }
    }

    fn parse_create_collection(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // COLLECTION
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

    fn parse_create_edge(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // EDGE
        let name = self.expect_ident()?;
        self.expect(&Token::From)?;
        let from_collection = self.expect_ident()?;
        self.expect(&Token::To)?;
        let to_collection = self.expect_ident()?;
        self.expect(&Token::Semicolon)?;
        Ok(Statement::CreateEdgeType(CreateEdgeTypeStmt {
            name,
            from_collection,
            to_collection,
        }))
    }
```

- [ ] **Step 4: Update parser — parse_insert branching**

The current `parse_insert` expects `INSERT INTO`. Change it to branch on INTO vs EDGE:

```rust
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
```

- [ ] **Step 5: Add parse_delete and parse_traverse**

Add to `parse_statement` match arms:

```rust
            Some(Token::Delete) => self.parse_delete(),
            Some(Token::Traverse) => self.parse_traverse(),
```

Add the methods:

```rust
    fn parse_delete(&mut self) -> Result<Statement, ParseError> {
        self.advance(); // DELETE
        self.expect(&Token::Edge)?;
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
```

- [ ] **Step 6: Add helper methods**

Add `expect_string_lit` and `parse_kv_list` to the Parser:

```rust
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
```

- [ ] **Step 7: Update lib.rs re-exports**

In `crates/trondb-tql/src/lib.rs`, the existing `pub use ast::*;` will automatically export the new AST types. No changes needed unless you want to be explicit.

- [ ] **Step 8: Add parser tests**

Add to `crates/trondb-tql/src/parser.rs` test module:

```rust
    #[test]
    fn parse_create_edge() {
        let stmt = parse("CREATE EDGE knows FROM people TO people;").unwrap();
        assert_eq!(
            stmt,
            Statement::CreateEdgeType(CreateEdgeTypeStmt {
                name: "knows".to_string(),
                from_collection: "people".to_string(),
                to_collection: "people".to_string(),
            })
        );
    }

    #[test]
    fn parse_insert_edge() {
        let stmt = parse("INSERT EDGE knows FROM 'v1' TO 'v2';").unwrap();
        assert_eq!(
            stmt,
            Statement::InsertEdge(InsertEdgeStmt {
                edge_type: "knows".to_string(),
                from_id: "v1".to_string(),
                to_id: "v2".to_string(),
                metadata: vec![],
            })
        );
    }

    #[test]
    fn parse_insert_edge_with_metadata() {
        let stmt = parse("INSERT EDGE knows FROM 'v1' TO 'v2' WITH (since = '2024', weight = 42);").unwrap();
        match &stmt {
            Statement::InsertEdge(e) => {
                assert_eq!(e.metadata.len(), 2);
                assert_eq!(e.metadata[0], ("since".to_string(), Literal::String("2024".to_string())));
                assert_eq!(e.metadata[1], ("weight".to_string(), Literal::Int(42)));
            }
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
        assert_eq!(
            stmt,
            Statement::Traverse(TraverseStmt {
                edge_type: "knows".to_string(),
                from_id: "v1".to_string(),
                depth: 1,
                limit: Some(10),
            })
        );
    }
```

- [ ] **Step 9: Add token test**

Add to `crates/trondb-tql/src/token.rs` test module:

```rust
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
```

- [ ] **Step 10: Verify and commit**

Run: `cargo test -p trondb-tql`
Expected: All existing tests + ~7 new tests pass.

```bash
git add crates/trondb-tql/
git commit -m "feat(tql): add edge and traverse grammar — CREATE EDGE, INSERT EDGE, DELETE EDGE, TRAVERSE

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Chunk 2: Edge Types, AdjacencyIndex, Store, and Planner

### Task 2: Create edge types and AdjacencyIndex

**Files:**
- Create: `crates/trondb-core/src/edge.rs`
- Modify: `crates/trondb-core/src/lib.rs` (add `pub mod edge;`)

- [ ] **Step 1: Create edge.rs**

Create `crates/trondb-core/src/edge.rs`:

```rust
use std::collections::HashMap;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::types::{LogicalId, Value};

// ---------------------------------------------------------------------------
// Edge — a directional relationship between two entities
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from_id: LogicalId,
    pub to_id: LogicalId,
    pub edge_type: String,
    pub confidence: f32,
    pub metadata: HashMap<String, Value>,
}

// ---------------------------------------------------------------------------
// EdgeType — schema declaration for a class of edges
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeType {
    pub name: String,
    pub from_collection: String,
    pub to_collection: String,
    pub decay_config: DecayConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecayConfig {
    pub decay_fn: Option<DecayFn>,
    pub decay_rate: Option<f64>,
    pub floor: Option<f64>,
    pub promote_threshold: Option<f64>,
    pub prune_threshold: Option<f64>,
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self {
            decay_fn: None,
            decay_rate: None,
            floor: None,
            promote_threshold: None,
            prune_threshold: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DecayFn {
    Exponential,
    Linear,
    Step,
}

// ---------------------------------------------------------------------------
// AdjacencyIndex — RAM index for fast TRAVERSE
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AdjEntry {
    pub to_id: LogicalId,
    pub confidence: f32,
}

pub struct AdjacencyIndex {
    forward: DashMap<(LogicalId, String), Vec<AdjEntry>>,
}

impl AdjacencyIndex {
    pub fn new() -> Self {
        Self {
            forward: DashMap::new(),
        }
    }

    pub fn insert(&self, from_id: &LogicalId, edge_type: &str, to_id: &LogicalId, confidence: f32) {
        let key = (from_id.clone(), edge_type.to_string());
        let entry = AdjEntry {
            to_id: to_id.clone(),
            confidence,
        };
        self.forward
            .entry(key)
            .or_insert_with(Vec::new)
            .push(entry);
    }

    pub fn remove(&self, from_id: &LogicalId, edge_type: &str, to_id: &LogicalId) {
        let key = (from_id.clone(), edge_type.to_string());
        if let Some(mut entries) = self.forward.get_mut(&key) {
            entries.retain(|e| e.to_id != *to_id);
            if entries.is_empty() {
                drop(entries);
                self.forward.remove(&key);
            }
        }
    }

    pub fn get(&self, from_id: &LogicalId, edge_type: &str) -> Vec<AdjEntry> {
        let key = (from_id.clone(), edge_type.to_string());
        self.forward
            .get(&key)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.forward.iter().map(|e| e.value().len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }
}

impl Default for AdjacencyIndex {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_id(s: &str) -> LogicalId {
        LogicalId::from_string(s)
    }

    #[test]
    fn insert_and_get() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0);
        idx.insert(&make_id("v1"), "knows", &make_id("v3"), 1.0);

        let results = idx.get(&make_id("v1"), "knows");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn get_empty() {
        let idx = AdjacencyIndex::new();
        let results = idx.get(&make_id("v1"), "knows");
        assert!(results.is_empty());
    }

    #[test]
    fn remove_edge() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0);
        idx.insert(&make_id("v1"), "knows", &make_id("v3"), 1.0);

        idx.remove(&make_id("v1"), "knows", &make_id("v2"));
        let results = idx.get(&make_id("v1"), "knows");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].to_id, make_id("v3"));
    }

    #[test]
    fn remove_last_edge_cleans_key() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0);
        idx.remove(&make_id("v1"), "knows", &make_id("v2"));
        assert!(idx.is_empty());
    }

    #[test]
    fn different_edge_types_separate() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0);
        idx.insert(&make_id("v1"), "likes", &make_id("v3"), 0.8);

        assert_eq!(idx.get(&make_id("v1"), "knows").len(), 1);
        assert_eq!(idx.get(&make_id("v1"), "likes").len(), 1);
    }

    #[test]
    fn len_counts_all_edges() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0);
        idx.insert(&make_id("v1"), "knows", &make_id("v3"), 1.0);
        idx.insert(&make_id("v2"), "likes", &make_id("v1"), 0.5);
        assert_eq!(idx.len(), 3);
    }
}
```

- [ ] **Step 2: Add module to lib.rs**

Add `pub mod edge;` to `crates/trondb-core/src/lib.rs` after `pub mod index;`.

- [ ] **Step 3: Verify and commit**

Run: `cargo test --workspace`
Expected: All tests pass.

```bash
git add crates/trondb-core/src/edge.rs crates/trondb-core/src/lib.rs
git commit -m "feat(edge): add Edge, EdgeType, DecayConfig types and AdjacencyIndex

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 3: Add error variants and FjallStore edge methods

**Files:**
- Modify: `crates/trondb-core/src/error.rs` (add error variants)
- Modify: `crates/trondb-core/src/store.rs` (add edge storage methods)

- [ ] **Step 1: Add error variants**

In `crates/trondb-core/src/error.rs`, add to the `EngineError` enum:

```rust
    #[error("edge type not found: {0}")]
    EdgeTypeNotFound(String),

    #[error("edge type already exists: {0}")]
    EdgeTypeAlreadyExists(String),
```

- [ ] **Step 2: Add FjallStore edge methods**

In `crates/trondb-core/src/store.rs`, add constants after the existing ones:

```rust
const EDGE_TYPE_PREFIX: &str = "edge_type:";
const EDGE_PREFIX: &str = "edge:";
```

Add these methods to the `impl FjallStore` block, before `fn get_collection_meta`:

```rust
    // --- Edge Type methods ---

    pub fn create_edge_type(&self, edge_type: &crate::edge::EdgeType) -> Result<(), EngineError> {
        let key = format!("{EDGE_TYPE_PREFIX}{}", edge_type.name);
        if self.meta.get(&key)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?
            .is_some()
        {
            return Err(EngineError::EdgeTypeAlreadyExists(edge_type.name.clone()));
        }

        let bytes = rmp_serde::to_vec_named(edge_type)
            .map_err(|e| EngineError::Storage(e.to_string()))?;
        self.meta.insert(&key, bytes)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?;

        // Create partition for this edge type's edges
        let partition_name = format!("edges:{}", edge_type.name);
        self.keyspace
            .open_partition(&partition_name, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        self.keyspace.persist(PersistMode::SyncAll)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        Ok(())
    }

    pub fn get_edge_type(&self, name: &str) -> Result<crate::edge::EdgeType, EngineError> {
        let key = format!("{EDGE_TYPE_PREFIX}{name}");
        let bytes = self.meta.get(&key)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?
            .ok_or_else(|| EngineError::EdgeTypeNotFound(name.to_owned()))?;
        rmp_serde::from_slice(&bytes).map_err(|e| EngineError::Storage(e.to_string()))
    }

    pub fn has_edge_type(&self, name: &str) -> bool {
        let key = format!("{EDGE_TYPE_PREFIX}{name}");
        self.meta.get(&key).ok().flatten().is_some()
    }

    pub fn list_edge_types(&self) -> Vec<crate::edge::EdgeType> {
        self.meta
            .prefix(EDGE_TYPE_PREFIX)
            .filter_map(|kv| {
                let (_k, v) = kv.ok()?;
                rmp_serde::from_slice(&v).ok()
            })
            .collect()
    }

    // --- Edge methods ---

    pub fn insert_edge(&self, edge: &crate::edge::Edge) -> Result<(), EngineError> {
        let partition_name = format!("edges:{}", edge.edge_type);
        let partition = self.keyspace
            .open_partition(&partition_name, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let key = format!("{EDGE_PREFIX}{}:{}", edge.from_id, edge.to_id);
        let bytes = rmp_serde::to_vec_named(edge)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        partition.insert(&key, bytes)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn delete_edge(&self, edge_type: &str, from_id: &str, to_id: &str) -> Result<(), EngineError> {
        let partition_name = format!("edges:{edge_type}");
        let partition = self.keyspace
            .open_partition(&partition_name, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let key = format!("{EDGE_PREFIX}{from_id}:{to_id}");
        partition.remove(&key)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn scan_edges(&self, edge_type: &str) -> Result<Vec<crate::edge::Edge>, EngineError> {
        let partition_name = format!("edges:{edge_type}");
        let partition = self.keyspace
            .open_partition(&partition_name, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let edges: Vec<crate::edge::Edge> = partition
            .prefix(EDGE_PREFIX)
            .filter_map(|kv| {
                let (_k, v) = kv.ok()?;
                rmp_serde::from_slice(&v).ok()
            })
            .collect();

        Ok(edges)
    }
```

- [ ] **Step 3: Verify and commit**

Run: `cargo test --workspace`
Expected: All tests pass. New methods are added but not yet called.

```bash
git add crates/trondb-core/src/error.rs crates/trondb-core/src/store.rs
git commit -m "feat(edge): add error variants and FjallStore edge storage methods

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 4: Add Plan types for edge operations

**Files:**
- Modify: `crates/trondb-core/src/planner.rs`

- [ ] **Step 1: Add Plan variants and plan structs**

In `crates/trondb-core/src/planner.rs`, add to the `Plan` enum:

```rust
    CreateEdgeType(CreateEdgeTypePlan),
    InsertEdge(InsertEdgePlan),
    DeleteEdge(DeleteEdgePlan),
    Traverse(TraversePlan),
```

Add the plan structs after the existing ones:

```rust
#[derive(Debug, Clone)]
pub struct CreateEdgeTypePlan {
    pub name: String,
    pub from_collection: String,
    pub to_collection: String,
}

#[derive(Debug, Clone)]
pub struct InsertEdgePlan {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
    pub metadata: Vec<(String, trondb_tql::Literal)>,
}

#[derive(Debug, Clone)]
pub struct DeleteEdgePlan {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
}

#[derive(Debug, Clone)]
pub struct TraversePlan {
    pub edge_type: String,
    pub from_id: String,
    pub depth: usize,
    pub limit: Option<usize>,
}
```

- [ ] **Step 2: Add planner match arms**

In the `plan()` function, add match arms:

```rust
        Statement::CreateEdgeType(s) => Ok(Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: s.name.clone(),
            from_collection: s.from_collection.clone(),
            to_collection: s.to_collection.clone(),
        })),

        Statement::InsertEdge(s) => Ok(Plan::InsertEdge(InsertEdgePlan {
            edge_type: s.edge_type.clone(),
            from_id: s.from_id.clone(),
            to_id: s.to_id.clone(),
            metadata: s.metadata.clone(),
        })),

        Statement::DeleteEdge(s) => Ok(Plan::DeleteEdge(DeleteEdgePlan {
            edge_type: s.edge_type.clone(),
            from_id: s.from_id.clone(),
            to_id: s.to_id.clone(),
        })),

        Statement::Traverse(s) => Ok(Plan::Traverse(TraversePlan {
            edge_type: s.edge_type.clone(),
            from_id: s.from_id.clone(),
            depth: s.depth,
            limit: s.limit,
        })),
```

- [ ] **Step 3: Verify and commit**

Run: `cargo test --workspace`
Expected: All tests pass.

```bash
git add crates/trondb-core/src/planner.rs
git commit -m "feat(planner): add Plan types for edge operations and TRAVERSE

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Chunk 3: Executor Wiring

### Task 5: Execute CREATE EDGE TYPE, INSERT EDGE, DELETE EDGE

**Files:**
- Modify: `crates/trondb-core/src/executor.rs`

- [ ] **Step 1: Add imports and fields**

Add to imports in executor.rs:

```rust
use crate::edge::{AdjacencyIndex, Edge, EdgeType, DecayConfig};
```

Add fields to the Executor struct:

```rust
pub struct Executor {
    store: FjallStore,
    wal: WalWriter,
    location: Arc<LocationTable>,
    indexes: DashMap<String, HnswIndex>,
    adjacency: AdjacencyIndex,              // NEW
    edge_types: DashMap<String, EdgeType>,   // NEW
}
```

Update constructor:

```rust
    pub fn new(store: FjallStore, wal: WalWriter, location: Arc<LocationTable>) -> Self {
        Self {
            store,
            wal,
            location,
            indexes: DashMap::new(),
            adjacency: AdjacencyIndex::new(),
            edge_types: DashMap::new(),
        }
    }
```

- [ ] **Step 2: Add CREATE EDGE TYPE execution**

Add a new match arm in `execute()`, before `Plan::Explain`:

```rust
            Plan::CreateEdgeType(p) => {
                // Validate from/to collections exist
                if !self.store.has_collection(&p.from_collection) {
                    return Err(EngineError::CollectionNotFound(p.from_collection.clone()));
                }
                if !self.store.has_collection(&p.to_collection) {
                    return Err(EngineError::CollectionNotFound(p.to_collection.clone()));
                }

                let edge_type = EdgeType {
                    name: p.name.clone(),
                    from_collection: p.from_collection.clone(),
                    to_collection: p.to_collection.clone(),
                    decay_config: DecayConfig::default(),
                };

                // WAL: TxBegin → SchemaCreateEdgeType → commit
                let tx_id = self.wal.next_tx_id();
                self.wal.append(RecordType::TxBegin, &p.name, tx_id, 1, vec![]);
                let payload = rmp_serde::to_vec_named(&edge_type)
                    .map_err(|e| EngineError::Storage(e.to_string()))?;
                self.wal.append(RecordType::SchemaCreateEdgeType, &p.name, tx_id, 1, payload);
                self.wal.commit(tx_id).await?;

                // Apply to Fjall
                self.store.create_edge_type(&edge_type)?;

                // Register in memory
                self.edge_types.insert(p.name.clone(), edge_type);

                Ok(QueryResult {
                    columns: vec!["result".into()],
                    rows: vec![Row {
                        values: HashMap::from([(
                            "result".into(),
                            Value::String(format!(
                                "Edge type '{}' created ({} → {})",
                                p.name, p.from_collection, p.to_collection
                            )),
                        )]),
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

- [ ] **Step 3: Add INSERT EDGE execution**

```rust
            Plan::InsertEdge(p) => {
                // Validate edge type exists
                let edge_type = self.store.get_edge_type(&p.edge_type)?;

                // Validate from/to entities exist
                let from_id = LogicalId::from_string(&p.from_id);
                let to_id = LogicalId::from_string(&p.to_id);
                self.store.get(&edge_type.from_collection, &from_id)?;
                self.store.get(&edge_type.to_collection, &to_id)?;

                // Build metadata
                let mut metadata = HashMap::new();
                for (key, lit) in &p.metadata {
                    metadata.insert(key.clone(), literal_to_value(lit));
                }

                let edge = Edge {
                    from_id: from_id.clone(),
                    to_id: to_id.clone(),
                    edge_type: p.edge_type.clone(),
                    confidence: 1.0,
                    metadata,
                };

                // WAL: TxBegin → EdgeWrite → commit
                let tx_id = self.wal.next_tx_id();
                self.wal.append(RecordType::TxBegin, &p.edge_type, tx_id, 1, vec![]);
                let payload = rmp_serde::to_vec_named(&edge)
                    .map_err(|e| EngineError::Storage(e.to_string()))?;
                self.wal.append(RecordType::EdgeWrite, &p.edge_type, tx_id, 1, payload);
                self.wal.commit(tx_id).await?;

                // Apply to Fjall
                self.store.insert_edge(&edge)?;
                self.store.persist()?;

                // Apply to AdjacencyIndex
                self.adjacency.insert(&from_id, &p.edge_type, &to_id, 1.0);

                Ok(QueryResult {
                    columns: vec!["result".into()],
                    rows: vec![Row {
                        values: HashMap::from([(
                            "result".into(),
                            Value::String(format!(
                                "Edge '{}' created: {} → {}",
                                p.edge_type, p.from_id, p.to_id
                            )),
                        )]),
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

- [ ] **Step 4: Add DELETE EDGE execution**

```rust
            Plan::DeleteEdge(p) => {
                let from_id = LogicalId::from_string(&p.from_id);
                let to_id = LogicalId::from_string(&p.to_id);

                // WAL: TxBegin → EdgeDelete → commit
                let tx_id = self.wal.next_tx_id();
                self.wal.append(RecordType::TxBegin, &p.edge_type, tx_id, 1, vec![]);
                let payload = rmp_serde::to_vec_named(&serde_json::json!({
                    "edge_type": p.edge_type,
                    "from_id": p.from_id,
                    "to_id": p.to_id,
                }))
                .map_err(|e| EngineError::Storage(e.to_string()))?;
                self.wal.append(RecordType::EdgeDelete, &p.edge_type, tx_id, 1, payload);
                self.wal.commit(tx_id).await?;

                // Apply to Fjall
                self.store.delete_edge(&p.edge_type, &p.from_id, &p.to_id)?;
                self.store.persist()?;

                // Apply to AdjacencyIndex
                self.adjacency.remove(&from_id, &p.edge_type, &to_id);

                Ok(QueryResult {
                    columns: vec!["result".into()],
                    rows: vec![Row {
                        values: HashMap::from([(
                            "result".into(),
                            Value::String(format!(
                                "Edge '{}' deleted: {} → {}",
                                p.edge_type, p.from_id, p.to_id
                            )),
                        )]),
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

- [ ] **Step 5: Add accessor methods**

After the existing accessor methods, add:

```rust
    pub fn adjacency(&self) -> &AdjacencyIndex {
        &self.adjacency
    }

    pub fn edge_types(&self) -> &DashMap<String, EdgeType> {
        &self.edge_types
    }

    pub fn list_edge_types(&self) -> Vec<EdgeType> {
        self.store.list_edge_types()
    }

    pub fn scan_edges(&self, edge_type: &str) -> Result<Vec<Edge>, EngineError> {
        self.store.scan_edges(edge_type)
    }
```

- [ ] **Step 6: Verify and commit**

Run: `cargo check --workspace` (some tests may fail due to missing TRAVERSE arm — that's Task 6).

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "feat(edge): execute CREATE EDGE TYPE, INSERT EDGE, DELETE EDGE

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 6: Execute TRAVERSE + EXPLAIN TRAVERSE

**Files:**
- Modify: `crates/trondb-core/src/executor.rs`

- [ ] **Step 1: Add TRAVERSE execution**

Add a match arm for `Plan::Traverse` in `execute()`:

```rust
            Plan::Traverse(p) => {
                // Validate edge type exists
                let edge_type = self.store.get_edge_type(&p.edge_type)?;

                // Gate multi-hop
                if p.depth > 1 {
                    return Err(EngineError::UnsupportedOperation(
                        "TRAVERSE DEPTH > 1 requires Phase 6+".into(),
                    ));
                }

                let from_id = LogicalId::from_string(&p.from_id);
                let entries = self.adjacency.get(&from_id, &p.edge_type);

                let mut rows = Vec::new();
                for entry in &entries {
                    if let Ok(entity) = self.store.get(&edge_type.to_collection, &entry.to_id) {
                        rows.push(entity_to_row(&entity, &FieldList::All));
                    }
                }

                if let Some(limit) = p.limit {
                    rows.truncate(limit);
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

- [ ] **Step 2: Add EXPLAIN for edge plans**

In the `explain_plan()` function, add match arms:

```rust
        Plan::CreateEdgeType(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "CREATE EDGE".into()));
            props.push(("edge_type", p.name.clone()));
            props.push(("from_collection", p.from_collection.clone()));
            props.push(("to_collection", p.to_collection.clone()));
        }
        Plan::InsertEdge(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "INSERT EDGE".into()));
            props.push(("edge_type", p.edge_type.clone()));
            props.push(("tier", "Fjall".into()));
        }
        Plan::DeleteEdge(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "DELETE EDGE".into()));
            props.push(("edge_type", p.edge_type.clone()));
            props.push(("tier", "Fjall".into()));
        }
        Plan::Traverse(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "TRAVERSE".into()));
            props.push(("edge_type", p.edge_type.clone()));
            props.push(("tier", "Ram".into()));
            props.push(("strategy", "AdjacencyIndex".into()));
            props.push(("depth", p.depth.to_string()));
        }
```

- [ ] **Step 3: Verify and commit**

Run: `cargo test --workspace`
Expected: All tests pass.

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "feat(edge): execute TRAVERSE with AdjacencyIndex lookup and EXPLAIN

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Chunk 4: WAL Replay, Rebuild, Tests, Cleanup

### Task 7: WAL replay for edge records + Engine::open rebuild

**Files:**
- Modify: `crates/trondb-core/src/executor.rs` (replay_wal_records)
- Modify: `crates/trondb-core/src/lib.rs` (Engine::open rebuild)

- [ ] **Step 1: Add WAL replay handlers**

In `executor.rs`, in `replay_wal_records`, add cases to the match on `record.record_type`:

```rust
                RecordType::SchemaCreateEdgeType => {
                    let edge_type: crate::edge::EdgeType = rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                    if !self.store.has_edge_type(&edge_type.name) {
                        self.store.create_edge_type(&edge_type)?;
                        replayed += 1;
                    }
                    self.edge_types.insert(edge_type.name.clone(), edge_type);
                }
                RecordType::EdgeWrite => {
                    let edge: crate::edge::Edge = rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                    if self.store.has_edge_type(&edge.edge_type) {
                        self.store.insert_edge(&edge)?;
                        replayed += 1;
                    }
                }
                RecordType::EdgeDelete => {
                    #[derive(serde::Deserialize)]
                    struct EdgeDeletePayload {
                        edge_type: String,
                        from_id: String,
                        to_id: String,
                    }
                    let payload: EdgeDeletePayload = rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                    self.store.delete_edge(&payload.edge_type, &payload.from_id, &payload.to_id)?;
                    replayed += 1;
                }
```

- [ ] **Step 2: Add AdjacencyIndex rebuild in Engine::open**

In `crates/trondb-core/src/lib.rs`, in `Engine::open`, after the HNSW rebuild block, add:

```rust
        // Rebuild AdjacencyIndex from Fjall
        let edge_type_list = executor.list_edge_types();
        for et in &edge_type_list {
            if let Ok(edges) = executor.scan_edges(&et.name) {
                for edge in &edges {
                    executor.adjacency().insert(
                        &edge.from_id,
                        &edge.edge_type,
                        &edge.to_id,
                        edge.confidence,
                    );
                }
            }
            executor.edge_types().insert(et.name.clone(), et.clone());
        }
```

- [ ] **Step 3: Verify and commit**

Run: `cargo test --workspace`
Expected: All tests pass.

```bash
git add crates/trondb-core/src/executor.rs crates/trondb-core/src/lib.rs
git commit -m "feat(edge): add WAL replay for edge records and AdjacencyIndex rebuild on startup

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 8: Integration tests for edges

**Files:**
- Modify: `crates/trondb-core/src/lib.rs` (test module)

- [ ] **Step 1: Add edge integration tests**

Add these tests to the `#[cfg(test)] mod tests` section in `crates/trondb-core/src/lib.rs`:

```rust
    #[tokio::test]
    async fn create_edge_type_and_traverse() {
        let (engine, _dir) = test_engine().await;

        engine.execute_tql("CREATE COLLECTION people WITH DIMENSIONS 3;").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p1', 'Alice');").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p2', 'Bob');").await.unwrap();

        engine.execute_tql("CREATE EDGE knows FROM people TO people;").await.unwrap();
        engine.execute_tql("INSERT EDGE knows FROM 'p1' TO 'p2';").await.unwrap();

        let result = engine.execute_tql("TRAVERSE knows FROM 'p1';").await.unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("name"),
            Some(&Value::String("Bob".into()))
        );
    }

    #[tokio::test]
    async fn delete_edge_removes_from_traverse() {
        let (engine, _dir) = test_engine().await;

        engine.execute_tql("CREATE COLLECTION people WITH DIMENSIONS 3;").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p1', 'Alice');").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p2', 'Bob');").await.unwrap();

        engine.execute_tql("CREATE EDGE knows FROM people TO people;").await.unwrap();
        engine.execute_tql("INSERT EDGE knows FROM 'p1' TO 'p2';").await.unwrap();
        engine.execute_tql("DELETE EDGE knows FROM 'p1' TO 'p2';").await.unwrap();

        let result = engine.execute_tql("TRAVERSE knows FROM 'p1';").await.unwrap();
        assert!(result.rows.is_empty());
    }

    #[tokio::test]
    async fn insert_edge_with_metadata() {
        let (engine, _dir) = test_engine().await;

        engine.execute_tql("CREATE COLLECTION people WITH DIMENSIONS 3;").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p1', 'Alice');").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p2', 'Bob');").await.unwrap();

        engine.execute_tql("CREATE EDGE knows FROM people TO people;").await.unwrap();
        engine.execute_tql("INSERT EDGE knows FROM 'p1' TO 'p2' WITH (since = '2024');").await.unwrap();

        // Edge was created successfully (metadata stored in Fjall)
        let result = engine.execute_tql("TRAVERSE knows FROM 'p1';").await.unwrap();
        assert_eq!(result.rows.len(), 1);
    }

    #[tokio::test]
    async fn insert_edge_nonexistent_type_fails() {
        let (engine, _dir) = test_engine().await;

        engine.execute_tql("CREATE COLLECTION people WITH DIMENSIONS 3;").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p1', 'Alice');").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p2', 'Bob');").await.unwrap();

        let result = engine.execute_tql("INSERT EDGE knows FROM 'p1' TO 'p2';").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn traverse_nonexistent_edge_type_fails() {
        let (engine, _dir) = test_engine().await;
        let result = engine.execute_tql("TRAVERSE knows FROM 'p1';").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn traverse_depth_gt_1_fails() {
        let (engine, _dir) = test_engine().await;

        engine.execute_tql("CREATE COLLECTION people WITH DIMENSIONS 3;").await.unwrap();
        engine.execute_tql("CREATE EDGE knows FROM people TO people;").await.unwrap();

        let result = engine.execute_tql("TRAVERSE knows FROM 'p1' DEPTH 2;").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn explain_traverse_shows_adjacency_index() {
        let (engine, _dir) = test_engine().await;

        let result = engine
            .execute_tql("EXPLAIN TRAVERSE knows FROM 'p1';")
            .await
            .unwrap();

        let strategy_row = result.rows.iter()
            .find(|r| r.values.get("property") == Some(&Value::String("strategy".into())))
            .expect("should have 'strategy' property");
        assert_eq!(
            strategy_row.values.get("value"),
            Some(&Value::String("AdjacencyIndex".into()))
        );

        let mode_row = result.rows.iter()
            .find(|r| r.values.get("property") == Some(&Value::String("mode".into())))
            .expect("should have 'mode' property");
        assert_eq!(
            mode_row.values.get("value"),
            Some(&Value::String("Deterministic".into()))
        );
    }

    #[tokio::test]
    async fn edges_survive_restart() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("data"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
        };

        // Insert edge
        {
            let engine = Engine::open(config.clone()).await.unwrap();
            engine.execute_tql("CREATE COLLECTION people WITH DIMENSIONS 3;").await.unwrap();
            engine.execute_tql("INSERT INTO people (id, name) VALUES ('p1', 'Alice');").await.unwrap();
            engine.execute_tql("INSERT INTO people (id, name) VALUES ('p2', 'Bob');").await.unwrap();
            engine.execute_tql("CREATE EDGE knows FROM people TO people;").await.unwrap();
            engine.execute_tql("INSERT EDGE knows FROM 'p1' TO 'p2';").await.unwrap();
        }

        // Reopen — AdjacencyIndex should be rebuilt
        {
            let engine = Engine::open(config).await.unwrap();
            let result = engine.execute_tql("TRAVERSE knows FROM 'p1';").await.unwrap();
            assert_eq!(result.rows.len(), 1);
            assert_eq!(
                result.rows[0].values.get("name"),
                Some(&Value::String("Bob".into()))
            );
        }
    }
```

- [ ] **Step 2: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass (existing 89 + ~8 new edge tests).

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-core/src/lib.rs
git commit -m "test(edge): add integration tests for edges, traverse, restart, explain

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 9: Update CLAUDE.md

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update CLAUDE.md**

Replace contents of `CLAUDE.md`:

```markdown
# TronDB

Inference-first storage engine. Phase 5: Structural edges + TRAVERSE.

## Project Structure

- `crates/trondb-wal/` — Write-Ahead Log: record types (MessagePack), segment files, buffer, async writer, crash recovery
- `crates/trondb-core/` — Engine: types, Fjall-backed store, Location Table (DashMap), HNSW index (hnsw_rs), edges (AdjacencyIndex), planner, async executor. Depends on trondb-wal + trondb-tql.
- `crates/trondb-tql/` — TQL parser (logos lexer + recursive descent). No engine dependency.
- `crates/trondb-cli/` — Interactive REPL binary (Tokio + rustyline). Depends on all crates.

## Conventions

- Rust 2021 edition, async (Tokio runtime)
- Tests: `cargo test --workspace`
- Run REPL: `cargo run -p trondb-cli`
- Run REPL with custom data dir: `cargo run -p trondb-cli -- --data-dir /path/to/data`
- TQL is case-insensitive for keywords, case-sensitive for identifiers
- All vectors are Float32 (gated until Phase 7)
- LogicalId is a String wrapper (user-provided or UUID v4)
- Persistence: Fjall (LSM-based). Data dir default: ./trondb_data
- WAL: MessagePack records, CRC32 verified, segment files. Dir: {data_dir}/wal/
- Write path: WAL append → flush+fsync → apply to Fjall + Location Table + HNSW index + AdjacencyIndex → ack
- Location Table: DashMap-backed control fabric, always in RAM
  - Tracks LocationDescriptor per representation (tier, state, encoding, node address)
  - WAL-logged via RecordType::LocationUpdate (0x40)
  - Periodically snapshotted to {data_dir}/location_table.snap
  - Restored from snapshot + WAL replay on startup
  - State machine: Clean → Dirty → Recomputing → Clean; Clean → Migrating → Clean
- HNSW Index: per-collection vector index, always in RAM (control fabric)
  - Uses hnsw_rs with DistCosine (cosine similarity)
  - Rebuilt from Fjall on startup (no persistence)
  - SEARCH returns results with similarity scores, filtered by confidence threshold
  - QueryMode::Probabilistic for SEARCH, QueryMode::Deterministic for FETCH
- Edges: schema-first structural edges with AdjacencyIndex
  - Edge types declared via CREATE EDGE, stored in Fjall
  - Edges stored in Fjall per-type partitions (edges:{type})
  - AdjacencyIndex (DashMap) in RAM for fast TRAVERSE, rebuilt from Fjall on startup
  - Structural edges have confidence=1.0, no decay
  - TRAVERSE returns connected entities (single-hop, DEPTH > 1 gated)
  - DecayConfig fields defined but not driven until Phase 6
```

- [ ] **Step 2: Run tests and clippy**

Run: `cargo test --workspace && cargo clippy --workspace`
Expected: All tests pass, clippy clean.

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: update CLAUDE.md for Phase 5 — structural edges + TRAVERSE

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```
