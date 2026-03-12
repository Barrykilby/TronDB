# Phase 11: Inference Pipeline — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give TronDB the ability to propose edges from vector similarity (INFER), confirm or reject proposals (CONFIRM), run inference automatically in the background, and prune stale inferred edges.

**Architecture:** Edge struct gains a `source` field (Structural/Inferred/Confirmed). A new `inference.rs` module in trondb-core contains the inference pipeline (HNSW similarity → candidate scoring → filtering), audit ring buffer, and sweeper logic. INFER is a read-only query (returns proposals, writes nothing). CONFIRM writes edges. Background InferenceSweeper runs on a timer like TierMigrator. DecaySweeper prunes inferred edges below threshold.

**Tech Stack:** Rust 2021, async-trait, Tokio (background tasks), DashMap/DashSet (work queue), VecDeque (ring buffer)

**Spec:** `docs/superpowers/specs/2026-03-12-trondb-phase11-design.md`

---

## Dependency Graph

```
Task 1 (EdgeSource + Edge struct)
    ↓
Task 2 (InferenceConfig + EdgeType)
    ↓
Task 3 (Tokens + AST) → Task 4 (INFER parser) → Task 7 (Planner)
                       → Task 5 (CONFIRM + EXPLAIN HISTORY parser) → Task 7
                       → Task 6 (INFER AUTO parser)
                                                      Task 7 → Task 9 (INFER executor)
                                                             → Task 10 (CONFIRM executor)
                                                             → Task 11 (EXPLAIN HISTORY executor)
Task 8 (inference.rs) → Task 9
                       → Task 12 (InferenceSweeper)
                       → Task 13 (DecaySweeper)
Task 14 (WAL replay) — independent
Task 15 (Proto/gRPC) — depends on Task 7
Task 16 (Engine wiring + E2E) — depends on all
```

## File Structure

### New file:

| File | Responsibility |
|------|---------------|
| `crates/trondb-core/src/inference.rs` | Inference pipeline (HNSW search → candidate scoring → filtering), InferenceAuditBuffer (ring buffer), InferenceCandidate, InferenceTrigger, inference sweeper logic |

### Modified files:

| File | Changes |
|------|---------|
| `crates/trondb-core/src/edge.rs` | EdgeSource enum, source field on Edge + AdjEntry, InferenceConfig struct on EdgeType |
| `crates/trondb-tql/src/token.rs` | Infer, Confirm, Returning, Top, All, History, Auto tokens |
| `crates/trondb-tql/src/ast.rs` | InferStmt, ConfirmEdgeStmt, ExplainHistoryStmt, InferenceConfigDecl, Statement variants |
| `crates/trondb-tql/src/parser.rs` | INFER, CONFIRM, EXPLAIN HISTORY parsing; INFER AUTO on CREATE EDGE TYPE |
| `crates/trondb-core/src/planner.rs` | InferPlan, ConfirmEdgePlan, ExplainHistoryPlan, plan() arms |
| `crates/trondb-core/src/executor.rs` | INFER, CONFIRM, EXPLAIN HISTORY execution; edge source on INSERT EDGE |
| `crates/trondb-core/src/lib.rs` | Engine holds InferenceAuditBuffer, inference work queue; InferenceSweeper + DecaySweeper background tasks |
| `crates/trondb-proto/proto/trondb.proto` | InferPlan, ConfirmEdgePlan, ExplainHistoryPlan messages |
| `crates/trondb-proto/src/convert_plan.rs` | Bidirectional conversion for new plan types |

---

## Chunk 1: Type System + TQL Foundation

### Task 1: EdgeSource Enum + Edge/AdjEntry Struct Changes

**Files:**
- Modify: `crates/trondb-core/src/edge.rs`

**Context:** The Edge struct (line 15) currently has no `source` field. All edges are implicitly structural. We add an `EdgeSource` enum with `#[serde(default)]` for backward compatibility with existing Fjall-persisted edges.

- [ ] **Step 1: Write test for EdgeSource default deserialization**

In `crates/trondb-core/src/edge.rs`, add at the bottom of the file (there's a `#[cfg(test)]` module — if none exists, create one):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_source_defaults_to_structural() {
        let source = EdgeSource::default();
        assert_eq!(source, EdgeSource::Structural);
    }

    #[test]
    fn edge_deserializes_without_source_field() {
        // Simulate legacy edge data (no source field)
        let json = r#"{"from_id":"a","to_id":"b","edge_type":"test","confidence":1.0,"metadata":{},"created_at":0}"#;
        let edge: Edge = serde_json::from_str(json).unwrap();
        assert_eq!(edge.source, EdgeSource::Structural);
    }

    #[test]
    fn edge_serializes_with_source_field() {
        let edge = Edge {
            from_id: LogicalId::new("a"),
            to_id: LogicalId::new("b"),
            edge_type: "test".into(),
            confidence: 0.8,
            metadata: HashMap::new(),
            created_at: 0,
            source: EdgeSource::Inferred,
        };
        let json = serde_json::to_string(&edge).unwrap();
        assert!(json.contains("Inferred"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core --lib edge::tests -- --nocapture`
Expected: FAIL — `EdgeSource` doesn't exist yet.

- [ ] **Step 3: Add EdgeSource enum and update Edge struct**

Before the Edge struct definition (around line 14), add:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeSource {
    Structural,
    Inferred,
    Confirmed,
}

impl Default for EdgeSource {
    fn default() -> Self {
        EdgeSource::Structural
    }
}
```

Add `source` field to the Edge struct:

```rust
pub struct Edge {
    pub from_id: LogicalId,
    pub to_id: LogicalId,
    pub edge_type: String,
    pub confidence: f32,
    pub metadata: HashMap<String, Value>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub source: EdgeSource,
}
```

Add `source` to AdjEntry (around line 90):

```rust
#[derive(Debug, Clone)]
pub struct AdjEntry {
    pub to_id: LogicalId,
    pub confidence: f32,
    pub created_at: u64,
    pub source: EdgeSource,
}
```

- [ ] **Step 4: Fix all compilation errors**

The `AdjacencyIndex::insert` method (line 110) creates `AdjEntry` — add `source: EdgeSource` parameter. Update the method signature:

```rust
pub fn insert(&self, from_id: &LogicalId, edge_type: &str, to_id: &LogicalId, confidence: f32, created_at: u64, source: EdgeSource)
```

And the AdjEntry construction inside it:

```rust
AdjEntry {
    to_id: to_id.clone(),
    confidence,
    created_at,
    source,
}
```

Update all call sites of `adjacency.insert()`:
- `executor.rs` INSERT EDGE arm (~line 1060): add `EdgeSource::Structural`
- `executor.rs` EdgeWrite WAL replay (~line 140): add `EdgeSource::Structural` (or deserialize from Edge if available)
- `lib.rs` startup rebuild (~line 284): add `edge.source.clone()` (will default to Structural for legacy data)

Also add `serde_json` to dev-dependencies in `crates/trondb-core/Cargo.toml` if not already present (needed for the deserialize test).

- [ ] **Step 5: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core --lib -- --nocapture`
Expected: All tests pass including new edge source tests.

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/
git commit -m "feat(edge): add EdgeSource enum (Structural/Inferred/Confirmed) with backward compat"
```

---

### Task 2: InferenceConfig on EdgeType

**Files:**
- Modify: `crates/trondb-core/src/edge.rs`

**Context:** EdgeType (line 30) needs an `InferenceConfig` to control background inference. Added with `#[serde(default)]` for backward compatibility.

- [ ] **Step 1: Write test**

```rust
#[test]
fn edge_type_deserializes_without_inference_config() {
    let json = r#"{"name":"test","from_collection":"a","to_collection":"b","decay_config":{}}"#;
    let et: EdgeType = serde_json::from_str(json).unwrap();
    assert!(!et.inference_config.auto);
}

#[test]
fn inference_config_defaults_sensible() {
    let config = InferenceConfig::default();
    assert!(!config.auto);
    assert_eq!(config.confidence_floor, 0.5);
    assert_eq!(config.limit, 10);
}
```

- [ ] **Step 2: Add InferenceConfig struct and update EdgeType**

After DecayConfig (around line 45), add:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceConfig {
    pub auto: bool,
    pub confidence_floor: f32,
    pub limit: usize,
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            auto: false,
            confidence_floor: 0.5,
            limit: 10,
        }
    }
}
```

Add to EdgeType:

```rust
pub struct EdgeType {
    pub name: String,
    pub from_collection: String,
    pub to_collection: String,
    pub decay_config: DecayConfig,
    #[serde(default)]
    pub inference_config: InferenceConfig,
}
```

- [ ] **Step 3: Run tests + commit**

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core --lib edge::tests -- --nocapture
git add crates/trondb-core/src/edge.rs
git commit -m "feat(edge): add InferenceConfig on EdgeType for background inference control"
```

---

### Task 3: New TQL Tokens + AST Types

**Files:**
- Modify: `crates/trondb-tql/src/token.rs`
- Modify: `crates/trondb-tql/src/ast.rs`

**Context:** Need new tokens for INFER, CONFIRM, RETURNING, TOP, ALL, HISTORY, AUTO keywords, and new AST structs for the three new statement types.

- [ ] **Step 1: Add tokens**

In `token.rs`, add these variants to the Token enum (in the keyword section, before `Ident`):

```rust
#[token("INFER", ignore(ascii_case))]
Infer,

#[token("CONFIRM", ignore(ascii_case))]
Confirm,

#[token("RETURNING", ignore(ascii_case))]
Returning,

#[token("TOP", ignore(ascii_case))]
Top,

// Note: ALL might conflict if it already exists — check first
#[token("ALL", ignore(ascii_case))]
All,

#[token("HISTORY", ignore(ascii_case))]
History,

#[token("AUTO", ignore(ascii_case))]
Auto,

#[token("TYPE", ignore(ascii_case))]
Type,

#[token("VIA", ignore(ascii_case))]
Via,

#[token("EDGES", ignore(ascii_case))]
Edges,
```

Check if `Via`, `Type`, `All`, `Edges` already exist as tokens. If so, skip adding them. The `Via` keyword is used in TRAVERSE (`TRAVERSE ... VIA ...`) — check if it's already tokenized or if TRAVERSE uses a different mechanism.

- [ ] **Step 2: Write lexer tests**

```rust
#[test]
fn lex_infer() {
    let tokens = tokenize("INFER EDGES FROM 'x' RETURNING TOP 5 CONFIDENCE > 0.7");
    assert!(tokens.contains(&Token::Infer));
    assert!(tokens.contains(&Token::Edges));
    assert!(tokens.contains(&Token::Returning));
    assert!(tokens.contains(&Token::Top));
}

#[test]
fn lex_confirm() {
    let tokens = tokenize("CONFIRM EDGE FROM 'x' TO 'y' TYPE test CONFIDENCE 0.9");
    assert!(tokens.contains(&Token::Confirm));
    assert!(tokens.contains(&Token::Type));
}
```

- [ ] **Step 3: Add AST types**

In `ast.rs`, add new structs and extend the Statement enum:

```rust
// After UpdateStmt
#[derive(Debug, Clone, PartialEq)]
pub struct InferStmt {
    pub from_id: String,
    pub edge_types: Vec<String>,       // empty = all applicable
    pub limit: Option<usize>,          // None = ALL
    pub confidence_floor: Option<f32>, // CONFIDENCE > threshold
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfirmEdgeStmt {
    pub from_id: String,
    pub to_id: String,
    pub edge_type: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainHistoryStmt {
    pub entity_id: String,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InferenceConfigDecl {
    pub auto: bool,
    pub confidence_floor: Option<f32>,
    pub limit: Option<usize>,
}
```

Extend the Statement enum:

```rust
pub enum Statement {
    // ... existing variants ...
    Infer(InferStmt),
    ConfirmEdge(ConfirmEdgeStmt),
    ExplainHistory(ExplainHistoryStmt),
}
```

Add `inference_config: Option<InferenceConfigDecl>` to `CreateEdgeTypeStmt`.

- [ ] **Step 4: Run tests + commit**

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- --nocapture
git add crates/trondb-tql/
git commit -m "feat(tql): add INFER, CONFIRM, EXPLAIN HISTORY tokens and AST types"
```

---

### Task 4: INFER Parser

**Files:**
- Modify: `crates/trondb-tql/src/parser.rs`

**Context:** Parse `INFER EDGES FROM 'id' [VIA type1, type2] RETURNING (TOP n | ALL) [CONFIDENCE > threshold];`

- [ ] **Step 1: Write parser tests**

```rust
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
            assert!(s.limit.is_none()); // ALL = no limit
            assert_eq!(s.confidence_floor, Some(0.90));
        }
        _ => panic!("expected Infer"),
    }
}
```

- [ ] **Step 2: Implement parse_infer()**

In `parser.rs`, add a `parse_infer` function and wire it into the main `parse_statement` dispatch (when the first token is `Token::Infer`):

```rust
fn parse_infer(tokens: &[Token], pos: &mut usize) -> Result<Statement, ParseError> {
    expect(tokens, pos, &Token::Infer)?;
    expect(tokens, pos, &Token::Edges)?;
    expect(tokens, pos, &Token::From)?;

    let from_id = expect_string_lit(tokens, pos)?;

    // Optional VIA clause
    let mut edge_types = Vec::new();
    if peek(tokens, *pos) == Some(&Token::Via) {
        *pos += 1;
        loop {
            let name = expect_ident(tokens, pos)?;
            edge_types.push(name);
            if peek(tokens, *pos) != Some(&Token::Comma) {
                break;
            }
            *pos += 1; // consume comma
        }
    }

    expect(tokens, pos, &Token::Returning)?;

    // TOP n or ALL
    let limit = if peek(tokens, *pos) == Some(&Token::Top) {
        *pos += 1;
        Some(expect_int_lit(tokens, pos)? as usize)
    } else if peek(tokens, *pos) == Some(&Token::All) {
        *pos += 1;
        None
    } else {
        return Err(ParseError::Expected("TOP or ALL after RETURNING".into()));
    };

    // Optional CONFIDENCE > threshold
    let confidence_floor = if peek(tokens, *pos) == Some(&Token::Confidence) {
        *pos += 1;
        expect(tokens, pos, &Token::Gt)?;
        Some(expect_float_or_int(tokens, pos)? as f32)
    } else {
        None
    };

    expect(tokens, pos, &Token::Semicolon)?;

    Ok(Statement::Infer(InferStmt {
        from_id,
        edge_types,
        limit,
        confidence_floor,
    }))
}
```

Note: `expect_float_or_int` is a helper that accepts either `FloatLit` or `IntLit` — check if it exists. If not, use the existing pattern for parsing float values (look at how CONFIDENCE is parsed in TRAVERSE/WHERE).

- [ ] **Step 3: Run tests + commit**

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- --nocapture
git add crates/trondb-tql/src/parser.rs
git commit -m "feat(parser): INFER EDGES FROM ... RETURNING TOP/ALL parsing"
```

---

### Task 5: CONFIRM + EXPLAIN HISTORY Parser

**Files:**
- Modify: `crates/trondb-tql/src/parser.rs`

**Context:** Parse `CONFIRM EDGE FROM 'id' TO 'id' TYPE name CONFIDENCE value;` and `EXPLAIN HISTORY 'id' [LIMIT n];`

- [ ] **Step 1: Write parser tests**

```rust
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
```

- [ ] **Step 2: Implement parse_confirm_edge()**

```rust
fn parse_confirm_edge(tokens: &[Token], pos: &mut usize) -> Result<Statement, ParseError> {
    expect(tokens, pos, &Token::Confirm)?;
    expect(tokens, pos, &Token::Edge)?;
    expect(tokens, pos, &Token::From)?;
    let from_id = expect_string_lit(tokens, pos)?;
    expect(tokens, pos, &Token::To)?;
    let to_id = expect_string_lit(tokens, pos)?;
    expect(tokens, pos, &Token::Type)?;
    let edge_type = expect_ident(tokens, pos)?;
    expect(tokens, pos, &Token::Confidence)?;
    let confidence = expect_float_or_int(tokens, pos)? as f32;
    expect(tokens, pos, &Token::Semicolon)?;

    Ok(Statement::ConfirmEdge(ConfirmEdgeStmt {
        from_id,
        to_id,
        edge_type,
        confidence,
    }))
}
```

- [ ] **Step 3: Wire EXPLAIN HISTORY into existing EXPLAIN dispatch**

The existing EXPLAIN parsing likely checks the next token after EXPLAIN. Add a check: if the token after EXPLAIN is `History`, parse as ExplainHistory instead of wrapping a sub-statement.

```rust
fn parse_explain_history(tokens: &[Token], pos: &mut usize) -> Result<Statement, ParseError> {
    // EXPLAIN already consumed
    expect(tokens, pos, &Token::History)?;
    let entity_id = expect_string_lit(tokens, pos)?;
    let limit = if peek(tokens, *pos) == Some(&Token::Limit) {
        *pos += 1;
        Some(expect_int_lit(tokens, pos)? as usize)
    } else {
        None
    };
    expect(tokens, pos, &Token::Semicolon)?;

    Ok(Statement::ExplainHistory(ExplainHistoryStmt {
        entity_id,
        limit,
    }))
}
```

- [ ] **Step 4: Run tests + commit**

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- --nocapture
git add crates/trondb-tql/src/parser.rs
git commit -m "feat(parser): CONFIRM EDGE and EXPLAIN HISTORY parsing"
```

---

### Task 6: INFER AUTO on CREATE EDGE TYPE Parser

**Files:**
- Modify: `crates/trondb-tql/src/parser.rs`

**Context:** Extend CREATE EDGE TYPE to support `INFER AUTO CONFIDENCE > 0.75 LIMIT 5` after the DECAY clause.

- [ ] **Step 1: Write parser test**

```rust
#[test]
fn parse_create_edge_type_with_infer_auto() {
    let stmt = parse("CREATE EDGE TYPE 'performs_at' FROM acts TO venues DECAY EXPONENTIAL RATE 0.05 FLOOR 0.1 PRUNE 0.05 INFER AUTO CONFIDENCE > 0.75 LIMIT 5;").unwrap();
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
fn parse_create_edge_type_without_infer() {
    let stmt = parse("CREATE EDGE TYPE 'likes' FROM users TO venues;").unwrap();
    match stmt {
        Statement::CreateEdgeType(s) => {
            assert!(s.inference_config.is_none());
        }
        _ => panic!("expected CreateEdgeType"),
    }
}
```

- [ ] **Step 2: Add INFER AUTO parsing to CREATE EDGE TYPE**

In the existing `parse_create_edge_type()` function, after the DECAY clause parsing, add:

```rust
// Optional INFER AUTO clause
let inference_config = if peek(tokens, *pos) == Some(&Token::Infer) {
    *pos += 1;
    expect(tokens, pos, &Token::Auto)?;
    let mut confidence_floor = None;
    let mut limit = None;
    // Parse optional CONFIDENCE > value and LIMIT n
    while peek(tokens, *pos) != Some(&Token::Semicolon) {
        match peek(tokens, *pos) {
            Some(&Token::Confidence) => {
                *pos += 1;
                expect(tokens, pos, &Token::Gt)?;
                confidence_floor = Some(expect_float_or_int(tokens, pos)? as f32);
            }
            Some(&Token::Limit) => {
                *pos += 1;
                limit = Some(expect_int_lit(tokens, pos)? as usize);
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
```

- [ ] **Step 3: Run tests + commit**

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-tql -- --nocapture
git add crates/trondb-tql/src/parser.rs
git commit -m "feat(parser): INFER AUTO clause on CREATE EDGE TYPE"
```

---

## Chunk 2: Planner + Inference Engine

### Task 7: New Plan Types + Planner

**Files:**
- Modify: `crates/trondb-core/src/planner.rs`

**Context:** Add InferPlan, ConfirmEdgePlan, ExplainHistoryPlan structs, extend the Plan enum, and add plan() arms.

- [ ] **Step 1: Add plan structs**

After the existing plan structs (around line 169):

```rust
#[derive(Debug, Clone)]
pub struct InferPlan {
    pub from_id: String,
    pub edge_types: Vec<String>,
    pub limit: Option<usize>,
    pub confidence_floor: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct ConfirmEdgePlan {
    pub from_id: String,
    pub to_id: String,
    pub edge_type: String,
    pub confidence: f32,
}

#[derive(Debug, Clone)]
pub struct ExplainHistoryPlan {
    pub entity_id: String,
    pub limit: Option<usize>,
}
```

- [ ] **Step 2: Extend Plan enum**

```rust
pub enum Plan {
    // ... existing variants ...
    Infer(InferPlan),
    ConfirmEdge(ConfirmEdgePlan),
    ExplainHistory(ExplainHistoryPlan),
}
```

- [ ] **Step 3: Add plan() function arms**

In the `plan()` function's match on `stmt`:

```rust
Statement::Infer(s) => Ok(Plan::Infer(InferPlan {
    from_id: s.from_id.clone(),
    edge_types: s.edge_types.clone(),
    limit: s.limit,
    confidence_floor: s.confidence_floor,
})),

Statement::ConfirmEdge(s) => Ok(Plan::ConfirmEdge(ConfirmEdgePlan {
    from_id: s.from_id.clone(),
    to_id: s.to_id.clone(),
    edge_type: s.edge_type.clone(),
    confidence: s.confidence,
})),

Statement::ExplainHistory(s) => Ok(Plan::ExplainHistory(ExplainHistoryPlan {
    entity_id: s.entity_id.clone(),
    limit: s.limit,
})),
```

- [ ] **Step 4: Run tests + commit**

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core -- --nocapture
git add crates/trondb-core/src/planner.rs
git commit -m "feat(planner): InferPlan, ConfirmEdgePlan, ExplainHistoryPlan"
```

---

### Task 8: Inference Module (inference.rs)

**Files:**
- Create: `crates/trondb-core/src/inference.rs`
- Modify: `crates/trondb-core/src/lib.rs` (add `pub mod inference;`)

**Context:** New module for the inference pipeline algorithm, audit ring buffer, and candidate types. This is extracted from the executor to keep file sizes manageable.

- [ ] **Step 1: Create inference.rs with types and audit buffer**

```rust
use crate::edge::{AdjacencyIndex, EdgeSource, EdgeType};
use crate::types::LogicalId;
use std::collections::VecDeque;
use std::sync::Mutex;

/// A candidate edge proposed by the inference pipeline.
#[derive(Debug, Clone)]
pub struct InferenceCandidate {
    pub entity_id: LogicalId,
    pub similarity_score: f32,
    pub accepted: bool,
}

/// What triggered the inference.
#[derive(Debug, Clone, PartialEq)]
pub enum InferenceTrigger {
    Explicit,   // INFER EDGES query
    Background, // InferenceSweeper
}

/// One entry in the inference audit ring buffer.
#[derive(Debug, Clone)]
pub struct InferenceAuditEntry {
    pub timestamp: u64,
    pub source_entity: LogicalId,
    pub edge_type: String,
    pub candidates_evaluated: usize,
    pub candidates_above_threshold: usize,
    pub top_candidates: Vec<InferenceCandidate>,
    pub trigger: InferenceTrigger,
}

/// RAM-resident ring buffer of inference audit entries.
pub struct InferenceAuditBuffer {
    entries: Mutex<VecDeque<InferenceAuditEntry>>,
    capacity: usize,
}

impl InferenceAuditBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    pub fn record(&self, entry: InferenceAuditEntry) {
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= self.capacity {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    pub fn query(&self, entity_id: &LogicalId, limit: Option<usize>) -> Vec<InferenceAuditEntry> {
        let entries = self.entries.lock().unwrap();
        let limit = limit.unwrap_or(usize::MAX);
        entries
            .iter()
            .rev()
            .filter(|e| e.source_entity == *entity_id)
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}
```

- [ ] **Step 2: Write tests for audit buffer**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(entity: &str, edge_type: &str) -> InferenceAuditEntry {
        InferenceAuditEntry {
            timestamp: 0,
            source_entity: LogicalId::new(entity),
            edge_type: edge_type.into(),
            candidates_evaluated: 5,
            candidates_above_threshold: 2,
            top_candidates: vec![],
            trigger: InferenceTrigger::Explicit,
        }
    }

    #[test]
    fn audit_buffer_records_and_queries() {
        let buf = InferenceAuditBuffer::new(100);
        buf.record(make_entry("e1", "likes"));
        buf.record(make_entry("e2", "likes"));
        buf.record(make_entry("e1", "visits"));

        let results = buf.query(&LogicalId::new("e1"), None);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn audit_buffer_evicts_oldest() {
        let buf = InferenceAuditBuffer::new(2);
        buf.record(make_entry("e1", "a"));
        buf.record(make_entry("e2", "b"));
        buf.record(make_entry("e3", "c"));

        assert_eq!(buf.len(), 2);
        let all = buf.query(&LogicalId::new("e1"), None);
        assert!(all.is_empty()); // e1 was evicted
    }

    #[test]
    fn audit_buffer_respects_limit() {
        let buf = InferenceAuditBuffer::new(100);
        for i in 0..20 {
            buf.record(make_entry("e1", &format!("type{i}")));
        }
        let results = buf.query(&LogicalId::new("e1"), Some(5));
        assert_eq!(results.len(), 5);
    }
}
```

- [ ] **Step 3: Add `pub mod inference;` to lib.rs**

- [ ] **Step 4: Run tests + commit**

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core --lib inference -- --nocapture
git add crates/trondb-core/src/inference.rs crates/trondb-core/src/lib.rs
git commit -m "feat(core): inference module with audit ring buffer"
```

---

### Task 9: INFER Executor

**Files:**
- Modify: `crates/trondb-core/src/executor.rs`

**Context:** The INFER executor implements the inference pipeline: fetch source entity → resolve target collections → HNSW search → score → filter existing edges → merge + rank → record audit → return results.

- [ ] **Step 1: Write integration test in lib.rs**

```rust
#[tokio::test]
async fn infer_proposes_edges_from_similarity() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _) = Engine::open(test_config(dir.path())).await.unwrap();

    create_simple_collection(&engine, "acts", 3).await;
    create_simple_collection(&engine, "venues", 3).await;

    // Create edge type
    engine.execute_tql("CREATE EDGE TYPE 'performs_at' FROM acts TO venues;").await.unwrap();

    // Insert entities with similar vectors
    engine.execute_tql("INSERT INTO acts (id, name) VALUES ('act1', 'Jazz Band') REPRESENTATION default VECTOR [0.9, 0.1, 0.0];").await.unwrap();
    engine.execute_tql("INSERT INTO venues (id, name) VALUES ('v1', 'Jazz Club') REPRESENTATION default VECTOR [0.85, 0.15, 0.0];").await.unwrap();
    engine.execute_tql("INSERT INTO venues (id, name) VALUES ('v2', 'Rock Arena') REPRESENTATION default VECTOR [0.0, 0.1, 0.9];").await.unwrap();

    // INFER should propose edges
    let result = engine.execute_tql("INFER EDGES FROM 'act1' VIA performs_at RETURNING TOP 5;").await.unwrap();
    assert!(!result.rows.is_empty());
    // First result should be v1 (more similar)
    assert_eq!(result.rows[0].values.get("to_id"), Some(&Value::String("v1".into())));
}

#[tokio::test]
async fn infer_excludes_existing_edges() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _) = Engine::open(test_config(dir.path())).await.unwrap();

    create_simple_collection(&engine, "acts", 3).await;
    create_simple_collection(&engine, "venues", 3).await;
    engine.execute_tql("CREATE EDGE TYPE 'performs_at' FROM acts TO venues;").await.unwrap();

    engine.execute_tql("INSERT INTO acts (id, name) VALUES ('act1', 'Band') REPRESENTATION default VECTOR [0.9, 0.1, 0.0];").await.unwrap();
    engine.execute_tql("INSERT INTO venues (id, name) VALUES ('v1', 'Club') REPRESENTATION default VECTOR [0.85, 0.15, 0.0];").await.unwrap();

    // Create existing edge
    engine.execute_tql("INSERT EDGE 'performs_at' FROM 'act1' TO 'v1';").await.unwrap();

    // INFER should NOT propose act1→v1 (already exists)
    let result = engine.execute_tql("INFER EDGES FROM 'act1' VIA performs_at RETURNING TOP 5;").await.unwrap();
    let proposed_ids: Vec<_> = result.rows.iter()
        .filter_map(|r| r.values.get("to_id"))
        .collect();
    assert!(!proposed_ids.iter().any(|v| v == &&Value::String("v1".into())));
}
```

- [ ] **Step 2: Implement INFER execution in executor.rs**

Add a new arm in the `execute()` match:

```rust
Plan::Infer(p) => {
    // 1. Find source entity's collection
    let source_collection = self.entity_collections.get(&LogicalId::new(&p.from_id))
        .map(|r| r.value().clone())
        .ok_or_else(|| EngineError::NotFound(format!("entity '{}' not found", p.from_id)))?;

    // 2. Resolve applicable edge types
    let applicable_types: Vec<EdgeType> = if p.edge_types.is_empty() {
        self.edge_types.iter()
            .filter(|et| et.from_collection == source_collection)
            .map(|et| et.value().clone())
            .collect()
    } else {
        p.edge_types.iter()
            .filter_map(|name| self.edge_types.get(name).map(|et| et.value().clone()))
            .collect()
    };

    // 3. Get source entity's vector
    let source_entity = self.store.get(&source_collection, &LogicalId::new(&p.from_id))?
        .ok_or_else(|| EngineError::NotFound(format!("entity '{}' not found in store", p.from_id)))?;
    let source_vector = source_entity.representations.first()
        .and_then(|r| r.dense_vector())
        .ok_or_else(|| EngineError::NotFound("source entity has no vector representation".into()))?;

    let mut all_candidates = Vec::new();

    // 4. For each edge type, HNSW search in target collection
    for et in &applicable_types {
        let target_collection = &et.to_collection;
        if let Some(hnsw) = self.indexes.get(target_collection) {
            let search_results = hnsw.search(&source_vector, p.limit.unwrap_or(100));

            for (entity_id, score) in search_results {
                // Apply confidence floor
                if let Some(floor) = p.confidence_floor {
                    if score < floor { continue; }
                }

                // Exclude existing edges of this type
                let existing = self.adjacency.get(&LogicalId::new(&p.from_id), &et.name);
                if existing.iter().any(|e| e.to_id == entity_id) {
                    continue;
                }

                all_candidates.push((entity_id, et.name.clone(), score));
            }
        }
    }

    // 5. Sort by score descending and apply limit
    all_candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    if let Some(limit) = p.limit {
        all_candidates.truncate(limit);
    }

    // 6. Record audit
    // (audit recording via engine's InferenceAuditBuffer — passed through or accessible)

    // 7. Build result rows
    let rows: Vec<Row> = all_candidates.iter().map(|(to_id, edge_type, score)| {
        let mut values = HashMap::new();
        values.insert("from_id".to_string(), Value::String(p.from_id.clone()));
        values.insert("to_id".to_string(), Value::String(to_id.to_string()));
        values.insert("edge_type".to_string(), Value::String(edge_type.clone()));
        values.insert("confidence".to_string(), Value::Float(*score as f64));
        Row { values, score: Some(*score) }
    }).collect();

    Ok(QueryResult {
        rows,
        execution_time_us: 0,
        strategy: Some("Infer".to_string()),
        ..Default::default()
    })
}
```

Note: This is a sketch. Adapt to match actual method signatures on `HnswIndex`, `FjallStore`, `Entity`, `Row`, `QueryResult`, etc. Read the actual types before implementing.

- [ ] **Step 3: Run tests + commit**

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core --lib infer -- --nocapture
git add crates/trondb-core/src/executor.rs crates/trondb-core/src/lib.rs
git commit -m "feat(executor): INFER EDGES execution — HNSW similarity search for edge proposals"
```

---

### Task 10: CONFIRM Executor

**Files:**
- Modify: `crates/trondb-core/src/executor.rs`

**Context:** CONFIRM creates or updates edges. Uses WAL records EdgeConfirm (0x33) and EdgeConfidenceUpdate (0x32).

- [ ] **Step 1: Write integration test**

```rust
#[tokio::test]
async fn confirm_creates_new_confirmed_edge() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _) = Engine::open(test_config(dir.path())).await.unwrap();

    create_simple_collection(&engine, "acts", 3).await;
    create_simple_collection(&engine, "venues", 3).await;
    engine.execute_tql("CREATE EDGE TYPE 'performs_at' FROM acts TO venues;").await.unwrap();

    engine.execute_tql("INSERT INTO acts (id, name) VALUES ('act1', 'Band') REPRESENTATION default VECTOR [1.0, 0.0, 0.0];").await.unwrap();
    engine.execute_tql("INSERT INTO venues (id, name) VALUES ('v1', 'Club') REPRESENTATION default VECTOR [0.0, 1.0, 0.0];").await.unwrap();

    engine.execute_tql("CONFIRM EDGE FROM 'act1' TO 'v1' TYPE performs_at CONFIDENCE 0.95;").await.unwrap();

    // Edge should exist in TRAVERSE
    let result = engine.execute_tql("TRAVERSE 'act1' VIA 'performs_at' DEPTH 1;").await.unwrap();
    assert_eq!(result.rows.len(), 1);
}

#[tokio::test]
async fn confirm_rejects_structural_edge() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _) = Engine::open(test_config(dir.path())).await.unwrap();

    create_simple_collection(&engine, "acts", 3).await;
    create_simple_collection(&engine, "venues", 3).await;
    engine.execute_tql("CREATE EDGE TYPE 'performs_at' FROM acts TO venues;").await.unwrap();

    engine.execute_tql("INSERT INTO acts (id, name) VALUES ('act1', 'Band') REPRESENTATION default VECTOR [1.0, 0.0, 0.0];").await.unwrap();
    engine.execute_tql("INSERT INTO venues (id, name) VALUES ('v1', 'Club') REPRESENTATION default VECTOR [0.0, 1.0, 0.0];").await.unwrap();

    // Create structural edge first
    engine.execute_tql("INSERT EDGE 'performs_at' FROM 'act1' TO 'v1';").await.unwrap();

    // CONFIRM should fail on structural edge
    let result = engine.execute_tql("CONFIRM EDGE FROM 'act1' TO 'v1' TYPE performs_at CONFIDENCE 0.95;").await;
    assert!(result.is_err());
}
```

- [ ] **Step 2: Implement CONFIRM execution**

Add `Plan::ConfirmEdge` arm in `execute()`. Logic:
1. Validate edge type exists
2. Check if edge already exists (Fjall lookup)
3. If structural → error
4. If inferred → update source to Confirmed, update confidence, WAL EdgeConfirm
5. If confirmed → update confidence, WAL EdgeConfidenceUpdate
6. If no edge → create new with source=Confirmed, WAL EdgeConfirm
7. Update AdjacencyIndex

- [ ] **Step 3: Run tests + commit**

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core --lib confirm -- --nocapture
git add crates/trondb-core/src/executor.rs
git commit -m "feat(executor): CONFIRM EDGE execution — promote/create confirmed edges"
```

---

### Task 11: EXPLAIN HISTORY Executor + Edge Source in INSERT EDGE

**Files:**
- Modify: `crates/trondb-core/src/executor.rs`
- Modify: `crates/trondb-core/src/lib.rs`

**Context:** Two small changes: (1) EXPLAIN HISTORY queries the audit ring buffer, (2) INSERT EDGE explicitly sets `source: Structural`.

- [ ] **Step 1: Add InferenceAuditBuffer to Engine**

In `lib.rs`, add `inference_audit: Arc<InferenceAuditBuffer>` to the Engine struct. Initialize in `Engine::open()` with capacity 1000.

Add public accessor: `pub fn inference_audit(&self) -> &InferenceAuditBuffer`

Pass it to the Executor (add field to Executor struct, or make Engine pass it when calling execute).

- [ ] **Step 2: Implement EXPLAIN HISTORY execution**

```rust
Plan::ExplainHistory(p) => {
    let entries = self.inference_audit.query(
        &LogicalId::new(&p.entity_id),
        p.limit,
    );

    let rows: Vec<Row> = entries.iter().map(|e| {
        let mut values = HashMap::new();
        values.insert("timestamp".to_string(), Value::Int(e.timestamp as i64));
        values.insert("entity_id".to_string(), Value::String(e.source_entity.to_string()));
        values.insert("edge_type".to_string(), Value::String(e.edge_type.clone()));
        values.insert("candidates_evaluated".to_string(), Value::Int(e.candidates_evaluated as i64));
        values.insert("candidates_above_threshold".to_string(), Value::Int(e.candidates_above_threshold as i64));
        values.insert("trigger".to_string(), Value::String(format!("{:?}", e.trigger)));
        Row { values, score: None }
    }).collect();

    Ok(QueryResult {
        rows,
        execution_time_us: 0,
        strategy: Some("ExplainHistory".to_string()),
        ..Default::default()
    })
}
```

- [ ] **Step 3: Run tests + commit**

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core --lib -- --nocapture
git add crates/trondb-core/
git commit -m "feat(executor): EXPLAIN HISTORY + InferenceAuditBuffer wiring"
```

---

## Chunk 3: Background Tasks + Persistence + Integration

### Task 12: InferenceSweeper Background Task

**Files:**
- Modify: `crates/trondb-core/src/inference.rs`
- Modify: `crates/trondb-core/src/lib.rs`
- Modify: `crates/trondb-core/src/executor.rs`

**Context:** Background task that drains a work queue of entities flagged by INSERT/UPDATE, and runs the INFER pipeline for edge types with `INFER AUTO` enabled.

- [ ] **Step 1: Add inference work queue to Executor**

In executor.rs, add:
```rust
use dashmap::DashSet;
// Add to Executor struct:
inference_queue: Arc<DashSet<LogicalId>>,
```

- [ ] **Step 2: Populate queue on INSERT and UPDATE**

In the INSERT executor arm (after entity write), check if any edge type with `inference_config.auto == true` has `from_collection` matching the entity's collection. If so, insert entity ID into `inference_queue`.

Same for UPDATE.

- [ ] **Step 3: Add drain_inference_queue method**

```rust
pub async fn drain_inference_queue(&self) -> Result<usize, EngineError> {
    let mut processed = 0;
    let entity_ids: Vec<LogicalId> = self.inference_queue.iter().map(|r| r.key().clone()).collect();
    for entity_id in &entity_ids {
        self.inference_queue.remove(entity_id);

        // Find entity's collection
        let collection = match self.entity_collections.get(entity_id) {
            Some(c) => c.value().clone(),
            None => continue,
        };

        // Find edge types with auto inference enabled for this collection
        let auto_types: Vec<EdgeType> = self.edge_types.iter()
            .filter(|et| et.from_collection == collection && et.inference_config.auto)
            .map(|et| et.value().clone())
            .collect();

        for et in &auto_types {
            // Run inference pipeline (similar to INFER executor, but write edges)
            // Get entity vector, HNSW search target collection, filter, write EdgeInferred
            // ... (reuse inference pipeline from Task 9, but write results)
        }
        processed += 1;
    }
    Ok(processed)
}
```

- [ ] **Step 4: Spawn background task in Engine::open()**

```rust
let inference_sweeper_handle = {
    let executor = /* clone/arc needed fields */;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            if let Err(e) = executor.drain_inference_queue().await {
                tracing::warn!("inference sweeper error: {e}");
            }
        }
    })
};
```

Store handle in Engine struct: `_inference_sweeper_handle: Option<JoinHandle<()>>`

- [ ] **Step 5: Write test + commit**

```rust
#[tokio::test]
async fn background_inference_creates_inferred_edges() {
    // Create engine, collections, edge type with INFER AUTO
    // Insert entities
    // Trigger drain_inference_queue
    // Verify inferred edges exist in TRAVERSE
}
```

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core --lib background_inference -- --nocapture
git add crates/trondb-core/
git commit -m "feat(inference): background InferenceSweeper with work queue"
```

---

### Task 13: DecaySweeper Background Task

**Files:**
- Modify: `crates/trondb-core/src/executor.rs`
- Modify: `crates/trondb-core/src/lib.rs`

**Context:** Background task that prunes inferred edges whose effective confidence has decayed below `prune_threshold`.

- [ ] **Step 1: Add sweep_decayed_edges method to Executor**

```rust
pub async fn sweep_decayed_edges(&self) -> Result<usize, EngineError> {
    let mut pruned = 0;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    for et_ref in self.edge_types.iter() {
        let et = et_ref.value();
        if et.decay_config.prune_threshold.is_none() { continue; }
        let prune_threshold = et.decay_config.prune_threshold.unwrap() as f32;

        if let Ok(edges) = self.store.scan_edges(&et.name) {
            for edge in &edges {
                // Only prune Inferred edges
                if edge.source != EdgeSource::Inferred { continue; }

                let elapsed = now_ms.saturating_sub(edge.created_at);
                let effective = effective_confidence(edge.confidence, elapsed, &et.decay_config);

                if effective < prune_threshold {
                    // WAL + Fjall delete + AdjacencyIndex remove
                    // (same pattern as DELETE EDGE)
                    self.store.delete_edge(&et.name, &edge.from_id, &edge.to_id)?;
                    self.adjacency.remove(&edge.from_id, &et.name, &edge.to_id);
                    pruned += 1;
                }
            }
        }
    }
    Ok(pruned)
}
```

- [ ] **Step 2: Spawn background task**

In `Engine::open()`, spawn a DecaySweeper on 60s interval, same pattern as InferenceSweeper.

- [ ] **Step 3: Write test + commit**

```rust
#[tokio::test]
async fn decay_sweeper_prunes_old_inferred_edges() {
    // Create engine with edge type with DECAY + PRUNE
    // Write an inferred edge with old created_at (simulating age)
    // Run sweep_decayed_edges
    // Verify edge is deleted
}
```

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core --lib decay_sweeper -- --nocapture
git add crates/trondb-core/
git commit -m "feat(inference): DecaySweeper prunes stale inferred edges"
```

---

### Task 14: WAL Replay for EdgeInferred, EdgeConfidenceUpdate, EdgeConfirm

**Files:**
- Modify: `crates/trondb-core/src/executor.rs` (replay_wal_records)

**Context:** Three WAL record types (0x31, 0x32, 0x33) need replay handlers. They follow the same pattern as EdgeWrite (0x30).

- [ ] **Step 1: Add replay handlers**

In `replay_wal_records()`, add arms:

```rust
RecordType::EdgeInferred => {
    // Same as EdgeWrite but with source=Inferred
    let edge: Edge = rmp_serde::from_slice(&record.payload)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    self.store.insert_edge(&edge.edge_type, edge.clone())?;
    self.adjacency.insert(
        &edge.from_id, &edge.edge_type, &edge.to_id,
        edge.confidence, edge.created_at, edge.source.clone(),
    );
    replayed += 1;
}

RecordType::EdgeConfirm => {
    let edge: Edge = rmp_serde::from_slice(&record.payload)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    // Delete old entry if exists, insert new
    self.store.delete_edge(&edge.edge_type, &edge.from_id, &edge.to_id).ok();
    self.adjacency.remove(&edge.from_id, &edge.edge_type, &edge.to_id);
    self.store.insert_edge(&edge.edge_type, edge.clone())?;
    self.adjacency.insert(
        &edge.from_id, &edge.edge_type, &edge.to_id,
        edge.confidence, edge.created_at, edge.source.clone(),
    );
    replayed += 1;
}

RecordType::EdgeConfidenceUpdate => {
    // Update confidence on existing edge
    let edge: Edge = rmp_serde::from_slice(&record.payload)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    self.store.delete_edge(&edge.edge_type, &edge.from_id, &edge.to_id).ok();
    self.store.insert_edge(&edge.edge_type, edge.clone())?;
    self.adjacency.remove(&edge.from_id, &edge.edge_type, &edge.to_id);
    self.adjacency.insert(
        &edge.from_id, &edge.edge_type, &edge.to_id,
        edge.confidence, edge.created_at, edge.source.clone(),
    );
    replayed += 1;
}
```

- [ ] **Step 2: Write test + commit**

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core --lib wal_replay -- --nocapture
git add crates/trondb-core/src/executor.rs
git commit -m "feat(wal): replay handlers for EdgeInferred, EdgeConfirm, EdgeConfidenceUpdate"
```

---

### Task 15: Proto/gRPC Updates

**Files:**
- Modify: `crates/trondb-proto/proto/trondb.proto`
- Modify: `crates/trondb-proto/src/convert_plan.rs`

**Context:** Add InferPlan, ConfirmEdgePlan, ExplainHistoryPlan messages to proto and bidirectional conversions. Follow the exact pattern of existing plan message conversions.

- [ ] **Step 1: Add proto messages**

In `trondb.proto`, add messages after the existing plan messages:

```protobuf
message InferPlanProto {
    string from_id = 1;
    repeated string edge_types = 2;
    optional uint64 limit = 3;
    optional float confidence_floor = 4;
}

message ConfirmEdgePlanProto {
    string from_id = 1;
    string to_id = 2;
    string edge_type = 3;
    float confidence = 4;
}

message ExplainHistoryPlanProto {
    string entity_id = 1;
    optional uint64 limit = 2;
}
```

Add to `PlanRequest` oneof:
```protobuf
InferPlanProto infer = 18;
ConfirmEdgePlanProto confirm_edge = 19;
ExplainHistoryPlanProto explain_history = 20;
```

- [ ] **Step 2: Add conversion functions**

In `convert_plan.rs`, add arms to both `From<&Plan> for PlanRequest` and `TryFrom<PlanRequest> for Plan` match blocks. Follow the exact same pattern as existing conversions (e.g., TraversePlan).

- [ ] **Step 3: Run tests + commit**

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-proto -- --nocapture
CARGO_TARGET_DIR=/tmp/trondb-target cargo check --workspace
git add crates/trondb-proto/
git commit -m "feat(proto): InferPlan, ConfirmEdgePlan, ExplainHistoryPlan messages"
```

---

### Task 16: Engine Wiring + End-to-End Integration Tests

**Files:**
- Modify: `crates/trondb-core/src/lib.rs`

**Context:** Final wiring: Engine holds audit buffer, spawns sweepers, and a comprehensive E2E test exercises the full inference lifecycle.

- [ ] **Step 1: Wire InferenceAuditBuffer into Engine**

Ensure Engine struct has:
- `inference_audit: Arc<InferenceAuditBuffer>`
- `_inference_sweeper_handle: Option<JoinHandle<()>>`
- `_decay_sweeper_handle: Option<JoinHandle<()>>`

- [ ] **Step 2: Write E2E integration test**

```rust
#[tokio::test]
async fn end_to_end_inference_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _) = Engine::open(test_config(dir.path())).await.unwrap();

    // Setup collections
    create_simple_collection(&engine, "acts", 3).await;
    create_simple_collection(&engine, "venues", 3).await;

    // Edge type with INFER AUTO (need to test CREATE EDGE TYPE with inference config)
    engine.execute_tql("CREATE EDGE TYPE 'performs_at' FROM acts TO venues INFER AUTO CONFIDENCE > 0.5 LIMIT 5;").await.unwrap();

    // Insert entities
    engine.execute_tql("INSERT INTO acts (id, name) VALUES ('act1', 'Jazz Band') REPRESENTATION default VECTOR [0.9, 0.1, 0.0];").await.unwrap();
    engine.execute_tql("INSERT INTO venues (id, name) VALUES ('v1', 'Jazz Club') REPRESENTATION default VECTOR [0.85, 0.15, 0.0];").await.unwrap();
    engine.execute_tql("INSERT INTO venues (id, name) VALUES ('v2', 'Metal Pit') REPRESENTATION default VECTOR [0.0, 0.1, 0.9];").await.unwrap();

    // Explicit INFER
    let proposals = engine.execute_tql("INFER EDGES FROM 'act1' VIA performs_at RETURNING TOP 5;").await.unwrap();
    assert!(!proposals.rows.is_empty());

    // CONFIRM the top proposal
    engine.execute_tql("CONFIRM EDGE FROM 'act1' TO 'v1' TYPE performs_at CONFIDENCE 0.90;").await.unwrap();

    // Confirmed edge should be traversable
    let traversal = engine.execute_tql("TRAVERSE 'act1' VIA 'performs_at' DEPTH 1;").await.unwrap();
    assert_eq!(traversal.rows.len(), 1);

    // EXPLAIN HISTORY
    let history = engine.execute_tql("EXPLAIN HISTORY 'act1';").await.unwrap();
    // Should have at least the explicit INFER entry
    assert!(!history.rows.is_empty());
}
```

- [ ] **Step 3: Run full workspace test suite**

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace -- --nocapture
```

- [ ] **Step 4: Update CLAUDE.md**

Add Phase 11 section to CLAUDE.md documenting:
- EdgeSource classification
- INFER verb and execution pipeline
- CONFIRM verb behavior
- InferenceSweeper and DecaySweeper background tasks
- InferenceAuditBuffer + EXPLAIN HISTORY
- WAL record types used
- INFER AUTO on CREATE EDGE TYPE

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/ CLAUDE.md
git commit -m "feat: end-to-end inference pipeline — INFER, CONFIRM, sweepers, audit, integration tests"
```

---

## Post-Implementation

After all 16 tasks are complete:

1. Run `cargo clippy --workspace` and fix warnings
2. Run `cargo test --workspace` for final verification
3. Update README.md to show Phase 11 as complete
4. Use `superpowers:finishing-a-development-branch` skill
