# TronDB PoC Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an in-memory inference-first storage engine with TQL parser, HNSW vector index, and interactive REPL — validating that positioned entities can be queried in deterministic and probabilistic modes through a unified language.

**Architecture:** Three-crate Rust workspace. `trondb-tql` parses TQL into an AST. `trondb-core` stores entities in DashMap, indexes vectors in HNSW, plans and executes queries. `trondb-cli` bridges them in an interactive REPL. Vertical slices: each query verb is built end-to-end (grammar → AST → planner → executor → test).

**Tech Stack:** Rust (2021 edition), logos (lexer), dashmap (concurrent store), hnsw_rs (vector index), rustyline (REPL), comfy-table (output formatting), uuid, bytes, sha2, serde/serde_json.

**Spec:** `docs/superpowers/specs/2026-03-10-trondb-poc-design.md`

---

## File Structure

### `trondb-tql` crate (parser — no engine dependency)

| File | Responsibility |
|------|---------------|
| `crates/trondb-tql/src/lib.rs` | Public API: `parse(input) -> Result<Statement>` |
| `crates/trondb-tql/src/token.rs` | Token enum with logos derive macros |
| `crates/trondb-tql/src/ast.rs` | AST types: Statement, FetchStatement, SearchStatement, etc. |
| `crates/trondb-tql/src/parser.rs` | Recursive descent parser: tokens → AST |
| `crates/trondb-tql/src/error.rs` | Parse error types with span information |
| `crates/trondb-tql/Cargo.toml` | Crate manifest |

### `trondb-core` crate (engine — no parser dependency)

| File | Responsibility |
|------|---------------|
| `crates/trondb-core/src/lib.rs` | Public API: `Engine` struct |
| `crates/trondb-core/src/types.rs` | LogicalId, Entity, Representation, Value, Collection, FieldDef |
| `crates/trondb-core/src/store.rs` | In-memory store: DashMap per collection, insert/get/scan |
| `crates/trondb-core/src/index.rs` | HNSW vector index wrapper: insert vectors, query nearest neighbours |
| `crates/trondb-core/src/planner.rs` | AST → Plan: FetchPlan, SearchPlan, ExplainPlan |
| `crates/trondb-core/src/executor.rs` | Plan → QueryResult: executes plans against store + index |
| `crates/trondb-core/src/result.rs` | QueryResult, Row, QueryStats types |
| `crates/trondb-core/src/error.rs` | Engine error types |
| `crates/trondb-core/Cargo.toml` | Crate manifest |

### `trondb-cli` crate (REPL binary)

| File | Responsibility |
|------|---------------|
| `crates/trondb-cli/src/main.rs` | REPL loop: read → parse → execute → display |
| `crates/trondb-cli/src/display.rs` | Pretty-print QueryResult as tables |
| `crates/trondb-cli/Cargo.toml` | Crate manifest |

### Workspace root

| File | Responsibility |
|------|---------------|
| `Cargo.toml` | Workspace definition, shared dependencies |
| `CLAUDE.md` | Project conventions for Claude Code |

---

## Chunk 1: Workspace Scaffold + Core Types

### Task 1: Workspace scaffold

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `crates/trondb-core/Cargo.toml`
- Create: `crates/trondb-tql/Cargo.toml`
- Create: `crates/trondb-cli/Cargo.toml`
- Create: `crates/trondb-core/src/lib.rs`
- Create: `crates/trondb-tql/src/lib.rs`
- Create: `crates/trondb-cli/src/main.rs`
- Create: `CLAUDE.md`

- [ ] **Step 1: Create workspace root Cargo.toml**

```toml
[workspace]
resolver = "2"
members = [
    "crates/trondb-core",
    "crates/trondb-tql",
    "crates/trondb-cli",
]

[workspace.dependencies]
uuid = { version = "1", features = ["v4"] }
bytes = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
dashmap = "6"
logos = "0.15"
thiserror = "2"
comfy-table = "7"
rustyline = "15"
hnsw_rs = "0.3"

[profile.release]
lto = true
codegen-units = 1
strip = true
```

- [ ] **Step 2: Create trondb-tql Cargo.toml**

```toml
[package]
name = "trondb-tql"
version = "0.1.0"
edition = "2021"

[dependencies]
logos.workspace = true
thiserror.workspace = true
serde.workspace = true
```

- [ ] **Step 3: Create trondb-core Cargo.toml**

```toml
[package]
name = "trondb-core"
version = "0.1.0"
edition = "2021"

[dependencies]
trondb-tql = { path = "../trondb-tql" }
uuid.workspace = true
bytes.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
dashmap.workspace = true
thiserror.workspace = true
hnsw_rs.workspace = true
```

Note: `trondb-core` depends on `trondb-tql` for AST types. The planner takes AST nodes as input, so core needs to know the AST type definitions. The parser logic stays in trondb-tql — core only imports the types.

- [ ] **Step 4: Create trondb-cli Cargo.toml**

```toml
[package]
name = "trondb-cli"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "trondb"
path = "src/main.rs"

[dependencies]
trondb-core = { path = "../trondb-core" }
trondb-tql = { path = "../trondb-tql" }
rustyline.workspace = true
comfy-table.workspace = true
```

- [ ] **Step 5: Create stub lib.rs and main.rs files**

`crates/trondb-tql/src/lib.rs`:
```rust
pub mod ast;
pub mod token;
pub mod parser;
pub mod error;
```

`crates/trondb-core/src/lib.rs`:
```rust
pub mod types;
pub mod store;
pub mod index;
pub mod planner;
pub mod executor;
pub mod result;
pub mod error;
```

`crates/trondb-cli/src/main.rs`:
```rust
fn main() {
    println!("TronDB v0.1.0");
}
```

- [ ] **Step 6: Create CLAUDE.md**

```markdown
# TronDB

Inference-first storage engine. PoC phase: in-memory, single-node.

## Project Structure

- `crates/trondb-tql/` — TQL parser (logos lexer + recursive descent). No engine dependency.
- `crates/trondb-core/` — Engine: types, in-memory store, HNSW index, planner, executor. Depends on trondb-tql for AST types.
- `crates/trondb-cli/` — Interactive REPL binary. Depends on both.

## Conventions

- Rust 2021 edition, synchronous (no Tokio yet)
- Tests: `cargo test --workspace`
- Run REPL: `cargo run -p trondb-cli`
- TQL is case-insensitive for keywords, case-sensitive for identifiers
- All vectors are Float32 for now
- LogicalId is UUID v4
```

- [ ] **Step 7: Verify workspace compiles**

Run: `cd ~/Projects/TronDB && cargo check --workspace`
Expected: Compiles with warnings about empty modules (that's fine — stubs).

- [ ] **Step 8: Commit**

```bash
git add -A && git commit -m "feat: scaffold workspace with three crates"
```

---

### Task 2: Core types

**Files:**
- Create: `crates/trondb-core/src/types.rs`
- Create: `crates/trondb-core/src/error.rs`
- Create: `crates/trondb-core/src/result.rs`
- Test: `crates/trondb-core/src/types.rs` (unit tests in module)

- [ ] **Step 1: Write tests for core types**

Add to bottom of `crates/trondb-core/src/types.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_id_is_unique() {
        let a = LogicalId::new();
        let b = LogicalId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn logical_id_from_string() {
        let id = LogicalId::from_str("test-id-1");
        assert_eq!(id.as_str(), "test-id-1");
    }

    #[test]
    fn value_display() {
        assert_eq!(Value::String("hello".into()).to_string(), "hello");
        assert_eq!(Value::Int(42).to_string(), "42");
        assert_eq!(Value::Float(3.14).to_string(), "3.14");
        assert_eq!(Value::Bool(true).to_string(), "true");
        assert_eq!(Value::Null.to_string(), "NULL");
    }

    #[test]
    fn entity_builder() {
        let entity = Entity::new(LogicalId::from_str("e1"))
            .with_metadata("name", Value::String("Test".into()))
            .with_metadata("score", Value::Int(100));

        assert_eq!(entity.metadata.get("name"), Some(&Value::String("Test".into())));
        assert_eq!(entity.metadata.len(), 2);
        assert!(entity.representations.is_empty());
    }

    #[test]
    fn collection_validates_dimensions() {
        let result = Collection::new("test", 0);
        assert!(result.is_err());

        let coll = Collection::new("test", 384).unwrap();
        assert_eq!(coll.dimensions, 384);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-core`
Expected: FAIL — types not defined yet.

- [ ] **Step 3: Implement core types**

`crates/trondb-core/src/types.rs`:

```rust
use std::collections::HashMap;
use std::fmt;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// Stable logical identity for an entity, decoupled from physical storage.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LogicalId(String);

impl LogicalId {
    /// Generate a new random UUID-based LogicalId.
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Create a LogicalId from a user-provided string.
    pub fn from_str(s: &str) -> Self {
        Self(s.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LogicalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The storage atom. Carries structural address and semantic addresses.
#[derive(Debug, Clone)]
pub struct Entity {
    pub id: LogicalId,
    pub raw_data: Bytes,
    pub metadata: HashMap<String, Value>,
    pub representations: Vec<Representation>,
    pub schema_version: u32,
}

impl Entity {
    pub fn new(id: LogicalId) -> Self {
        Self {
            id,
            raw_data: Bytes::new(),
            metadata: HashMap::new(),
            representations: Vec::new(),
            schema_version: 1,
        }
    }

    pub fn with_metadata(mut self, key: &str, value: Value) -> Self {
        self.metadata.insert(key.to_string(), value);
        self
    }

    pub fn with_representation(mut self, repr: Representation) -> Self {
        self.representations.push(repr);
        self
    }

    pub fn with_raw_data(mut self, data: Bytes) -> Self {
        self.raw_data = data;
        self
    }
}

/// A single vector representation of an entity.
#[derive(Debug, Clone)]
pub struct Representation {
    pub repr_type: ReprType,
    pub fields: Vec<String>,
    pub vector: Vec<f32>,
    pub recipe_hash: [u8; 32],
    pub state: ReprState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReprType {
    Atomic,
    Composite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReprState {
    Clean,
    Dirty,
}

/// A collection: a namespace with fixed embedding dimensionality.
#[derive(Debug, Clone)]
pub struct Collection {
    pub name: String,
    pub dimensions: usize,
    pub fields: Vec<FieldDef>,
}

impl Collection {
    pub fn new(name: &str, dimensions: usize) -> Result<Self, &'static str> {
        if dimensions == 0 {
            return Err("dimensions must be > 0");
        }
        Ok(Self {
            name: name.to_string(),
            dimensions,
            fields: Vec::new(),
        })
    }
}

/// Schema definition for a field within a collection.
#[derive(Debug, Clone)]
pub struct FieldDef {
    pub name: String,
    pub field_type: FieldType,
    pub indexed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    String,
    Int,
    Float,
    Bool,
}

/// Queryable metadata value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::String(s) => write!(f, "{s}"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(n) => write!(f, "{n}"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Null => write!(f, "NULL"),
        }
    }
}
```

`crates/trondb-core/src/error.rs`:

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("collection '{0}' not found")]
    CollectionNotFound(String),

    #[error("collection '{0}' already exists")]
    CollectionAlreadyExists(String),

    #[error("entity '{0}' not found")]
    EntityNotFound(String),

    #[error("vector dimensionality mismatch: expected {expected}, got {got}")]
    DimensionMismatch { expected: usize, got: usize },

    #[error("invalid query: {0}")]
    InvalidQuery(String),
}
```

`crates/trondb-core/src/result.rs`:

```rust
use std::collections::HashMap;
use std::time::Duration;

use crate::types::Value;

/// Unified result type for all TQL queries.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
    pub stats: QueryStats,
}

/// A single result row: column name → value.
#[derive(Debug, Clone)]
pub struct Row {
    pub values: HashMap<String, Value>,
    /// Similarity score, present only for SEARCH results.
    pub score: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct QueryStats {
    pub elapsed: Duration,
    pub entities_scanned: usize,
    pub mode: QueryMode,
    pub tier: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub enum QueryMode {
    Deterministic,
    Probabilistic,
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-core`
Expected: All 5 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: core types — Entity, LogicalId, Value, Collection, Representation"
```

---

## Chunk 2: TQL Lexer + Parser (CREATE COLLECTION, INSERT, FETCH)

### Task 3: TQL lexer

**Files:**
- Create: `crates/trondb-tql/src/token.rs`
- Create: `crates/trondb-tql/src/error.rs`
- Test: unit tests in `token.rs`

- [ ] **Step 1: Write lexer tests**

Add to bottom of `crates/trondb-tql/src/token.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use logos::Logos;

    fn lex(input: &str) -> Vec<Token> {
        Token::lexer(input)
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn lex_fetch_statement() {
        let tokens = lex("FETCH * FROM venues WHERE id = 'v1';");
        assert_eq!(tokens[0], Token::Fetch);
        assert_eq!(tokens[1], Token::Star);
        assert_eq!(tokens[2], Token::From);
        assert_eq!(tokens[3], Token::Ident("venues".into()));
        assert_eq!(tokens[4], Token::Where);
        assert_eq!(tokens[5], Token::Ident("id".into()));
        assert_eq!(tokens[6], Token::Eq);
        assert_eq!(tokens[7], Token::StringLit("v1".into()));
        assert_eq!(tokens[8], Token::Semicolon);
    }

    #[test]
    fn lex_create_collection() {
        let tokens = lex("CREATE COLLECTION venues WITH DIMENSIONS 384;");
        assert_eq!(tokens[0], Token::Create);
        assert_eq!(tokens[1], Token::Collection);
        assert_eq!(tokens[2], Token::Ident("venues".into()));
        assert_eq!(tokens[3], Token::With);
        assert_eq!(tokens[4], Token::Dimensions);
        assert_eq!(tokens[5], Token::IntLit(384));
        assert_eq!(tokens[6], Token::Semicolon);
    }

    #[test]
    fn lex_search_statement() {
        let tokens = lex("SEARCH venues NEAR VECTOR [1.0, 2.0] CONFIDENCE > 0.8 LIMIT 5;");
        assert_eq!(tokens[0], Token::Search);
        assert_eq!(tokens[3], Token::Vector);
        assert_eq!(tokens[4], Token::LBracket);
        assert_eq!(tokens[5], Token::FloatLit(1.0));
    }

    #[test]
    fn keywords_are_case_insensitive() {
        let upper = lex("FETCH");
        let lower = lex("fetch");
        let mixed = lex("Fetch");
        assert_eq!(upper[0], Token::Fetch);
        assert_eq!(lower[0], Token::Fetch);
        assert_eq!(mixed[0], Token::Fetch);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-tql`
Expected: FAIL — Token type doesn't exist.

- [ ] **Step 3: Implement the lexer**

`crates/trondb-tql/src/token.rs`:

```rust
use logos::Logos;

fn to_lowercase_string(lex: &logos::Lexer<Token>) -> String {
    lex.slice().to_lowercase()
}

#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(skip r"[ \t\r\n\f]+")]
#[logos(skip r"--[^\n]*")]
pub enum Token {
    // Keywords (case-insensitive via callback)
    #[regex("(?i)fetch", priority = 10)]
    Fetch,
    #[regex("(?i)search", priority = 10)]
    Search,
    #[regex("(?i)from", priority = 10)]
    From,
    #[regex("(?i)where", priority = 10)]
    Where,
    #[regex("(?i)and", priority = 10)]
    And,
    #[regex("(?i)or", priority = 10)]
    Or,
    #[regex("(?i)limit", priority = 10)]
    Limit,
    #[regex("(?i)create", priority = 10)]
    Create,
    #[regex("(?i)collection", priority = 10)]
    Collection,
    #[regex("(?i)with", priority = 10)]
    With,
    #[regex("(?i)dimensions", priority = 10)]
    Dimensions,
    #[regex("(?i)insert", priority = 10)]
    Insert,
    #[regex("(?i)into", priority = 10)]
    Into,
    #[regex("(?i)values", priority = 10)]
    Values,
    #[regex("(?i)vector", priority = 10)]
    Vector,
    #[regex("(?i)near", priority = 10)]
    Near,
    #[regex("(?i)confidence", priority = 10)]
    Confidence,
    #[regex("(?i)explain", priority = 10)]
    Explain,
    #[regex("(?i)null", priority = 10)]
    Null,
    #[regex("(?i)true", priority = 10)]
    True,
    #[regex("(?i)false", priority = 10)]
    False,

    // Identifiers (must not match keywords — lower priority)
    #[regex("[a-zA-Z_][a-zA-Z0-9_]*", |lex| lex.slice().to_string(), priority = 1)]
    Ident(String),

    // Literals
    #[regex(r"'[^']*'", |lex| {
        let s = lex.slice();
        s[1..s.len()-1].to_string()
    })]
    StringLit(String),

    #[regex(r"-?[0-9]+\.[0-9]+", |lex| lex.slice().parse::<f64>().ok())]
    FloatLit(f64),

    #[regex(r"-?[0-9]+", |lex| lex.slice().parse::<i64>().ok(), priority = 1)]
    IntLit(i64),

    // Symbols
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
    #[token(">=")]
    Gte,
    #[token("<=")]
    Lte,
    #[token("!=")]
    Neq,
}
```

`crates/trondb-tql/src/error.rs`:

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("unexpected token at position {pos}: expected {expected}, got {got}")]
    UnexpectedToken {
        pos: usize,
        expected: String,
        got: String,
    },

    #[error("unexpected end of input: expected {0}")]
    UnexpectedEof(String),

    #[error("invalid syntax: {0}")]
    InvalidSyntax(String),

    #[error("lexer error at position {0}")]
    LexerError(usize),
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-tql`
Expected: All 4 lexer tests PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: TQL lexer with logos — keywords, literals, symbols"
```

---

### Task 4: AST types

**Files:**
- Create: `crates/trondb-tql/src/ast.rs`

- [ ] **Step 1: Implement AST types**

`crates/trondb-tql/src/ast.rs`:

```rust
/// Top-level TQL statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateCollection(CreateCollectionStmt),
    Insert(InsertStmt),
    Fetch(FetchStmt),
    Search(SearchStmt),
    Explain(Box<Statement>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateCollectionStmt {
    pub name: String,
    pub dimensions: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    pub collection: String,
    pub fields: Vec<String>,
    pub values: Vec<Literal>,
    pub vector: Option<Vec<f64>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FetchStmt {
    pub collection: String,
    pub fields: FieldList,
    pub filter: Option<WhereClause>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchStmt {
    pub collection: String,
    pub fields: FieldList,
    pub near: Vec<f64>,
    pub confidence: Option<f64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldList {
    All,
    Named(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum WhereClause {
    Eq(String, Literal),
    Gt(String, Literal),
    Lt(String, Literal),
    And(Box<WhereClause>, Box<WhereClause>),
    Or(Box<WhereClause>, Box<WhereClause>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cd ~/Projects/TronDB && cargo check -p trondb-tql`
Expected: Compiles clean.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat: TQL AST types — Statement, FetchStmt, SearchStmt, InsertStmt"
```

---

### Task 5: TQL parser — CREATE COLLECTION + INSERT + FETCH

**Files:**
- Create: `crates/trondb-tql/src/parser.rs`
- Modify: `crates/trondb-tql/src/lib.rs`

- [ ] **Step 1: Write parser tests**

Add to bottom of `crates/trondb-tql/src/parser.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::*;

    #[test]
    fn parse_create_collection() {
        let stmt = parse("CREATE COLLECTION venues WITH DIMENSIONS 384;").unwrap();
        assert_eq!(stmt, Statement::CreateCollection(CreateCollectionStmt {
            name: "venues".into(),
            dimensions: 384,
        }));
    }

    #[test]
    fn parse_insert_with_vector() {
        let stmt = parse("INSERT INTO venues (name, city) VALUES ('Jazz Cafe', 'London') VECTOR [0.1, 0.2, 0.3];").unwrap();
        match stmt {
            Statement::Insert(ins) => {
                assert_eq!(ins.collection, "venues");
                assert_eq!(ins.fields, vec!["name", "city"]);
                assert_eq!(ins.values[0], Literal::String("Jazz Cafe".into()));
                assert_eq!(ins.vector.as_ref().unwrap().len(), 3);
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_without_vector() {
        let stmt = parse("INSERT INTO venues (name) VALUES ('Test');").unwrap();
        match stmt {
            Statement::Insert(ins) => {
                assert!(ins.vector.is_none());
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_fetch_all() {
        let stmt = parse("FETCH * FROM venues;").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                assert_eq!(f.collection, "venues");
                assert_eq!(f.fields, FieldList::All);
                assert!(f.filter.is_none());
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_fetch_with_where() {
        let stmt = parse("FETCH name, city FROM venues WHERE city = 'London';").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                assert_eq!(f.fields, FieldList::Named(vec!["name".into(), "city".into()]));
                assert_eq!(f.filter, Some(WhereClause::Eq("city".into(), Literal::String("London".into()))));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_fetch_with_limit() {
        let stmt = parse("FETCH * FROM venues LIMIT 10;").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                assert_eq!(f.limit, Some(10));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_fetch_where_and() {
        let stmt = parse("FETCH * FROM venues WHERE city = 'London' AND name = 'Jazz';").unwrap();
        match stmt {
            Statement::Fetch(f) => {
                match f.filter.unwrap() {
                    WhereClause::And(_, _) => {} // correct
                    other => panic!("expected And, got {:?}", other),
                }
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_error_missing_semicolon() {
        let result = parse("FETCH * FROM venues");
        assert!(result.is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-tql`
Expected: FAIL — parse function doesn't exist.

- [ ] **Step 3: Implement the parser**

`crates/trondb-tql/src/parser.rs`:

```rust
use logos::Logos;

use crate::ast::*;
use crate::error::ParseError;
use crate::token::Token;

/// Parse a TQL string into a Statement.
pub fn parse(input: &str) -> Result<Statement, ParseError> {
    let mut parser = Parser::new(input)?;
    let stmt = parser.parse_statement()?;
    Ok(stmt)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(input: &str) -> Result<Self, ParseError> {
        let mut tokens = Vec::new();
        let mut lexer = Token::lexer(input);
        while let Some(tok) = lexer.next() {
            match tok {
                Ok(t) => tokens.push(t),
                Err(_) => return Err(ParseError::LexerError(lexer.span().start)),
            }
        }
        Ok(Self { tokens, pos: 0 })
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Result<&Token, ParseError> {
        let tok = self.tokens.get(self.pos)
            .ok_or_else(|| ParseError::UnexpectedEof("token".into()))?;
        self.pos += 1;
        Ok(tok)
    }

    fn expect(&mut self, expected: &Token) -> Result<(), ParseError> {
        let tok = self.advance()?.clone();
        if &tok != expected {
            return Err(ParseError::UnexpectedToken {
                pos: self.pos - 1,
                expected: format!("{expected:?}"),
                got: format!("{tok:?}"),
            });
        }
        Ok(())
    }

    fn expect_ident(&mut self) -> Result<String, ParseError> {
        let tok = self.advance()?.clone();
        match tok {
            Token::Ident(s) => Ok(s),
            other => Err(ParseError::UnexpectedToken {
                pos: self.pos - 1,
                expected: "identifier".into(),
                got: format!("{other:?}"),
            }),
        }
    }

    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        let tok = self.peek()
            .ok_or_else(|| ParseError::UnexpectedEof("statement".into()))?
            .clone();

        let stmt = match tok {
            Token::Create => self.parse_create()?,
            Token::Insert => self.parse_insert()?,
            Token::Fetch => self.parse_fetch()?,
            Token::Search => self.parse_search()?,
            Token::Explain => {
                self.advance()?;
                let inner = self.parse_statement()?;
                // The inner statement already consumed the semicolon
                return Ok(Statement::Explain(Box::new(inner)));
            }
            other => return Err(ParseError::UnexpectedToken {
                pos: self.pos,
                expected: "FETCH, SEARCH, CREATE, INSERT, or EXPLAIN".into(),
                got: format!("{other:?}"),
            }),
        };

        Ok(stmt)
    }

    fn parse_create(&mut self) -> Result<Statement, ParseError> {
        self.expect(&Token::Create)?;
        self.expect(&Token::Collection)?;
        let name = self.expect_ident()?;
        self.expect(&Token::With)?;
        self.expect(&Token::Dimensions)?;
        let dims = match self.advance()?.clone() {
            Token::IntLit(n) => n as usize,
            other => return Err(ParseError::UnexpectedToken {
                pos: self.pos - 1,
                expected: "integer".into(),
                got: format!("{other:?}"),
            }),
        };
        self.expect(&Token::Semicolon)?;
        Ok(Statement::CreateCollection(CreateCollectionStmt {
            name,
            dimensions: dims,
        }))
    }

    fn parse_insert(&mut self) -> Result<Statement, ParseError> {
        self.expect(&Token::Insert)?;
        self.expect(&Token::Into)?;
        let collection = self.expect_ident()?;

        // Parse field list: (field1, field2, ...)
        self.expect(&Token::LParen)?;
        let fields = self.parse_ident_list()?;
        self.expect(&Token::RParen)?;

        // VALUES (val1, val2, ...)
        self.expect(&Token::Values)?;
        self.expect(&Token::LParen)?;
        let values = self.parse_literal_list()?;
        self.expect(&Token::RParen)?;

        // Optional VECTOR [...]
        let vector = if self.peek() == Some(&Token::Vector) {
            self.advance()?;
            Some(self.parse_float_array()?)
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
        self.expect(&Token::Fetch)?;

        let fields = self.parse_field_list()?;

        self.expect(&Token::From)?;
        let collection = self.expect_ident()?;

        let filter = if self.peek() == Some(&Token::Where) {
            self.advance()?;
            Some(self.parse_where()?)
        } else {
            None
        };

        let limit = if self.peek() == Some(&Token::Limit) {
            self.advance()?;
            match self.advance()?.clone() {
                Token::IntLit(n) => Some(n as usize),
                other => return Err(ParseError::UnexpectedToken {
                    pos: self.pos - 1,
                    expected: "integer".into(),
                    got: format!("{other:?}"),
                }),
            }
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
        self.expect(&Token::Search)?;
        let collection = self.expect_ident()?;

        self.expect(&Token::Near)?;
        self.expect(&Token::Vector)?;
        let near = self.parse_float_array()?;

        let confidence = if self.peek() == Some(&Token::Confidence) {
            self.advance()?;
            self.expect(&Token::Gt)?;
            match self.advance()?.clone() {
                Token::FloatLit(f) => Some(f),
                Token::IntLit(n) => Some(n as f64),
                other => return Err(ParseError::UnexpectedToken {
                    pos: self.pos - 1,
                    expected: "number".into(),
                    got: format!("{other:?}"),
                }),
            }
        } else {
            None
        };

        let limit = if self.peek() == Some(&Token::Limit) {
            self.advance()?;
            match self.advance()?.clone() {
                Token::IntLit(n) => Some(n as usize),
                other => return Err(ParseError::UnexpectedToken {
                    pos: self.pos - 1,
                    expected: "integer".into(),
                    got: format!("{other:?}"),
                }),
            }
        } else {
            None
        };

        self.expect(&Token::Semicolon)?;

        Ok(Statement::Search(SearchStmt {
            collection,
            fields: FieldList::All,
            near,
            confidence,
            limit,
        }))
    }

    fn parse_field_list(&mut self) -> Result<FieldList, ParseError> {
        if self.peek() == Some(&Token::Star) {
            self.advance()?;
            return Ok(FieldList::All);
        }
        let mut fields = vec![self.expect_ident()?];
        while self.peek() == Some(&Token::Comma) {
            self.advance()?;
            fields.push(self.expect_ident()?);
        }
        Ok(FieldList::Named(fields))
    }

    fn parse_ident_list(&mut self) -> Result<Vec<String>, ParseError> {
        let mut idents = vec![self.expect_ident()?];
        while self.peek() == Some(&Token::Comma) {
            self.advance()?;
            idents.push(self.expect_ident()?);
        }
        Ok(idents)
    }

    fn parse_literal_list(&mut self) -> Result<Vec<Literal>, ParseError> {
        let mut lits = vec![self.parse_literal()?];
        while self.peek() == Some(&Token::Comma) {
            self.advance()?;
            lits.push(self.parse_literal()?);
        }
        Ok(lits)
    }

    fn parse_literal(&mut self) -> Result<Literal, ParseError> {
        let tok = self.advance()?.clone();
        match tok {
            Token::StringLit(s) => Ok(Literal::String(s)),
            Token::IntLit(n) => Ok(Literal::Int(n)),
            Token::FloatLit(f) => Ok(Literal::Float(f)),
            Token::True => Ok(Literal::Bool(true)),
            Token::False => Ok(Literal::Bool(false)),
            Token::Null => Ok(Literal::Null),
            other => Err(ParseError::UnexpectedToken {
                pos: self.pos - 1,
                expected: "literal value".into(),
                got: format!("{other:?}"),
            }),
        }
    }

    fn parse_float_array(&mut self) -> Result<Vec<f64>, ParseError> {
        self.expect(&Token::LBracket)?;
        let mut values = Vec::new();
        loop {
            let tok = self.advance()?.clone();
            match tok {
                Token::FloatLit(f) => values.push(f),
                Token::IntLit(n) => values.push(n as f64),
                Token::RBracket => break,
                other => return Err(ParseError::UnexpectedToken {
                    pos: self.pos - 1,
                    expected: "number or ]".into(),
                    got: format!("{other:?}"),
                }),
            }
            match self.peek() {
                Some(Token::Comma) => { self.advance()?; }
                Some(Token::RBracket) => {}
                _ => return Err(ParseError::UnexpectedToken {
                    pos: self.pos,
                    expected: ", or ]".into(),
                    got: format!("{:?}", self.peek()),
                }),
            }
        }
        Ok(values)
    }

    fn parse_where(&mut self) -> Result<WhereClause, ParseError> {
        let left = self.parse_where_atom()?;

        match self.peek() {
            Some(Token::And) => {
                self.advance()?;
                let right = self.parse_where()?;
                Ok(WhereClause::And(Box::new(left), Box::new(right)))
            }
            Some(Token::Or) => {
                self.advance()?;
                let right = self.parse_where()?;
                Ok(WhereClause::Or(Box::new(left), Box::new(right)))
            }
            _ => Ok(left),
        }
    }

    fn parse_where_atom(&mut self) -> Result<WhereClause, ParseError> {
        let field = self.expect_ident()?;
        let op = self.advance()?.clone();
        let value = self.parse_literal()?;

        match op {
            Token::Eq => Ok(WhereClause::Eq(field, value)),
            Token::Gt => Ok(WhereClause::Gt(field, value)),
            Token::Lt => Ok(WhereClause::Lt(field, value)),
            other => Err(ParseError::UnexpectedToken {
                pos: self.pos - 2,
                expected: "comparison operator (=, >, <)".into(),
                got: format!("{other:?}"),
            }),
        }
    }
}
```

Update `crates/trondb-tql/src/lib.rs`:

```rust
pub mod ast;
pub mod error;
pub mod parser;
pub mod token;

pub use ast::*;
pub use error::ParseError;
pub use parser::parse;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-tql`
Expected: All lexer + parser tests PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: TQL parser — CREATE COLLECTION, INSERT, FETCH, SEARCH, EXPLAIN"
```

---

## Chunk 3: In-Memory Store + HNSW Index

### Task 6: In-memory entity store

**Files:**
- Create: `crates/trondb-core/src/store.rs`
- Test: unit tests in `store.rs`

- [ ] **Step 1: Write store tests**

Add to bottom of `crates/trondb-core/src/store.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    #[test]
    fn create_collection() {
        let store = Store::new();
        store.create_collection("venues", 384).unwrap();
        assert!(store.has_collection("venues"));
    }

    #[test]
    fn create_duplicate_collection_fails() {
        let store = Store::new();
        store.create_collection("venues", 384).unwrap();
        assert!(store.create_collection("venues", 384).is_err());
    }

    #[test]
    fn insert_and_get_entity() {
        let store = Store::new();
        store.create_collection("venues", 384).unwrap();
        let id = LogicalId::from_str("v1");
        let entity = Entity::new(id.clone())
            .with_metadata("name", Value::String("Jazz Cafe".into()));
        store.insert("venues", entity).unwrap();

        let retrieved = store.get("venues", &id).unwrap();
        assert_eq!(retrieved.metadata.get("name"), Some(&Value::String("Jazz Cafe".into())));
    }

    #[test]
    fn insert_into_nonexistent_collection_fails() {
        let store = Store::new();
        let entity = Entity::new(LogicalId::from_str("v1"));
        assert!(store.insert("nope", entity).is_err());
    }

    #[test]
    fn scan_all_entities() {
        let store = Store::new();
        store.create_collection("venues", 384).unwrap();
        for i in 0..5 {
            let entity = Entity::new(LogicalId::from_str(&format!("v{i}")))
                .with_metadata("idx", Value::Int(i));
            store.insert("venues", entity).unwrap();
        }
        let all = store.scan("venues").unwrap();
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn get_collection_dimensions() {
        let store = Store::new();
        store.create_collection("events", 1408).unwrap();
        assert_eq!(store.get_dimensions("events").unwrap(), 1408);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-core -- store`
Expected: FAIL — Store doesn't exist.

- [ ] **Step 3: Implement the store**

`crates/trondb-core/src/store.rs`:

```rust
use std::sync::Arc;

use dashmap::DashMap;

use crate::error::EngineError;
use crate::types::{Collection, Entity, LogicalId};

/// In-memory entity store. One DashMap per collection.
pub struct Store {
    collections: DashMap<String, CollectionStore>,
}

struct CollectionStore {
    collection: Collection,
    entities: DashMap<LogicalId, Entity>,
}

impl Store {
    pub fn new() -> Self {
        Self {
            collections: DashMap::new(),
        }
    }

    pub fn create_collection(&self, name: &str, dimensions: usize) -> Result<(), EngineError> {
        if self.collections.contains_key(name) {
            return Err(EngineError::CollectionAlreadyExists(name.into()));
        }
        let collection = Collection::new(name, dimensions)
            .map_err(|e| EngineError::InvalidQuery(e.to_string()))?;
        self.collections.insert(name.to_string(), CollectionStore {
            collection,
            entities: DashMap::new(),
        });
        Ok(())
    }

    pub fn has_collection(&self, name: &str) -> bool {
        self.collections.contains_key(name)
    }

    pub fn get_dimensions(&self, collection: &str) -> Result<usize, EngineError> {
        self.collections.get(collection)
            .map(|cs| cs.collection.dimensions)
            .ok_or_else(|| EngineError::CollectionNotFound(collection.into()))
    }

    pub fn list_collections(&self) -> Vec<String> {
        self.collections.iter().map(|e| e.key().clone()).collect()
    }

    pub fn insert(&self, collection: &str, entity: Entity) -> Result<(), EngineError> {
        let cs = self.collections.get(collection)
            .ok_or_else(|| EngineError::CollectionNotFound(collection.into()))?;

        // Validate vector dimensions if present
        let dims = cs.collection.dimensions;
        for repr in &entity.representations {
            if repr.vector.len() != dims {
                return Err(EngineError::DimensionMismatch {
                    expected: dims,
                    got: repr.vector.len(),
                });
            }
        }

        cs.entities.insert(entity.id.clone(), entity);
        Ok(())
    }

    pub fn get(&self, collection: &str, id: &LogicalId) -> Result<Entity, EngineError> {
        let cs = self.collections.get(collection)
            .ok_or_else(|| EngineError::CollectionNotFound(collection.into()))?;
        cs.entities.get(id)
            .map(|e| e.clone())
            .ok_or_else(|| EngineError::EntityNotFound(id.to_string()))
    }

    pub fn scan(&self, collection: &str) -> Result<Vec<Entity>, EngineError> {
        let cs = self.collections.get(collection)
            .ok_or_else(|| EngineError::CollectionNotFound(collection.into()))?;
        Ok(cs.entities.iter().map(|e| e.value().clone()).collect())
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-core -- store`
Expected: All 6 store tests PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: in-memory entity store with DashMap per collection"
```

---

### Task 7: HNSW vector index

**Files:**
- Create: `crates/trondb-core/src/index.rs`
- Test: unit tests in `index.rs`

Note: The exact HNSW crate API may need adjustment based on what's available. The `hnsw_rs` crate (0.3.x) provides a pure-Rust HNSW implementation. If API differs from what's sketched here, adapt the wrapper accordingly — the public interface (`VectorIndex`) stays the same.

- [ ] **Step 1: Write index tests**

Add to bottom of `crates/trondb-core/src/index.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LogicalId;

    fn make_index() -> VectorIndex {
        VectorIndex::new(3) // 3-dimensional for testing
    }

    #[test]
    fn insert_and_search() {
        let idx = make_index();
        let id1 = LogicalId::from_str("a");
        let id2 = LogicalId::from_str("b");
        let id3 = LogicalId::from_str("c");

        idx.insert(&id1, &[1.0, 0.0, 0.0]);
        idx.insert(&id2, &[0.0, 1.0, 0.0]);
        idx.insert(&id3, &[0.9, 0.1, 0.0]); // close to id1

        let results = idx.search(&[1.0, 0.0, 0.0], 2);
        assert_eq!(results.len(), 2);
        // id1 should be closest (exact match), id3 second
        assert_eq!(results[0].0, id1);
        assert_eq!(results[1].0, id3);
    }

    #[test]
    fn search_empty_index() {
        let idx = make_index();
        let results = idx.search(&[1.0, 0.0, 0.0], 5);
        assert!(results.is_empty());
    }

    #[test]
    fn similarity_scores_are_valid() {
        let idx = make_index();
        idx.insert(&LogicalId::from_str("a"), &[1.0, 0.0, 0.0]);
        idx.insert(&LogicalId::from_str("b"), &[1.0, 0.0, 0.0]); // identical

        let results = idx.search(&[1.0, 0.0, 0.0], 2);
        // Exact match should have similarity close to 1.0
        assert!(results[0].1 > 0.99);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-core -- index`
Expected: FAIL — VectorIndex doesn't exist.

- [ ] **Step 3: Implement the HNSW index wrapper**

`crates/trondb-core/src/index.rs`:

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::types::LogicalId;

/// HNSW-backed vector index for a single collection.
/// Wraps the HNSW implementation and provides a LogicalId-based API.
///
/// Uses a simple brute-force implementation for the PoC.
/// Will be replaced with hnsw_rs or similar when we confirm the right crate.
pub struct VectorIndex {
    dimensions: usize,
    inner: Mutex<BruteForceIndex>,
}

struct BruteForceIndex {
    vectors: Vec<(LogicalId, Vec<f32>)>,
}

impl VectorIndex {
    pub fn new(dimensions: usize) -> Self {
        Self {
            dimensions,
            inner: Mutex::new(BruteForceIndex {
                vectors: Vec::new(),
            }),
        }
    }

    /// Insert a vector associated with a LogicalId.
    pub fn insert(&self, id: &LogicalId, vector: &[f32]) {
        let mut inner = self.inner.lock().unwrap();
        // Remove existing entry for this ID if present (update case)
        inner.vectors.retain(|(existing_id, _)| existing_id != id);
        inner.vectors.push((id.clone(), vector.to_vec()));
    }

    /// Search for the k nearest neighbours to the query vector.
    /// Returns (LogicalId, similarity_score) pairs sorted by descending similarity.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(LogicalId, f32)> {
        let inner = self.inner.lock().unwrap();
        if inner.vectors.is_empty() {
            return Vec::new();
        }

        let mut scored: Vec<(LogicalId, f32)> = inner.vectors.iter()
            .map(|(id, vec)| {
                let sim = cosine_similarity(query, vec);
                (id.clone(), sim)
            })
            .collect();

        // Sort by descending similarity
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        scored
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().vectors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}
```

Note: This uses brute-force cosine similarity for the PoC. Swapping in a real HNSW implementation (hnsw_rs, instant-distance, or similar) is a future task — the `VectorIndex` public API stays the same.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-core -- index`
Expected: All 3 index tests PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: brute-force vector index with cosine similarity"
```

---

## Chunk 4: Engine — Planner, Executor, and Engine API

### Task 8: Planner

**Files:**
- Create: `crates/trondb-core/src/planner.rs`
- Test: unit tests in `planner.rs`

- [ ] **Step 1: Write planner tests**

Add to bottom of `crates/trondb-core/src/planner.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use trondb_tql::ast::*;

    #[test]
    fn plan_fetch_all() {
        let stmt = Statement::Fetch(FetchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            limit: None,
        });
        let plan = plan(&stmt).unwrap();
        match plan {
            Plan::Fetch(fp) => {
                assert_eq!(fp.collection, "venues");
                assert!(fp.filter.is_none());
            }
            _ => panic!("expected FetchPlan"),
        }
    }

    #[test]
    fn plan_search() {
        let stmt = Statement::Search(SearchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            near: vec![1.0, 2.0, 3.0],
            confidence: Some(0.8),
            limit: Some(5),
        });
        let plan = plan(&stmt).unwrap();
        match plan {
            Plan::Search(sp) => {
                assert_eq!(sp.k, 5);
                assert_eq!(sp.confidence_threshold, 0.8);
            }
            _ => panic!("expected SearchPlan"),
        }
    }

    #[test]
    fn plan_explain_wraps_inner() {
        let stmt = Statement::Explain(Box::new(Statement::Fetch(FetchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            limit: None,
        })));
        let plan = plan(&stmt).unwrap();
        match plan {
            Plan::Explain(inner) => {
                assert!(matches!(*inner, Plan::Fetch(_)));
            }
            _ => panic!("expected ExplainPlan"),
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-core -- planner`
Expected: FAIL.

- [ ] **Step 3: Implement the planner**

`crates/trondb-core/src/planner.rs`:

```rust
use trondb_tql::ast::*;
use crate::error::EngineError;

/// A concrete execution plan produced by the planner.
#[derive(Debug, Clone)]
pub enum Plan {
    CreateCollection(CreateCollectionPlan),
    Insert(InsertPlan),
    Fetch(FetchPlan),
    Search(SearchPlan),
    Explain(Box<Plan>),
}

#[derive(Debug, Clone)]
pub struct CreateCollectionPlan {
    pub name: String,
    pub dimensions: usize,
}

#[derive(Debug, Clone)]
pub struct InsertPlan {
    pub collection: String,
    pub fields: Vec<String>,
    pub values: Vec<Literal>,
    pub vector: Option<Vec<f64>>,
}

#[derive(Debug, Clone)]
pub struct FetchPlan {
    pub collection: String,
    pub fields: FieldList,
    pub filter: Option<WhereClause>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct SearchPlan {
    pub collection: String,
    pub query_vector: Vec<f64>,
    pub k: usize,
    pub confidence_threshold: f64,
}

/// Convert a parsed AST statement into an execution plan.
pub fn plan(stmt: &Statement) -> Result<Plan, EngineError> {
    match stmt {
        Statement::CreateCollection(c) => Ok(Plan::CreateCollection(CreateCollectionPlan {
            name: c.name.clone(),
            dimensions: c.dimensions,
        })),
        Statement::Insert(ins) => Ok(Plan::Insert(InsertPlan {
            collection: ins.collection.clone(),
            fields: ins.fields.clone(),
            values: ins.values.clone(),
            vector: ins.vector.clone(),
        })),
        Statement::Fetch(f) => Ok(Plan::Fetch(FetchPlan {
            collection: f.collection.clone(),
            fields: f.fields.clone(),
            filter: f.filter.clone(),
            limit: f.limit,
        })),
        Statement::Search(s) => Ok(Plan::Search(SearchPlan {
            collection: s.collection.clone(),
            query_vector: s.near.clone(),
            k: s.limit.unwrap_or(10),
            confidence_threshold: s.confidence.unwrap_or(0.0),
        })),
        Statement::Explain(inner) => {
            let inner_plan = plan(inner)?;
            Ok(Plan::Explain(Box::new(inner_plan)))
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-core -- planner`
Expected: All 3 planner tests PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: query planner — AST to execution plan"
```

---

### Task 9: Executor

**Files:**
- Create: `crates/trondb-core/src/executor.rs`
- Test: unit tests in `executor.rs`

- [ ] **Step 1: Write executor tests**

Add to bottom of `crates/trondb-core/src/executor.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use crate::store::Store;
    use crate::index::VectorIndex;
    use trondb_tql::ast::*;

    fn setup() -> Executor {
        let store = Store::new();
        store.create_collection("venues", 3).unwrap();

        let entity = Entity::new(LogicalId::from_str("v1"))
            .with_metadata("name", Value::String("Jazz Cafe".into()))
            .with_metadata("city", Value::String("London".into()))
            .with_representation(Representation {
                repr_type: ReprType::Atomic,
                fields: vec!["name".into()],
                vector: vec![1.0, 0.0, 0.0],
                recipe_hash: [0u8; 32],
                state: ReprState::Clean,
            });
        store.insert("venues", entity).unwrap();

        let entity2 = Entity::new(LogicalId::from_str("v2"))
            .with_metadata("name", Value::String("Blues Bar".into()))
            .with_metadata("city", Value::String("Manchester".into()))
            .with_representation(Representation {
                repr_type: ReprType::Atomic,
                fields: vec!["name".into()],
                vector: vec![0.9, 0.1, 0.0],
                recipe_hash: [0u8; 32],
                state: ReprState::Clean,
            });
        store.insert("venues", entity2).unwrap();

        Executor::new(store)
    }

    #[test]
    fn execute_fetch_all() {
        let exec = setup();
        let plan = FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            limit: None,
        };
        let result = exec.execute(&Plan::Fetch(plan)).unwrap();
        assert_eq!(result.rows.len(), 2);
    }

    #[test]
    fn execute_fetch_with_filter() {
        let exec = setup();
        let plan = FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            limit: None,
        };
        let result = exec.execute(&Plan::Fetch(plan)).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].values.get("city"), Some(&Value::String("London".into())));
    }

    #[test]
    fn execute_search() {
        let exec = setup();
        let plan = SearchPlan {
            collection: "venues".into(),
            query_vector: vec![1.0, 0.0, 0.0],
            k: 2,
            confidence_threshold: 0.0,
        };
        let result = exec.execute(&Plan::Search(plan)).unwrap();
        assert_eq!(result.rows.len(), 2);
        // First result should be v1 (exact match)
        assert_eq!(result.rows[0].values.get("id"), Some(&Value::String("v1".into())));
        assert!(result.rows[0].score.unwrap() > 0.99);
    }

    #[test]
    fn execute_search_with_confidence_filter() {
        let exec = setup();
        let plan = SearchPlan {
            collection: "venues".into(),
            query_vector: vec![0.0, 1.0, 0.0], // orthogonal to both entities
            k: 10,
            confidence_threshold: 0.9,
        };
        let result = exec.execute(&Plan::Search(plan)).unwrap();
        // Neither entity should meet the 0.9 threshold
        assert_eq!(result.rows.len(), 0);
    }

    #[test]
    fn execute_explain() {
        let exec = setup();
        let inner = Plan::Search(SearchPlan {
            collection: "venues".into(),
            query_vector: vec![1.0, 0.0, 0.0],
            k: 5,
            confidence_threshold: 0.8,
        });
        let result = exec.execute(&Plan::Explain(Box::new(inner))).unwrap();
        // EXPLAIN returns plan description as rows
        assert!(!result.rows.is_empty());
        // Should contain mode information
        let first_row = &result.rows[0];
        assert!(first_row.values.contains_key("property"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-core -- executor`
Expected: FAIL.

- [ ] **Step 3: Implement the executor**

`crates/trondb-core/src/executor.rs`:

```rust
use std::collections::HashMap;
use std::time::Instant;

use trondb_tql::ast::{FieldList, Literal, WhereClause};

use crate::error::EngineError;
use crate::index::VectorIndex;
use crate::planner::*;
use crate::result::*;
use crate::store::Store;
use crate::types::{Entity, LogicalId, ReprState, Representation, ReprType, Value};

/// Executes plans against the store and vector index.
pub struct Executor {
    store: Store,
    /// One vector index per collection.
    indexes: dashmap::DashMap<String, VectorIndex>,
}

impl Executor {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            indexes: dashmap::DashMap::new(),
        }
    }

    pub fn execute(&self, plan: &Plan) -> Result<QueryResult, EngineError> {
        let start = Instant::now();

        match plan {
            Plan::CreateCollection(p) => self.exec_create_collection(p, start),
            Plan::Insert(p) => self.exec_insert(p, start),
            Plan::Fetch(p) => self.exec_fetch(p, start),
            Plan::Search(p) => self.exec_search(p, start),
            Plan::Explain(inner) => self.exec_explain(inner, start),
        }
    }

    fn exec_create_collection(&self, plan: &CreateCollectionPlan, start: Instant) -> Result<QueryResult, EngineError> {
        self.store.create_collection(&plan.name, plan.dimensions)?;
        self.indexes.insert(plan.name.clone(), VectorIndex::new(plan.dimensions));

        Ok(QueryResult {
            columns: vec!["result".into()],
            rows: vec![Row {
                values: HashMap::from([("result".into(), Value::String(format!("Collection '{}' created ({}d)", plan.name, plan.dimensions)))]),
                score: None,
            }],
            stats: QueryStats {
                elapsed: start.elapsed(),
                entities_scanned: 0,
                mode: QueryMode::Deterministic,
                tier: "RAM",
            },
        })
    }

    fn exec_insert(&self, plan: &InsertPlan, start: Instant) -> Result<QueryResult, EngineError> {
        let id_value = plan.fields.iter().zip(plan.values.iter())
            .find(|(f, _)| f.as_str() == "id")
            .map(|(_, v)| match v {
                Literal::String(s) => s.clone(),
                other => format!("{other:?}"),
            });

        let id = LogicalId::from_str(&id_value.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()));

        let mut entity = Entity::new(id.clone());

        // Set metadata from fields/values
        for (field, value) in plan.fields.iter().zip(plan.values.iter()) {
            if field == "id" { continue; }
            let v = literal_to_value(value);
            entity.metadata.insert(field.clone(), v);
        }

        // Add vector representation if provided
        if let Some(ref vec_data) = plan.vector {
            let dims = self.store.get_dimensions(&plan.collection)?;
            if vec_data.len() != dims {
                return Err(EngineError::DimensionMismatch {
                    expected: dims,
                    got: vec_data.len(),
                });
            }
            let vector_f32: Vec<f32> = vec_data.iter().map(|&v| v as f32).collect();

            entity.representations.push(Representation {
                repr_type: ReprType::Atomic,
                fields: plan.fields.iter().filter(|f| f.as_str() != "id").cloned().collect(),
                vector: vector_f32.clone(),
                recipe_hash: [0u8; 32],
                state: ReprState::Clean,
            });

            // Add to vector index
            if let Some(index) = self.indexes.get(&plan.collection) {
                index.insert(&id, &vector_f32);
            }
        }

        self.store.insert(&plan.collection, entity)?;

        Ok(QueryResult {
            columns: vec!["result".into()],
            rows: vec![Row {
                values: HashMap::from([("result".into(), Value::String(format!("Inserted entity '{}'", id)))]),
                score: None,
            }],
            stats: QueryStats {
                elapsed: start.elapsed(),
                entities_scanned: 0,
                mode: QueryMode::Deterministic,
                tier: "RAM",
            },
        })
    }

    fn exec_fetch(&self, plan: &FetchPlan, start: Instant) -> Result<QueryResult, EngineError> {
        let entities = self.store.scan(&plan.collection)?;
        let scanned = entities.len();

        let mut rows: Vec<Row> = entities.into_iter()
            .filter(|e| match &plan.filter {
                Some(clause) => entity_matches(e, clause),
                None => true,
            })
            .map(|e| entity_to_row(&e, &plan.fields))
            .collect();

        if let Some(limit) = plan.limit {
            rows.truncate(limit);
        }

        let columns = if rows.is_empty() {
            vec!["id".into()]
        } else {
            let mut cols: Vec<String> = rows[0].values.keys().cloned().collect();
            cols.sort();
            cols
        };

        Ok(QueryResult {
            columns,
            rows,
            stats: QueryStats {
                elapsed: start.elapsed(),
                entities_scanned: scanned,
                mode: QueryMode::Deterministic,
                tier: "RAM",
            },
        })
    }

    fn exec_search(&self, plan: &SearchPlan, start: Instant) -> Result<QueryResult, EngineError> {
        let index = self.indexes.get(&plan.collection)
            .ok_or_else(|| EngineError::CollectionNotFound(plan.collection.clone()))?;

        let query_f32: Vec<f32> = plan.query_vector.iter().map(|&v| v as f32).collect();
        let candidates = index.search(&query_f32, plan.k);

        let mut rows = Vec::new();
        let mut scanned = 0;
        for (id, score) in candidates {
            scanned += 1;
            if (score as f64) < plan.confidence_threshold {
                continue;
            }
            if let Ok(entity) = self.store.get(&plan.collection, &id) {
                let mut row = entity_to_row(&entity, &FieldList::All);
                row.score = Some(score);
                rows.push(row);
            }
        }

        let columns = if rows.is_empty() {
            vec!["id".into(), "_score".into()]
        } else {
            let mut cols: Vec<String> = rows[0].values.keys().cloned().collect();
            cols.sort();
            cols.push("_score".into());
            cols
        };

        Ok(QueryResult {
            columns,
            rows,
            stats: QueryStats {
                elapsed: start.elapsed(),
                entities_scanned: scanned,
                mode: QueryMode::Probabilistic,
                tier: "RAM",
            },
        })
    }

    fn exec_explain(&self, inner: &Plan, start: Instant) -> Result<QueryResult, EngineError> {
        let mut props: Vec<(String, String)> = Vec::new();

        match inner {
            Plan::Fetch(f) => {
                props.push(("mode".into(), "Deterministic".into()));
                props.push(("verb".into(), "FETCH".into()));
                props.push(("collection".into(), f.collection.clone()));
                props.push(("tier".into(), "RAM".into()));
                props.push(("encoding".into(), "N/A".into()));
                props.push(("strategy".into(), "full_scan".into()));
                props.push(("filter".into(), format!("{:?}", f.filter)));
                props.push(("limit".into(), format!("{:?}", f.limit)));
            }
            Plan::Search(s) => {
                props.push(("mode".into(), "Probabilistic".into()));
                props.push(("verb".into(), "SEARCH".into()));
                props.push(("collection".into(), s.collection.clone()));
                props.push(("tier".into(), "RAM".into()));
                props.push(("encoding".into(), "Float32".into()));
                props.push(("strategy".into(), "single_pass".into()));
                props.push(("k".into(), s.k.to_string()));
                props.push(("confidence_threshold".into(), format!("{}", s.confidence_threshold)));
                props.push(("vector_dimensions".into(), s.query_vector.len().to_string()));
            }
            _ => {
                props.push(("error".into(), "EXPLAIN not supported for this statement type".into()));
            }
        }

        let rows = props.into_iter().map(|(k, v)| Row {
            values: HashMap::from([
                ("property".into(), Value::String(k)),
                ("value".into(), Value::String(v)),
            ]),
            score: None,
        }).collect();

        Ok(QueryResult {
            columns: vec!["property".into(), "value".into()],
            rows,
            stats: QueryStats {
                elapsed: start.elapsed(),
                entities_scanned: 0,
                mode: QueryMode::Deterministic,
                tier: "RAM",
            },
        })
    }
}

fn literal_to_value(lit: &Literal) -> Value {
    match lit {
        Literal::String(s) => Value::String(s.clone()),
        Literal::Int(n) => Value::Int(*n),
        Literal::Float(f) => Value::Float(*f),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
    }
}

fn entity_to_row(entity: &Entity, fields: &FieldList) -> Row {
    let mut values = HashMap::new();
    values.insert("id".into(), Value::String(entity.id.to_string()));

    match fields {
        FieldList::All => {
            for (k, v) in &entity.metadata {
                values.insert(k.clone(), v.clone());
            }
        }
        FieldList::Named(names) => {
            for name in names {
                if name == "id" {
                    continue; // already added
                }
                if let Some(v) = entity.metadata.get(name) {
                    values.insert(name.clone(), v.clone());
                } else {
                    values.insert(name.clone(), Value::Null);
                }
            }
        }
    }

    Row {
        values,
        score: None,
    }
}

fn entity_matches(entity: &Entity, clause: &WhereClause) -> bool {
    match clause {
        WhereClause::Eq(field, lit) => {
            let target = literal_to_value(lit);
            if field == "id" {
                Value::String(entity.id.to_string()) == target
            } else {
                entity.metadata.get(field).map_or(false, |v| *v == target)
            }
        }
        WhereClause::Gt(field, lit) => {
            let target = literal_to_value(lit);
            entity.metadata.get(field).map_or(false, |v| value_gt(v, &target))
        }
        WhereClause::Lt(field, lit) => {
            let target = literal_to_value(lit);
            entity.metadata.get(field).map_or(false, |v| value_lt(v, &target))
        }
        WhereClause::And(a, b) => entity_matches(entity, a) && entity_matches(entity, b),
        WhereClause::Or(a, b) => entity_matches(entity, a) || entity_matches(entity, b),
    }
}

fn value_gt(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(a), Value::Int(b)) => a > b,
        (Value::Float(a), Value::Float(b)) => a > b,
        (Value::String(a), Value::String(b)) => a > b,
        _ => false,
    }
}

fn value_lt(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(a), Value::Int(b)) => a < b,
        (Value::Float(a), Value::Float(b)) => a < b,
        (Value::String(a), Value::String(b)) => a < b,
        _ => false,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-core -- executor`
Expected: All 5 executor tests PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: query executor — FETCH, SEARCH, INSERT, EXPLAIN"
```

---

### Task 10: Engine public API

**Files:**
- Modify: `crates/trondb-core/src/lib.rs`
- Test: integration test in `lib.rs`

- [ ] **Step 1: Write Engine integration test**

Add to bottom of `crates/trondb-core/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use trondb_tql::parse;

    #[test]
    fn end_to_end_create_insert_fetch() {
        let engine = Engine::new();

        engine.execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;").unwrap();
        engine.execute_tql("INSERT INTO venues (id, name, city) VALUES ('v1', 'Jazz Cafe', 'London') VECTOR [1.0, 0.0, 0.0];").unwrap();
        engine.execute_tql("INSERT INTO venues (id, name, city) VALUES ('v2', 'Blues Bar', 'Manchester') VECTOR [0.0, 1.0, 0.0];").unwrap();

        let result = engine.execute_tql("FETCH * FROM venues WHERE city = 'London';").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].values.get("name"), Some(&types::Value::String("Jazz Cafe".into())));
    }

    #[test]
    fn end_to_end_search() {
        let engine = Engine::new();

        engine.execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;").unwrap();
        engine.execute_tql("INSERT INTO venues (id, name) VALUES ('v1', 'Jazz') VECTOR [1.0, 0.0, 0.0];").unwrap();
        engine.execute_tql("INSERT INTO venues (id, name) VALUES ('v2', 'Blues') VECTOR [0.9, 0.1, 0.0];").unwrap();
        engine.execute_tql("INSERT INTO venues (id, name) VALUES ('v3', 'Rock') VECTOR [0.0, 1.0, 0.0];").unwrap();

        let result = engine.execute_tql("SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] CONFIDENCE > 0.8 LIMIT 2;").unwrap();
        assert_eq!(result.rows.len(), 2);
        // v1 should be first (exact match)
        assert_eq!(result.rows[0].values.get("id"), Some(&types::Value::String("v1".into())));
    }

    #[test]
    fn end_to_end_explain() {
        let engine = Engine::new();
        engine.execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;").unwrap();

        let result = engine.execute_tql("EXPLAIN SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] CONFIDENCE > 0.7;").unwrap();
        assert!(!result.rows.is_empty());
        // Check that mode is Probabilistic
        let mode_row = result.rows.iter().find(|r| r.values.get("property") == Some(&types::Value::String("mode".into()))).unwrap();
        assert_eq!(mode_row.values.get("value"), Some(&types::Value::String("Probabilistic".into())));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-core -- tests`
Expected: FAIL — Engine type doesn't exist.

- [ ] **Step 3: Implement Engine API**

Update `crates/trondb-core/src/lib.rs`:

```rust
pub mod error;
pub mod executor;
pub mod index;
pub mod planner;
pub mod result;
pub mod store;
pub mod types;

use error::EngineError;
use executor::Executor;
use planner::plan;
use result::QueryResult;
use store::Store;

/// The TronDB engine. Entry point for all operations.
pub struct Engine {
    executor: Executor,
}

impl Engine {
    pub fn new() -> Self {
        Self {
            executor: Executor::new(Store::new()),
        }
    }

    /// Parse and execute a TQL statement.
    pub fn execute_tql(&self, input: &str) -> Result<QueryResult, EngineError> {
        let stmt = trondb_tql::parse(input)
            .map_err(|e| EngineError::InvalidQuery(e.to_string()))?;
        let execution_plan = plan(&stmt)?;
        self.executor.execute(&execution_plan)
    }

    /// List all collection names.
    pub fn collections(&self) -> Vec<String> {
        self.executor.collections()
    }
}
```

Note: `Executor::collections()` needs to be added — a one-liner that delegates to `Store::list_collections()`.

Add to `crates/trondb-core/src/executor.rs`:

```rust
pub fn collections(&self) -> Vec<String> {
    self.store.list_collections()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd ~/Projects/TronDB && cargo test -p trondb-core`
Expected: ALL tests PASS (types + store + index + planner + executor + integration).

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: Engine public API — parse, plan, execute TQL end-to-end"
```

---

## Chunk 5: CLI REPL

### Task 11: REPL display

**Files:**
- Create: `crates/trondb-cli/src/display.rs`

- [ ] **Step 1: Implement display module**

`crates/trondb-cli/src/display.rs`:

```rust
use comfy_table::{Table, ContentArrangement};
use trondb_core::result::{QueryResult, QueryMode};
use trondb_core::types::Value;

/// Format a QueryResult as a pretty-printed table string.
pub fn format_result(result: &QueryResult) -> String {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);

    // Header row — sorted columns, with _score at the end if present
    let mut headers = result.columns.clone();
    table.set_header(&headers);

    // Data rows
    for row in &result.rows {
        let cells: Vec<String> = headers.iter().map(|col| {
            if col == "_score" {
                row.score.map(|s| format!("{:.4}", s)).unwrap_or_default()
            } else {
                row.values.get(col)
                    .map(|v| v.to_string())
                    .unwrap_or_default()
            }
        }).collect();
        table.add_row(cells);
    }

    let mut output = table.to_string();
    output.push('\n');

    // Stats line
    let mode = match result.stats.mode {
        QueryMode::Deterministic => "deterministic",
        QueryMode::Probabilistic => "probabilistic",
    };
    output.push_str(&format!(
        "{} row(s) | {} scanned | {} | {} | {:.2}ms",
        result.rows.len(),
        result.stats.entities_scanned,
        mode,
        result.stats.tier,
        result.stats.elapsed.as_secs_f64() * 1000.0,
    ));

    output
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cd ~/Projects/TronDB && cargo check -p trondb-cli`
Expected: Compiles clean.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat: CLI display — format QueryResult as table"
```

---

### Task 12: REPL main loop

**Files:**
- Modify: `crates/trondb-cli/src/main.rs`

- [ ] **Step 1: Implement the REPL**

`crates/trondb-cli/src/main.rs`:

```rust
mod display;

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use trondb_core::Engine;

fn main() {
    println!("TronDB v0.1.0 — inference-first storage engine");
    println!("Type .help for commands, or enter TQL statements ending with ;\n");

    let engine = Engine::new();
    let mut rl = DefaultEditor::new().expect("failed to create editor");
    let mut buffer = String::new();

    loop {
        let prompt = if buffer.is_empty() { "trondb> " } else { "   ...> " };

        match rl.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim();

                // Handle dot-commands (only when buffer is empty)
                if buffer.is_empty() && trimmed.starts_with('.') {
                    handle_dot_command(trimmed, &engine);
                    continue;
                }

                buffer.push_str(trimmed);
                buffer.push(' ');

                // Check if statement is complete (ends with ;)
                if !buffer.trim_end().ends_with(';') {
                    continue;
                }

                let input = buffer.trim().to_string();
                buffer.clear();

                let _ = rl.add_history_entry(&input);

                match engine.execute_tql(&input) {
                    Ok(result) => {
                        println!("{}", display::format_result(&result));
                    }
                    Err(e) => {
                        eprintln!("Error: {e}");
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                buffer.clear();
                println!("(statement cleared)");
            }
            Err(ReadlineError::Eof) => {
                println!("Goodbye.");
                break;
            }
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        }
    }
}

fn handle_dot_command(cmd: &str, engine: &Engine) {
    match cmd {
        ".help" => {
            println!("Commands:");
            println!("  .help          Show this help");
            println!("  .collections   List all collections");
            println!("  .quit          Exit TronDB");
            println!();
            println!("TQL statements must end with a semicolon (;)");
            println!("Multi-line input is supported.");
        }
        ".collections" => {
            let collections = engine.collections();
            if collections.is_empty() {
                println!("No collections.");
            } else {
                for name in collections {
                    println!("  {name}");
                }
            }
        }
        ".quit" | ".exit" => {
            println!("Goodbye.");
            std::process::exit(0);
        }
        _ => {
            eprintln!("Unknown command: {cmd}. Type .help for available commands.");
        }
    }
}
```

- [ ] **Step 2: Build and test the REPL manually**

Run: `cd ~/Projects/TronDB && cargo build -p trondb-cli`
Expected: Compiles clean. Binary at `target/debug/trondb`.

Run: `echo "CREATE COLLECTION test WITH DIMENSIONS 3;" | cargo run -p trondb-cli`
Expected: Output showing collection created.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat: interactive TQL REPL with rustyline"
```

---

### Task 13: End-to-end smoke test script

**Files:**
- Create: `tests/smoke.tql`

- [ ] **Step 1: Create a TQL smoke test script**

`tests/smoke.tql`:

```sql
-- TronDB PoC smoke test
CREATE COLLECTION venues WITH DIMENSIONS 3;

INSERT INTO venues (id, name, city) VALUES ('v1', 'The Jazz Cafe', 'London') VECTOR [1.0, 0.0, 0.0];
INSERT INTO venues (id, name, city) VALUES ('v2', 'Blues Kitchen', 'London') VECTOR [0.9, 0.1, 0.0];
INSERT INTO venues (id, name, city) VALUES ('v3', 'Rock City', 'Nottingham') VECTOR [0.0, 1.0, 0.0];
INSERT INTO venues (id, name, city) VALUES ('v4', 'The Cavern Club', 'Liverpool') VECTOR [0.1, 0.9, 0.1];

-- Deterministic query
FETCH * FROM venues WHERE city = 'London';

-- Vector similarity search
SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] CONFIDENCE > 0.5 LIMIT 3;

-- Query plan introspection
EXPLAIN SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] CONFIDENCE > 0.7;

-- Fetch with limit
FETCH name, city FROM venues LIMIT 2;
```

- [ ] **Step 2: Run the smoke test**

Run: `cd ~/Projects/TronDB && cat tests/smoke.tql | while IFS= read -r line; do [ -z "$line" ] && continue; [[ "$line" == --* ]] && continue; echo "$line"; done | cargo run -p trondb-cli`
Expected: All statements execute, results displayed.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat: add TQL smoke test script"
```

---

### Task 14: Final workspace verification

- [ ] **Step 1: Run all tests**

Run: `cd ~/Projects/TronDB && cargo test --workspace`
Expected: ALL tests pass across all three crates.

- [ ] **Step 2: Run clippy**

Run: `cd ~/Projects/TronDB && cargo clippy --workspace -- -D warnings`
Expected: No warnings.

- [ ] **Step 3: Fix any clippy warnings, commit if needed**

```bash
git add -A && git commit -m "chore: fix clippy warnings"
```
