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

#### Deletion Protocol (8 steps)
1. **WAL log** — `EntityDelete` record (0x11) with entity_id + collection
2. **Fjall delete** — Remove all representation keys for this entity
3. **HNSW tombstone** — Use existing `HnswIndex::remove()` from Phase 7a
4. **Field index removal** — Remove all field index entries for this entity
5. **Sparse index removal** — Remove from SparseIndex
6. **Location table removal** — Remove all LocationDescriptor entries
7. **Edge cleanup** — Using backward index, find+delete all edges TO this entity; using forward index, find+delete all edges FROM this entity. WAL-log each edge deletion.
8. **Tiered storage cleanup** — Delete from warm/archive Fjall partitions if present

#### Error Handling
- `DELETE` of nonexistent entity returns `EntityNotFound` error
- Entity must exist in the specified collection (checked before deletion)

### Implementation Notes
- New `Statement::Delete`, `Plan::DeleteEntity`, `DeleteEntityPlan`
- Executor calls cascading cleanup methods on store, indexes, adjacency
- Field index removal needs entity's field values — read entity from Fjall first, then delete
- Sparse index removal: existing `SparseIndex` has per-entity cleanup if we track which entities are in which terms (may need small addition)

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
- Post-filter for exclusive bounds (Gt/Lt) — lookup_range is inclusive, so strip boundary matches

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

#### Lazy Decay on Read
During TRAVERSE, compute effective confidence:
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
Parser already scaffolds DecayConfig fields — need to wire the syntax.

### Implementation Notes
- `created_at` uses `std::time::SystemTime::now().duration_since(UNIX_EPOCH).as_millis() as u64`
- DecaySweeper follows same pattern as TierMigrator (background tokio task, shared Engine Arc)
- Step decay: `rate` is interpreted as seconds threshold (not a rate per se)

## File Impact Summary

| File | Changes |
|------|---------|
| `trondb-tql/src/ast.rs` | Add Gte, Lte to WhereClause; DeleteStmt; TierTarget already exists |
| `trondb-tql/src/token.rs` | No changes (Gte/Lte tokens exist) |
| `trondb-tql/src/parser.rs` | Parse Gte/Lte in WHERE; parse DELETE stmt; parse DECAY in CREATE EDGE |
| `trondb-core/src/edge.rs` | Add backward index to AdjacencyIndex; add created_at to Edge; decay functions |
| `trondb-core/src/planner.rs` | DeleteEntityPlan; FieldIndexRange strategy; route range queries |
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
