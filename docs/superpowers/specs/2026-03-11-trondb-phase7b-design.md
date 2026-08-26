# Phase 7b Design: Graph Depth, Entity Lifecycle, Range Queries, Edge Decay

**Goal:** Complete the deferred capabilities from earlier phases — multi-hop TRAVERSE, entity deletion, range-based FETCH, and edge confidence decay.

**Architecture:** Four independent features that build on existing scaffolding (WhereClause Gt/Lt, FieldIndex.lookup_range(), RecordType::EntityDelete, DecayConfig). Each can be implemented and tested independently.

## 1. Multi-hop TRAVERSE

### Current State
- `executor.rs:828` gates DEPTH > 1 with error "TRAVERSE DEPTH > 1 requires Phase 6+"
- AdjacencyIndex stores forward edges only: `DashMap<(LogicalId, String), Vec<AdjEntry>>`
- Single-hop: looks up adjacents, fetches entities, returns flat result

### Design
- **Algorithm:** BFS with `HashSet<LogicalId>` visited set for cycle detection
- **Max depth:** Hard cap at 10 (prevents runaway queries). Configurable via future `SET` command.
- **Direction:** Forward-only (from→to). Bidirectional is future work.
- **Result shape:** Flat entity set (all entities reachable within DEPTH hops). No path tracking.
- **Single edge type per TRAVERSE:** Each TRAVERSE uses one edge type across all hops. The `to_collection` of that edge type determines which collection entities are fetched from at each hop. Cross-edge-type graph walks are future work.
- **Each hop:** `AdjacencyIndex::get(frontier_id, edge_type)` → filter visited → add to next frontier → fetch entities from Fjall

### TQL (unchanged syntax)
```
TRAVERSE edge_type FROM "entity_id" DEPTH 3 LIMIT 100
```

### Implementation Notes
- Remove the DEPTH > 1 gate
- Add BFS loop around existing single-hop logic
- visited set prevents infinite cycles in cyclic graphs
- LIMIT applies to total result count across all hops
- EXPLAIN shows depth reached, entities per hop

## 2. Entity Deletion

### Current State
- No DELETE entity support (executor.rs:85-87 comment: "Phase 2 doesn't support DELETE yet")
- `RecordType::EntityDelete` (0x11) exists in WAL but is never used
- No backward edge index (can't efficiently find edges pointing TO an entity)

### Design

#### TQL
```
DELETE "entity_id" FROM collection
```

#### Backward Index
Add reverse lookup to AdjacencyIndex:
```rust
pub struct AdjacencyIndex {
    forward: DashMap<(LogicalId, String), Vec<AdjEntry>>,
    backward: DashMap<(LogicalId, String), Vec<LogicalId>>,  // NEW
}
```
- `backward` maps `(to_id, edge_type) → Vec<from_id>`
- Updated on edge insert/delete
- Rebuilt from Fjall on startup (same scan as forward index)
- Enables efficient cascade: "find all edges pointing to this entity"

#### Deletion Protocol (9 steps)
1. **WAL log** — `EntityDelete` record (0x11) with entity_id + collection
2. **Read entity** — Fetch full entity (fields + representations) from Fjall BEFORE any deletes. Needed for field/sparse index cleanup.
3. **HNSW tombstone** — Use existing `HnswIndex::remove()` from Phase 7a
4. **Field index removal** — Remove all field index entries using the entity's field values (from step 2)
5. **Sparse index removal** — Remove from SparseIndex using original sparse vectors (from step 2)
6. **Location table removal** — Remove all LocationDescriptor entries
7. **Fjall delete** — Remove all representation keys for this entity (after indexes are cleaned)
8. **Edge cleanup** — Iterate all registered edge types. For each type: use backward index `(entity_id, edge_type)` to find+delete all edges TO this entity; use forward index to find+delete all edges FROM this entity. WAL-log each edge deletion.
9. **Tiered storage cleanup** — Delete from warm/archive Fjall partitions if present

#### Error Handling
- `DELETE` of nonexistent entity returns `EntityNotFound` error
- Entity must exist in the specified collection (checked before deletion)

### Implementation Notes
- New `Statement::Delete`, `Plan::DeleteEntity`, `DeleteEntityPlan`
- Executor calls cascading cleanup methods on store, indexes, adjacency
- Entity data (fields + sparse vectors) must be read from Fjall BEFORE deletion — needed for index cleanup
- SparseIndex removal requires original sparse vector: `SparseIndex::remove(entity_id, vector)`. The vector is obtained from the entity read in step 2.

## 3. Range FETCH

### Current State
- WhereClause has Gt/Lt but NOT Gte/Lte
- Lexer has `>=`/`<=` tokens (Gte/Lte) but parser ignores them
- `select_fetch_strategy()` only routes Eq → FieldIndexLookup
- `FieldIndex::lookup_range()` already exists and works
- FullScan path handles Gt/Lt via `entity_matches()` but not Gte/Lte

### Design

#### TQL (extends WHERE syntax)
```
FETCH * FROM venues WHERE score >= 80 LIMIT 10
FETCH * FROM venues WHERE score > 50 AND score < 100
```

#### AST Changes
Add to `WhereClause`:
```rust
Gte(String, Literal),
Lte(String, Literal),
```

#### Parser Changes
Wire `Token::Gte` and `Token::Lte` in `parse_where_comparison()`:
```rust
Some((Token::Gte, _)) => Ok(WhereClause::Gte(field, self.parse_literal()?)),
Some((Token::Lte, _)) => Ok(WhereClause::Lte(field, self.parse_literal()?)),
```

#### Planner Changes
New strategy: `FetchStrategy::FieldIndexRange(String)`
- `select_fetch_strategy()` routes Gt/Lt/Gte/Lte to FieldIndexRange when field is indexed
- Falls back to FullScan when no index covers the field

#### Executor Changes
For FieldIndexRange:
- Compute lower/upper bounds from the WhereClause
- Open-ended ranges use type min/max (e.g., `i64::MIN` for unbounded lower)
- Call `FieldIndex::lookup_range(lower, upper)`
- Post-filter for exclusive bounds (Gt/Lt): `lookup_range` returns entity IDs; re-read entity field values to check boundary exclusion, or over-fetch with inclusive bounds and filter via `entity_matches()`

#### entity_matches() Extension
Add Gte/Lte handling for FullScan path (same pattern as existing Gt/Lt).

## 4. Edge Confidence Decay

### Current State
- `DecayConfig` fully defined: decay_fn, decay_rate, floor, promote_threshold, prune_threshold
- `DecayFn` enum: Exponential, Linear, Step
- All edges store `confidence: f32` (always 1.0 currently)
- `RecordType::EdgeConfidenceUpdate` (0x32) and `RecordType::EdgeDelete` (0x34) exist
- No timestamp on edges

### Design

#### Edge Timestamp
Add `created_at: u64` (epoch millis) to Edge struct. Set to current time at INSERT EDGE. Default 0 for existing edges (no decay applied when `created_at == 0`).

Backward-compatible: `#[serde(default)]` for Fjall deserialisation of existing edges.

#### AdjEntry Extension
Add `created_at: u64` to `AdjEntry` so TRAVERSE can compute decay without Fjall reads:
```rust
pub struct AdjEntry {
    pub to_id: LogicalId,
    pub confidence: f32,
    pub created_at: u64,  // NEW — epoch millis, 0 for legacy
}
```
Populated from Edge.created_at during edge insert and startup rebuild.

#### Lazy Decay on Read
During TRAVERSE, compute effective confidence using AdjEntry data (no Fjall read needed):
```rust
fn effective_confidence(edge: &Edge, config: &DecayConfig) -> f32 {
    if edge.created_at == 0 || config.decay_fn.is_none() {
        return edge.confidence;
    }
    let elapsed_secs = (now_millis() - edge.created_at) as f64 / 1000.0;
    let rate = config.decay_rate.unwrap_or(0.0);
    let floor = config.floor.unwrap_or(0.0);

    let decayed = match config.decay_fn.as_ref().unwrap() {
        DecayFn::Exponential => (edge.confidence as f64) * (-rate * elapsed_secs).exp(),
        DecayFn::Linear => (edge.confidence as f64) * (1.0 - rate * elapsed_secs),
        DecayFn::Step => if elapsed_secs > rate { floor } else { edge.confidence as f64 },
    };
    decayed.max(floor).min(1.0) as f32
}
```

- TRAVERSE filters edges where effective_confidence < prune_threshold
- TRAVERSE result includes effective confidence (not stored confidence)
- No write on read — purely computed

#### Background Decay Sweeper
- Periodic tokio task (configurable interval, default 60s)
- Scans edge types with non-None decay_config
- For each edge: compute effective_confidence
- If below prune_threshold → hard-delete (Fjall + AdjacencyIndex + WAL EdgeDelete)
- If above promote_threshold → could boost (future: re-insert with higher confidence)
- Light-touch: only processes one edge type per cycle to avoid lock contention

#### CREATE EDGE Syntax Extension
```
CREATE EDGE knows FROM people TO people
    DECAY EXPONENTIAL RATE 0.001
    FLOOR 0.1
    PRUNE 0.05
```
The `DecayConfig` struct exists in core but `CreateEdgeTypeStmt` and `CreateEdgeTypePlan` lack decay fields. Both need extending:
- `CreateEdgeTypeStmt` gains `decay_config: Option<DecayConfig>` (from AST)
- `CreateEdgeTypePlan` gains matching field (from planner)
- Parser adds DECAY/FLOOR/PRUNE keyword handling after FROM...TO clause
- Executor passes decay_config to EdgeType construction

### Implementation Notes
- `created_at` uses `std::time::SystemTime::now().duration_since(UNIX_EPOCH).as_millis() as u64`
- DecaySweeper follows same pattern as TierMigrator (background tokio task, shared Engine Arc)
- Step decay: `rate` is interpreted as seconds threshold (not a rate per se)

## File Impact Summary

| File | Changes |
|------|---------|
| `trondb-tql/src/ast.rs` | Add Gte, Lte to WhereClause; DeleteStmt; CreateEdgeTypeStmt decay fields |
| `trondb-tql/src/token.rs` | No changes (Gte/Lte tokens exist) |
| `trondb-tql/src/parser.rs` | Parse Gte/Lte in WHERE; parse DELETE stmt; parse DECAY in CREATE EDGE |
| `trondb-core/src/edge.rs` | Add backward index to AdjacencyIndex; add created_at to Edge; decay functions |
| `trondb-core/src/planner.rs` | DeleteEntityPlan; FieldIndexRange strategy; route range queries; add Gte/Lte to first_field_in_clause() |
| `trondb-core/src/executor.rs` | Multi-hop BFS; DELETE handler; FieldIndexRange handler; Gte/Lte in entity_matches |
| `trondb-core/src/store.rs` | delete_entity() method |
| `trondb-core/src/lib.rs` | Engine delete_entity() method |
| `trondb-routing/src/sweeper.rs` | NEW: DecaySweeper background task |
| `trondb-routing/src/router.rs` | Wire DELETE through router; optional DecaySweeper |
| `trondb-cli/src/main.rs` | Wire DecaySweeper construction |

## Testing Strategy

Each feature gets unit tests + integration tests:
- **Multi-hop TRAVERSE:** cycle detection, depth limiting, cross-collection traversal
- **Entity deletion:** cascading cleanup verification, nonexistent entity error, edge cascade
- **Range FETCH:** indexed range scan, open-ended ranges, FullScan Gte/Lte, boundary exclusion
- **Edge decay:** decay function correctness, lazy filtering, sweeper hard-delete, backward-compat (created_at=0)
