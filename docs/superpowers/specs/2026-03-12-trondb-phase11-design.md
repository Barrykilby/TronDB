# Phase 11: Inference Pipeline — Design Spec

**Date**: 2026-03-12
**Status**: Approved
**Depends on**: Phase 10 (Pluggable Vectoriser + Mutation Cascade)
**Parent spec**: `2026-03-12-trondb-roadmap-phases-10-15.md` §4

---

## Goal

Give TronDB the ability to reason about relationships — propose edges that don't yet exist based on vector similarity, confirm or reject proposals, and run inference automatically in the background. This is the "inference" in inference-first.

## Scope

Full implementation of roadmap §4.1–4.6:
- Edge source classification (Structural / Inferred / Confirmed)
- INFER verb (on-demand edge proposals from vector similarity)
- CONFIRM verb (promote inferred edges to confirmed status)
- Background inference (async sweeper, configurable per edge type)
- Decay sweeper (prune inferred edges below threshold)
- Inference audit ring buffer + EXPLAIN HISTORY
- Planner + proto/gRPC integration

---

## 1. Edge Source Classification

### 1.1 EdgeSource Enum

```rust
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum EdgeSource {
    Structural,  // INSERT EDGE — app assertion, confidence always 1.0
    Inferred,    // INFER pipeline — engine proposal, decays, prunable
    Confirmed,   // CONFIRM EDGE — promoted from Inferred, no auto-prune
}

impl Default for EdgeSource {
    fn default() -> Self { EdgeSource::Structural }
}
```

### 1.2 Edge Struct Change

```rust
pub struct Edge {
    pub from_id: LogicalId,
    pub to_id: LogicalId,
    pub edge_type: String,
    pub confidence: f32,
    pub metadata: HashMap<String, Value>,
    pub created_at: u64,
    #[serde(default)]
    pub source: EdgeSource,  // NEW — defaults to Structural for backwards compat
}
```

### 1.3 AdjEntry Change

```rust
pub struct AdjEntry {
    pub to_id: LogicalId,
    pub confidence: f32,
    pub created_at: u64,
    pub source: EdgeSource,  // NEW
}
```

### 1.4 Lifecycle

```
Structural (confidence 1.0, permanent, app-asserted)
    — no transitions; structural edges are permanent

Inferred (confidence 0.0–1.0, subject to decay/pruning, engine-proposed)
    → CONFIRM → Confirmed

Confirmed (confidence set by CONFIRM, excluded from auto-pruning)
    — re-confirmable (updates confidence)
```

### 1.5 Backward Compatibility

`#[serde(default)]` on the `source` field means existing Fjall-persisted edges deserialize as `Structural`. No migration needed.

---

## 2. INFER Verb

### 2.1 TQL Syntax

```sql
-- Propose top 10 edges of any type
INFER EDGES FROM 'ent_abc123' RETURNING TOP 10;

-- With confidence floor
INFER EDGES FROM 'ent_abc123' RETURNING TOP 10 CONFIDENCE > 0.70;

-- Scoped to specific edge types
INFER EDGES FROM 'ent_abc123' VIA PERFORMED_AT, HEADLINED_BY
    RETURNING TOP 5 CONFIDENCE > 0.80;

-- All edges above threshold
INFER EDGES FROM 'ent_abc123' RETURNING ALL CONFIDENCE > 0.90;
```

### 2.2 AST

```rust
pub struct InferStmt {
    pub from_id: String,
    pub edge_types: Vec<String>,       // empty = all applicable
    pub limit: Option<usize>,          // None = ALL
    pub confidence_floor: Option<f32>, // CONFIDENCE > threshold
}
```

### 2.3 Execution Pipeline

1. **Fetch source entity** — get entity and its vector representations
2. **Determine target collections** — if VIA specified, use edge type's `to_collection`; otherwise scan all declared edge types where `from_collection` matches the source entity's collection
3. **HNSW search per target** — for each target collection, search using the source entity's vector against the target's HNSW index. Use the first matching representation (same dimensionality).
4. **Score candidates** — raw HNSW similarity score becomes the proposed confidence
5. **Filter** — exclude entities that already have an edge of this type from the source (any source classification); apply confidence floor
6. **Merge + rank** — merge results across edge types, sort by confidence descending, apply TOP limit
7. **Return** — query result rows with `from_id`, `to_id`, `edge_type`, `confidence` — nothing written to the graph

### 2.4 Planner

New `InferPlan` type:
```rust
pub struct InferPlan {
    pub from_id: String,
    pub edge_types: Vec<String>,
    pub limit: Option<usize>,
    pub confidence_floor: Option<f32>,
}
```

Strategy is always vector similarity (HNSW). The planner validates edge types exist, resolves collections, and builds the plan.

---

## 3. CONFIRM Verb

### 3.1 TQL Syntax

```sql
CONFIRM EDGE FROM 'ent_abc123' TO 'ent_def456'
    TYPE PERFORMED_AT
    CONFIDENCE 0.95;
```

### 3.2 AST

```rust
pub struct ConfirmEdgeStmt {
    pub from_id: String,
    pub to_id: String,
    pub edge_type: String,
    pub confidence: f32,
}
```

### 3.3 Behavior

| Existing Edge | Action |
|---------------|--------|
| None | Create new edge with `source: Confirmed` |
| Inferred | Transition to `Confirmed`, update confidence |
| Confirmed | Update confidence (idempotent re-confirmation) |
| Structural | Error — structural edges are already permanent at 1.0 |

### 3.4 WAL Records

- New confirmed edge or updated existing: `EdgeConfirm` (0x33)
- Confidence update on existing: `EdgeConfidenceUpdate` (0x32)

### 3.5 TRAVERSE Interaction

Confirmed edges are visible in TRAVERSE like structural edges. They respect decay configuration but are excluded from automatic pruning (DecaySweeper skips `source: Confirmed`).

---

## 4. Background Inference

### 4.1 InferenceSweeper

A background task following the same pattern as TierMigrator. Runs on a configurable interval (default 30s).

**Each cycle:**
1. Drain the inference work queue (entities flagged by INSERT/UPDATE handlers)
2. For each queued entity, determine applicable edge types (those with `INFER AUTO`)
3. Run the INFER pipeline for each applicable edge type
4. Write accepted edges as `Inferred` (WAL: `EdgeInferred` 0x31)
5. Deduplicate: skip if an edge of the same type already exists between the pair (any source)

### 4.2 Work Queue

A `DashSet<LogicalId>` of entities pending inference. Populated by:
- INSERT: after entity write, if any edge type with `INFER AUTO` has `from_collection` matching the entity's collection
- UPDATE: after entity mutation, same check

The queue is RAM-only — if the process crashes, pending inference work is lost. This is acceptable because inference is speculative, not authoritative.

### 4.3 Edge Type Configuration

Extended CREATE EDGE TYPE syntax:

```sql
CREATE EDGE TYPE 'performs_at'
  FROM acts TO venues
  DECAY EXPONENTIAL RATE 0.05 FLOOR 0.1 PRUNE 0.05
  INFER AUTO CONFIDENCE > 0.75 LIMIT 5;
```

- `INFER AUTO` — enable background inference for this edge type
- `CONFIDENCE > 0.75` — admission gate (proposals below this are discarded)
- `LIMIT 5` — max inferred edges per source entity per cycle

Edge types without `INFER AUTO` only produce edges via explicit `INFER EDGES` queries.

### 4.4 InferenceConfig

```rust
pub struct InferenceConfig {
    pub auto: bool,              // INFER AUTO enabled
    pub confidence_floor: f32,   // admission gate
    pub limit: usize,            // max inferred edges per entity per cycle
}
```

Added to `EdgeType` struct with `#[serde(default)]`.

---

## 5. DecaySweeper

The DecaySweeper concept (referenced in CLAUDE.md but not fully built) gets proper implementation.

### 5.1 Behavior

Runs on configurable interval (default 60s). Each cycle:

1. Scan all edges with `source: Inferred` (structural and confirmed are excluded)
2. Compute effective confidence via the edge type's decay function
3. If below `prune_threshold` — delete the edge (WAL: `EdgeDelete` 0x34)
4. Update the AdjacencyIndex

### 5.2 Implementation

Uses `AdjacencyIndex` iteration to find inferred edges, then computes decay and prunes. The sweeper holds a reference to the executor for WAL writes and Fjall deletes.

---

## 6. Inference Audit Ring Buffer

### 6.1 Purpose

Observability for the inference engine. Every INFER execution (explicit or background) records its reasoning in a RAM-resident ring buffer.

### 6.2 Data Model

```rust
pub struct InferenceAuditEntry {
    pub timestamp: u64,
    pub source_entity: LogicalId,
    pub edge_type: String,
    pub candidates_evaluated: usize,
    pub candidates_above_threshold: usize,
    pub top_candidates: Vec<InferenceCandidate>,
    pub trigger: InferenceTrigger,
}

pub struct InferenceCandidate {
    pub entity_id: LogicalId,
    pub similarity_score: f32,
    pub accepted: bool,
}

pub enum InferenceTrigger {
    Explicit,    // INFER EDGES query
    Background,  // InferenceSweeper
}
```

### 6.3 Buffer

`Mutex<VecDeque<InferenceAuditEntry>>` with configurable capacity (default 1000). Oldest entries evicted when full. Low contention — writes are infrequent.

### 6.4 EXPLAIN HISTORY

```sql
EXPLAIN HISTORY 'ent_abc123' LIMIT 50;
```

Returns the most recent audit entries for the given entity, filtered from the ring buffer. Shows candidates evaluated, scores, threshold pass/fail, and trigger type.

---

## 7. Planner + Proto Integration

### 7.1 New Plan Types

```rust
pub struct InferPlan {
    pub from_id: String,
    pub edge_types: Vec<String>,
    pub limit: Option<usize>,
    pub confidence_floor: Option<f32>,
}

pub struct ConfirmEdgePlan {
    pub from_id: String,
    pub to_id: String,
    pub edge_type: String,
    pub confidence: f32,
}

pub struct ExplainHistoryPlan {
    pub entity_id: String,
    pub limit: Option<usize>,
}
```

### 7.2 Proto Updates

Three new plan messages in `trondb.proto`:
- `InferPlan` message
- `ConfirmEdgePlan` message
- `ExplainHistoryPlan` message

Bidirectional conversion functions following existing patterns.

### 7.3 WAL Record Usage

| Record | Code | Used By |
|--------|------|---------|
| EdgeInferred | 0x31 | Background inference — new inferred edge |
| EdgeConfidenceUpdate | 0x32 | CONFIRM — update existing edge confidence |
| EdgeConfirm | 0x33 | CONFIRM — new confirmed edge or source transition |
| EdgeWrite | 0x30 | INSERT EDGE — structural (existing, unchanged) |
| EdgeDelete | 0x34 | DecaySweeper — pruned inferred edge |

All WAL record types already defined in the enum. Replay handlers needed for EdgeInferred, EdgeConfidenceUpdate, EdgeConfirm.

---

## 8. File Impact

### Modified files:
- `crates/trondb-core/src/edge.rs` — EdgeSource enum, Edge struct, AdjEntry, InferenceConfig
- `crates/trondb-core/src/executor.rs` — INFER + CONFIRM execution, inference work queue, background inference trigger on INSERT/UPDATE
- `crates/trondb-core/src/planner.rs` — InferPlan, ConfirmEdgePlan, ExplainHistoryPlan
- `crates/trondb-core/src/lib.rs` — InferenceSweeper, DecaySweeper, audit ring buffer, Engine wiring
- `crates/trondb-tql/src/token.rs` — INFER, CONFIRM, RETURNING, HISTORY tokens
- `crates/trondb-tql/src/ast.rs` — InferStmt, ConfirmEdgeStmt, ExplainHistoryStmt
- `crates/trondb-tql/src/parser.rs` — INFER, CONFIRM, EXPLAIN HISTORY parsing
- `crates/trondb-wal/src/record.rs` — no changes (record types already defined)
- `crates/trondb-proto/proto/trondb.proto` — new plan messages
- `crates/trondb-proto/src/convert_plan.rs` — new conversion functions

### New files:
- `crates/trondb-core/src/inference.rs` — InferenceAuditBuffer, InferenceSweeper, inference pipeline logic (extracted from executor to keep file size manageable)
