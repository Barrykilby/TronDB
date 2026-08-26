# TronDB Phase 4 — HNSW Vector Index

## Goal

Ungating SEARCH: build per-collection HNSW indexes using `hnsw_rs`, wire them into INSERT (index vectors) and SEARCH (query vectors), return scored results with confidence filtering. The HNSW index is part of the control fabric — always in RAM, rebuilt from Fjall on startup.

## Source Requirements

- v4 design doc Table 3: HNSW topology lives in control fabric (always in RAM)
- v4 Table 19 Phase 4 row
- v3 design doc: vector index per collection
- PoC design doc §HNSW index: one index per collection, keyed by LogicalId
- Phase 3 spec §Two Planes: control fabric contains "HNSW topology (Phase 4+)"

## Architecture

One `Hnsw<f32, DistCosine>` instance per collection, stored in a `DashMap<String, HnswIndex>` on the Executor. No new persistence format — Fjall + WAL remain the sole source of truth. On startup, indexes are rebuilt by scanning entities from Fjall. During operation, indexes are maintained incrementally via INSERT.

### Control Fabric (updated from Phase 3)

| Component | Phase 3 | Phase 4 |
|-----------|---------|---------|
| Location Table | DashMap, WAL-logged, snapshotted | Unchanged |
| HNSW Indexes | — | DashMap<String, HnswIndex>, rebuilt from Fjall |

## Dependencies

Add to workspace `Cargo.toml`:
```toml
hnsw_rs = "0.3"
anndists = "0.2"
```

Add to `crates/trondb-core/Cargo.toml`:
```toml
hnsw_rs.workspace = true
anndists.workspace = true
```

## Types

### HnswIndex

New file: `crates/trondb-core/src/index.rs`

```rust
use std::sync::atomic::{AtomicUsize, Ordering};

use anndists::dist::DistCosine;
use dashmap::DashMap;
use hnsw_rs::hnsw::Hnsw;

use crate::types::LogicalId;

/// HNSW parameters — sensible defaults for single-node.
const MAX_NB_CONNECTION: usize = 16;  // M: connections per node per layer
const MAX_ELEMENTS: usize = 100_000;  // initial capacity
const MAX_LAYER: usize = 16;
const EF_CONSTRUCTION: usize = 200;   // build-time beam width
const EF_SEARCH: usize = 50;          // query-time beam width

pub struct HnswIndex {
    inner: Hnsw<f32, DistCosine>,
    dimensions: usize,
    id_to_idx: DashMap<LogicalId, usize>,
    idx_to_id: DashMap<usize, LogicalId>,
    next_idx: AtomicUsize,
}
```

The `id_to_idx` / `idx_to_id` maps translate between TronDB's `LogicalId` and `hnsw_rs`'s `usize` external ID. This bidirectional mapping is needed because `hnsw_rs` returns `DataId` (usize) from search results, and we need to resolve back to entities.

### HnswIndex API

```rust
impl HnswIndex {
    pub fn new(dimensions: usize) -> Self;

    /// Insert a vector for an entity. If the entity already exists in the index,
    /// this is a no-op (idempotent for WAL replay / Fjall scan rebuild).
    pub fn insert(&self, id: &LogicalId, vector: &[f32]);

    /// Search for k nearest neighbours.
    /// Returns (LogicalId, similarity_score) sorted by descending similarity.
    /// similarity = 1.0 - cosine_distance (hnsw_rs DistCosine returns distance).
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(LogicalId, f32)>;

    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn dimensions(&self) -> usize;
}
```

### Score Conversion

`hnsw_rs` `DistCosine` returns distance `d = 1.0 - cosine_similarity`. We convert:
- `similarity = 1.0 - distance`
- Confidence threshold filters on `similarity >= threshold`
- Results sorted by descending similarity (highest first)

## Executor Changes

### Struct

```rust
pub struct Executor {
    store: FjallStore,
    wal: WalWriter,
    location: Arc<LocationTable>,
    indexes: DashMap<String, HnswIndex>,  // NEW: one per collection
}
```

### INSERT Path

After existing Fjall + LocationTable writes (same WAL transaction, no new WAL records):

```
if entity has representations:
    get-or-create HnswIndex for collection (using collection dimensions)
    for each representation:
        index.insert(entity_id, repr.vector)
```

The HnswIndex is created lazily on first insert with vectors. Collections without vectors never create an index.

### SEARCH Path

Replace the current `UnsupportedOperation` error:

```
1. Cast query vector f64 → f32
2. Get collection dimensions, validate query vector length matches
3. Look up HnswIndex for collection (if none, return empty results)
4. index.search(query_f32, k) → Vec<(LogicalId, similarity)>
5. Filter: similarity >= confidence_threshold
6. Resolve each LogicalId → Entity from Fjall store
7. Build rows with score = Some(similarity)
8. Return QueryResult with mode = QueryMode::Probabilistic, tier = "Ram"
```

### WAL Replay

No changes to WAL replay — the index is rebuilt from Fjall after replay completes, not from WAL records. INSERT replay writes to Fjall, then the full rebuild populates the index.

## Engine::open — Index Rebuild

After WAL replay, scan all collections and rebuild HNSW indexes:

```rust
// After replay completes:
for collection_name in executor.collections() {
    let entities = executor.store.scan(&collection_name)?;
    let dims = executor.store.get_dimensions(&collection_name)?;
    for entity in &entities {
        if !entity.representations.is_empty() {
            let index = executor.indexes
                .entry(collection_name.clone())
                .or_insert_with(|| HnswIndex::new(dims));
            for (idx, repr) in entity.representations.iter().enumerate() {
                index.insert(&entity.id, &repr.vector);
            }
        }
    }
}
```

This runs once on startup. The cost is O(n) where n is total entities with vectors. At single-node scale this is negligible.

## EXPLAIN Changes

SEARCH explain output (in `explain_plan()`):

```rust
Plan::Search(p) => {
    props.push(("mode", "Probabilistic".into()));
    props.push(("verb", "SEARCH".into()));
    props.push(("collection", p.collection.clone()));
    props.push(("tier", "Ram".into()));
    props.push(("encoding", "Float32".into()));
    props.push(("strategy", "HNSW".into()));
    props.push(("k", p.k.to_string()));
    props.push(("confidence_threshold", p.confidence_threshold.to_string()));
}
```

## Error Handling

- SEARCH on non-existent collection → `CollectionNotFound` (existing error)
- SEARCH with wrong dimension vector → `DimensionMismatch` (existing error)
- SEARCH on collection with no indexed vectors → return empty results (not an error)
- No new error variants needed

## Crate Placement

- New file: `crates/trondb-core/src/index.rs`
- `hnsw_rs` + `anndists` added as workspace dependencies
- No new crate needed — HNSW index is engine-internal

## What Phase 4 Does NOT Build

- Mutation dirty-marking (ReprState::Dirty / LocState::Dirty driven by entity changes)
- REPR_WRITE (0x20) / REPR_DIRTY (0x21) WAL records
- HNSW index serialization/persistence (rebuild from Fjall is sufficient)
- Multi-node index sharding
- Background recomputation task (Dirty → Recomputing → Clean)

## Testing

- Unit tests for HnswIndex (insert, search, empty index, idempotent insert)
- Unit test: score conversion (distance → similarity)
- Unit test: search results sorted by descending similarity
- Integration test: INSERT with vector → SEARCH returns it with score
- Integration test: confidence filtering excludes low-similarity results
- Integration test: SEARCH on collection with no vectors returns empty results
- Integration test: EXPLAIN SEARCH shows Probabilistic mode + HNSW strategy
- Integration test: index rebuilt after restart (insert → restart → search finds entity)
- Integration test: dimension mismatch on SEARCH returns error
