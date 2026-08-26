# Phase 4: HNSW Vector Index Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ungating SEARCH — build per-collection HNSW indexes using `hnsw_rs`, wire them into INSERT and SEARCH, return scored results with confidence filtering.

**Architecture:** One `Hnsw<f32, DistCosine>` per collection in a `DashMap<String, HnswIndex>` on the Executor. Indexes live in RAM (control fabric), rebuilt from Fjall on startup. No new persistence format.

**Tech Stack:** hnsw_rs 0.3, anndists 0.2, DashMap 6 (already present), existing trondb-core infrastructure.

**Spec:** `docs/superpowers/specs/2026-03-11-trondb-phase4-design.md`

---

## Chunk 1: Dependencies and HnswIndex Type

### Task 1: Add hnsw_rs and anndists dependencies

**Files:**
- Modify: `Cargo.toml` (workspace deps)
- Modify: `crates/trondb-core/Cargo.toml` (add deps)

- [ ] **Step 1: Add workspace dependencies**

In `Cargo.toml` (workspace root), add under `[workspace.dependencies]`:
```toml
hnsw_rs = "0.3"
anndists = "0.2"
```

- [ ] **Step 2: Add to trondb-core**

In `crates/trondb-core/Cargo.toml`, add under `[dependencies]`:
```toml
hnsw_rs.workspace = true
anndists.workspace = true
```

- [ ] **Step 3: Verify compilation**

Run: `cargo check --workspace`
Expected: Compiles successfully.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/trondb-core/Cargo.toml
git commit -m "deps: add hnsw_rs and anndists for Phase 4 vector index"
```

---

### Task 2: Create HnswIndex type with insert and search

**Files:**
- Create: `crates/trondb-core/src/index.rs`
- Modify: `crates/trondb-core/src/lib.rs:1-7` (add `pub mod index;`)

- [ ] **Step 1: Create index.rs**

Create `crates/trondb-core/src/index.rs`:

```rust
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anndists::dist::DistCosine;
use dashmap::DashMap;
use hnsw_rs::hnsw::{Hnsw, Neighbour};

use crate::types::LogicalId;

// ---------------------------------------------------------------------------
// HNSW parameters — sensible defaults for single-node
// ---------------------------------------------------------------------------

const MAX_NB_CONNECTION: usize = 16;
const MAX_ELEMENTS: usize = 100_000;
const MAX_LAYER: usize = 16;
const EF_CONSTRUCTION: usize = 200;
const EF_SEARCH: usize = 50;

// ---------------------------------------------------------------------------
// HnswIndex — one per collection
// ---------------------------------------------------------------------------

pub struct HnswIndex {
    inner: Mutex<Hnsw<'static, f32, DistCosine>>,
    dimensions: usize,
    id_to_idx: DashMap<LogicalId, usize>,
    idx_to_id: DashMap<usize, LogicalId>,
    next_idx: AtomicUsize,
}

impl HnswIndex {
    pub fn new(dimensions: usize) -> Self {
        let hnsw = Hnsw::new(
            MAX_NB_CONNECTION,
            MAX_ELEMENTS,
            MAX_LAYER,
            EF_CONSTRUCTION,
            DistCosine,
        );
        Self {
            inner: Mutex::new(hnsw),
            dimensions,
            id_to_idx: DashMap::new(),
            idx_to_id: DashMap::new(),
            next_idx: AtomicUsize::new(0),
        }
    }

    /// Insert a vector for an entity. Idempotent: skips if already indexed.
    pub fn insert(&self, id: &LogicalId, vector: &[f32]) {
        // Idempotent — skip if already in the index
        if self.id_to_idx.contains_key(id) {
            return;
        }

        let idx = self.next_idx.fetch_add(1, Ordering::SeqCst);
        self.id_to_idx.insert(id.clone(), idx);
        self.idx_to_id.insert(idx, id.clone());

        let hnsw = self.inner.lock().unwrap();
        hnsw.insert((&vector.to_vec(), idx));
    }

    /// Search for k nearest neighbours.
    /// Returns (LogicalId, similarity_score) sorted by descending similarity.
    /// similarity = 1.0 - cosine_distance.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(LogicalId, f32)> {
        if self.is_empty() {
            return Vec::new();
        }

        let hnsw = self.inner.lock().unwrap();
        let neighbours: Vec<Neighbour> = hnsw.search(query, k, EF_SEARCH);

        let mut results: Vec<(LogicalId, f32)> = neighbours
            .into_iter()
            .filter_map(|n| {
                let idx = n.d_id;
                let similarity = 1.0 - n.distance;
                self.idx_to_id.get(&idx).map(|id| (id.clone(), similarity))
            })
            .collect();

        // Sort by descending similarity
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results
    }

    pub fn len(&self) -> usize {
        self.id_to_idx.len()
    }

    pub fn is_empty(&self) -> bool {
        self.id_to_idx.is_empty()
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }
}
```

**Important note on `hnsw_rs` API**: The `Hnsw::insert` method takes `(&[T], usize)` — a tuple of (data slice, external ID). The `Hnsw::search` method takes `(&[T], k, ef_search)` and returns `Vec<Neighbour>`. The `Neighbour` struct has `d_id: usize` (the external ID we passed) and `distance: f32`. `DistCosine` returns `1.0 - cosine_similarity` as distance.

The `Hnsw` type requires a `'static` lifetime for the data it references. We use `Mutex` wrapping because `Hnsw` is not `Sync`. The `insert` method may need to pass owned data — check the `hnsw_rs` API and adjust if `insert` expects `(&Vec<T>, usize)` rather than `(&[T], usize)`.

- [ ] **Step 2: Add module to lib.rs**

Add `pub mod index;` to `crates/trondb-core/src/lib.rs` after line 6 (`pub mod location;`).

- [ ] **Step 3: Verify compilation**

Run: `cargo check --workspace`
Expected: Compiles. If `hnsw_rs` API differs from what's written above (lifetime issues, method signatures), adjust accordingly. The key contract is:
- `insert` takes an entity ID + vector, maps to hnsw_rs internal ID
- `search` takes a query vector + k, returns (LogicalId, similarity) pairs
- Idempotent insert (skip duplicates)

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-core/src/index.rs crates/trondb-core/src/lib.rs
git commit -m "feat(index): add HnswIndex type with insert and search"
```

---

### Task 3: HnswIndex unit tests

**Files:**
- Modify: `crates/trondb-core/src/index.rs` (add `#[cfg(test)] mod tests`)

- [ ] **Step 1: Add unit tests**

Add to the bottom of `crates/trondb-core/src/index.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn make_id(s: &str) -> LogicalId {
        LogicalId::from_string(s)
    }

    #[test]
    fn insert_and_search() {
        let idx = HnswIndex::new(3);
        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        idx.insert(&make_id("e2"), &[0.0, 1.0, 0.0]);
        idx.insert(&make_id("e3"), &[0.9, 0.1, 0.0]);

        let results = idx.search(&[1.0, 0.0, 0.0], 3);
        assert_eq!(results.len(), 3);
        // e1 should be the closest match (exact vector)
        assert_eq!(results[0].0, make_id("e1"));
        // similarity should be close to 1.0
        assert!(results[0].1 > 0.99);
    }

    #[test]
    fn search_empty_index() {
        let idx = HnswIndex::new(3);
        let results = idx.search(&[1.0, 0.0, 0.0], 5);
        assert!(results.is_empty());
    }

    #[test]
    fn idempotent_insert() {
        let idx = HnswIndex::new(3);
        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]); // duplicate
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn similarity_score_range() {
        let idx = HnswIndex::new(3);
        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        idx.insert(&make_id("e2"), &[0.0, 1.0, 0.0]); // orthogonal

        let results = idx.search(&[1.0, 0.0, 0.0], 2);
        assert_eq!(results.len(), 2);
        // First result (e1) should have high similarity
        assert!(results[0].1 > 0.9);
        // Second result (e2) should have low similarity (orthogonal ≈ 0.0)
        assert!(results[1].1 < 0.1);
    }

    #[test]
    fn results_sorted_descending() {
        let idx = HnswIndex::new(3);
        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        idx.insert(&make_id("e2"), &[0.5, 0.5, 0.0]);
        idx.insert(&make_id("e3"), &[0.0, 1.0, 0.0]);

        let results = idx.search(&[1.0, 0.0, 0.0], 3);
        for w in results.windows(2) {
            assert!(w[0].1 >= w[1].1, "results should be sorted by descending similarity");
        }
    }

    #[test]
    fn len_and_dimensions() {
        let idx = HnswIndex::new(5);
        assert_eq!(idx.dimensions(), 5);
        assert_eq!(idx.len(), 0);
        assert!(idx.is_empty());

        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(idx.len(), 1);
        assert!(!idx.is_empty());
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --workspace`
Expected: All previous 79 tests pass + ~6 new HnswIndex tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-core/src/index.rs
git commit -m "test(index): add unit tests for HnswIndex insert, search, idempotency"
```

---

## Chunk 2: Wire into Executor

### Task 4: Add indexes to Executor and wire INSERT

**Files:**
- Modify: `crates/trondb-core/src/executor.rs:1-28` (imports, struct, constructor)
- Modify: `crates/trondb-core/src/executor.rs:192-211` (INSERT — after Fjall + Location Table writes)

- [ ] **Step 1: Add imports and indexes field**

In `crates/trondb-core/src/executor.rs`, add to imports:
```rust
use crate::index::HnswIndex;
```

Change Executor struct (currently lines 20-24):
```rust
pub struct Executor {
    store: FjallStore,
    wal: WalWriter,
    location: Arc<LocationTable>,
    indexes: DashMap<String, HnswIndex>,
}
```

Add `use dashmap::DashMap;` to imports.

Change constructor (currently lines 26-28):
```rust
    pub fn new(store: FjallStore, wal: WalWriter, location: Arc<LocationTable>) -> Self {
        Self {
            store,
            wal,
            location,
            indexes: DashMap::new(),
        }
    }
```

- [ ] **Step 2: Wire INSERT to populate HNSW index**

After the Location Table writes in the INSERT handler (after the `for` loop ending around line 211, before the `Ok(QueryResult...)`), add:

```rust
                // Apply to HNSW index
                for repr in &entity.representations {
                    let dims = self.store.get_dimensions(&p.collection)?;
                    let index = self.indexes
                        .entry(p.collection.clone())
                        .or_insert_with(|| HnswIndex::new(dims));
                    index.insert(&id, &repr.vector);
                }
```

- [ ] **Step 3: Add indexes accessor**

After the `wal_head_lsn()` method (around line 299), add:

```rust
    pub fn indexes(&self) -> &DashMap<String, HnswIndex> {
        &self.indexes
    }
```

- [ ] **Step 4: Update executor tests**

In the test module at the bottom of executor.rs, add `use dashmap::DashMap;` if not already present. The `setup_executor()` function should still work since the constructor now creates `indexes: DashMap::new()` internally.

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "feat(index): wire HNSW index into executor INSERT path"
```

---

### Task 5: Implement SEARCH execution

**Files:**
- Modify: `crates/trondb-core/src/executor.rs:267-269` (replace UnsupportedOperation)

- [ ] **Step 1: Replace the gated SEARCH handler**

Replace lines 267-269:
```rust
            Plan::Search(_) => Err(EngineError::UnsupportedOperation(
                "SEARCH requires vector index (Phase 4)".into(),
            )),
```

With:
```rust
            Plan::Search(p) => {
                // Validate collection exists
                if !self.store.has_collection(&p.collection) {
                    return Err(EngineError::CollectionNotFound(p.collection.clone()));
                }

                // Validate dimensions
                let dims = self.store.get_dimensions(&p.collection)?;
                if p.query_vector.len() != dims {
                    return Err(EngineError::DimensionMismatch {
                        expected: dims,
                        got: p.query_vector.len(),
                    });
                }

                // Cast query vector f64 → f32
                let query_f32: Vec<f32> = p.query_vector.iter().map(|v| *v as f32).collect();

                // Search HNSW index
                let candidates = if let Some(index) = self.indexes.get(&p.collection) {
                    index.search(&query_f32, p.k)
                } else {
                    Vec::new()
                };

                // Filter by confidence threshold and resolve entities
                let mut rows = Vec::new();
                let mut scanned = 0;
                for (id, similarity) in candidates {
                    if (similarity as f64) < p.confidence_threshold {
                        continue;
                    }
                    scanned += 1;

                    // Resolve entity from Fjall
                    if let Ok(entities) = self.store.scan(&p.collection) {
                        if let Some(entity) = entities.iter().find(|e| e.id == id) {
                            let mut row = entity_to_row(entity, &FieldList::All);
                            row.score = Some(similarity);
                            rows.push(row);
                        }
                    }
                }

                Ok(QueryResult {
                    columns: build_columns(&rows, &FieldList::All),
                    rows,
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: scanned,
                        mode: QueryMode::Probabilistic,
                        tier: "Ram".into(),
                    },
                })
            }
```

**Note**: The entity resolution via `store.scan()` + `find()` is O(n) per result. This is fine for Phase 4 — a direct `store.get()` by ID would be more efficient but would require adding that method to `FjallStore`. If `FjallStore` already has a `get` method, use it. If not, the scan + find approach works and can be optimized later.

- [ ] **Step 2: Update EXPLAIN for SEARCH**

In the `explain_plan()` function (around line 460), change the Search arm:
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

Changes from current: `tier` from `"Fjall"` to `"Ram"`, `strategy` from `"BruteForce"` to `"HNSW"`.

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass. The `search_returns_unsupported` integration test in lib.rs will now FAIL because SEARCH no longer returns an error. Update that test — see Task 7.

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "feat(search): implement SEARCH execution with HNSW index and confidence filtering"
```

---

## Chunk 3: Engine Integration and Index Rebuild

### Task 6: Rebuild HNSW indexes on Engine::open

**Files:**
- Modify: `crates/trondb-core/src/lib.rs:64-67` (after WAL replay, before snapshot task)

- [ ] **Step 1: Add index rebuild after WAL replay**

In `Engine::open`, after the WAL replay block (after `eprintln!("WAL recovery: replayed {replayed} records");`), add:

```rust
        // Rebuild HNSW indexes from Fjall
        for collection_name in executor.collections() {
            if let Ok(entities) = executor.scan_collection(&collection_name) {
                if let Ok(dims) = executor.get_collection_dimensions(&collection_name) {
                    for entity in &entities {
                        if !entity.representations.is_empty() {
                            let index = executor.indexes()
                                .entry(collection_name.clone())
                                .or_insert_with(|| crate::index::HnswIndex::new(dims));
                            for repr in &entity.representations {
                                index.insert(&entity.id, &repr.vector);
                            }
                        }
                    }
                }
            }
        }
```

- [ ] **Step 2: Add store delegation methods to Executor**

The rebuild code needs `scan_collection` and `get_collection_dimensions` on Executor. Add these delegation methods:

```rust
    pub fn scan_collection(&self, name: &str) -> Result<Vec<crate::types::Entity>, EngineError> {
        self.store.scan(name)
    }

    pub fn get_collection_dimensions(&self, name: &str) -> Result<usize, EngineError> {
        self.store.get_dimensions(name)
    }
```

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-core/src/lib.rs crates/trondb-core/src/executor.rs
git commit -m "feat(index): rebuild HNSW indexes from Fjall on engine startup"
```

---

### Task 7: Update existing tests and add SEARCH integration tests

**Files:**
- Modify: `crates/trondb-core/src/lib.rs` (update `search_returns_unsupported`, add new tests)

- [ ] **Step 1: Replace search_returns_unsupported test**

The test at `search_returns_unsupported` currently asserts SEARCH returns an error. Replace it with a test that SEARCH works:

```rust
    #[tokio::test]
    async fn search_returns_results() {
        let (engine, _dir) = test_engine().await;

        engine
            .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'The Shard') VECTOR [1.0, 0.0, 0.0];",
            )
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v2', 'Big Ben') VECTOR [0.0, 1.0, 0.0];",
            )
            .await
            .unwrap();

        let result = engine
            .execute_tql("SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] LIMIT 2;")
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 2);
        // First result should be v1 (exact match)
        assert_eq!(
            result.rows[0].values.get("id"),
            Some(&Value::String("v1".into()))
        );
        // Should have a score
        assert!(result.rows[0].score.is_some());
        assert!(result.rows[0].score.unwrap() > 0.9);
    }
```

- [ ] **Step 2: Add confidence filtering test**

```rust
    #[tokio::test]
    async fn search_confidence_filtering() {
        let (engine, _dir) = test_engine().await;

        engine
            .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'Close') VECTOR [0.9, 0.1, 0.0];",
            )
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v2', 'Far') VECTOR [0.0, 0.0, 1.0];",
            )
            .await
            .unwrap();

        // High confidence threshold should filter out the distant vector
        let result = engine
            .execute_tql("SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] CONFIDENCE > 0.5 LIMIT 10;")
            .await
            .unwrap();

        // Only the close vector should pass the threshold
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("id"),
            Some(&Value::String("v1".into()))
        );
    }
```

- [ ] **Step 3: Add empty search test**

```rust
    #[tokio::test]
    async fn search_empty_collection_returns_empty() {
        let (engine, _dir) = test_engine().await;

        engine
            .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
            .await
            .unwrap();

        let result = engine
            .execute_tql("SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] LIMIT 5;")
            .await
            .unwrap();

        assert!(result.rows.is_empty());
    }
```

- [ ] **Step 4: Add EXPLAIN SEARCH test**

```rust
    #[tokio::test]
    async fn explain_search_shows_hnsw() {
        let (engine, _dir) = test_engine().await;

        let result = engine
            .execute_tql("EXPLAIN SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] LIMIT 5;")
            .await
            .unwrap();

        let strategy_row = result
            .rows
            .iter()
            .find(|r| r.values.get("property") == Some(&Value::String("strategy".into())))
            .expect("should have 'strategy' property");

        assert_eq!(
            strategy_row.values.get("value"),
            Some(&Value::String("HNSW".into()))
        );

        let mode_row = result
            .rows
            .iter()
            .find(|r| r.values.get("property") == Some(&Value::String("mode".into())))
            .expect("should have 'mode' property");

        assert_eq!(
            mode_row.values.get("value"),
            Some(&Value::String("Probabilistic".into()))
        );
    }
```

- [ ] **Step 5: Add restart rebuild test**

```rust
    #[tokio::test]
    async fn search_works_after_restart() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("data"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
        };

        // Insert data
        {
            let engine = Engine::open(config.clone()).await.unwrap();
            engine
                .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
                .await
                .unwrap();
            engine
                .execute_tql(
                    "INSERT INTO venues (id, name) VALUES ('v1', 'Test') VECTOR [1.0, 0.0, 0.0];",
                )
                .await
                .unwrap();
        }

        // Reopen — HNSW index should be rebuilt from Fjall
        {
            let engine = Engine::open(config).await.unwrap();
            let result = engine
                .execute_tql("SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] LIMIT 1;")
                .await
                .unwrap();

            assert_eq!(result.rows.len(), 1);
            assert_eq!(
                result.rows[0].values.get("id"),
                Some(&Value::String("v1".into()))
            );
        }
    }
```

- [ ] **Step 6: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass including the new SEARCH tests.

- [ ] **Step 7: Commit**

```bash
git add crates/trondb-core/src/lib.rs
git commit -m "test(search): add integration tests for SEARCH, confidence filtering, EXPLAIN, restart"
```

---

## Chunk 4: Cleanup and CLAUDE.md

### Task 8: Remove dead_code allow on SearchPlan and update CLAUDE.md

**Files:**
- Modify: `crates/trondb-core/src/planner.rs:40` (remove `#[allow(dead_code)]`)
- Modify: `CLAUDE.md`

- [ ] **Step 1: Remove dead_code allow**

In `crates/trondb-core/src/planner.rs`, remove line 40:
```rust
#[allow(dead_code)]
```

SearchPlan is now used by the SEARCH executor.

- [ ] **Step 2: Update CLAUDE.md**

Replace CLAUDE.md contents with:

```markdown
# TronDB

Inference-first storage engine. Phase 4: HNSW vector index.

## Project Structure

- `crates/trondb-wal/` — Write-Ahead Log: record types (MessagePack), segment files, buffer, async writer, crash recovery
- `crates/trondb-core/` — Engine: types, Fjall-backed store, Location Table (DashMap), HNSW index (hnsw_rs), planner, async executor. Depends on trondb-wal + trondb-tql.
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
- Write path: WAL append → flush+fsync → apply to Fjall + Location Table + HNSW index → ack
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
```

- [ ] **Step 3: Run tests and clippy**

Run: `cargo test --workspace && cargo clippy --workspace`
Expected: All tests pass, clippy clean.

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-core/src/planner.rs CLAUDE.md
git commit -m "docs: update CLAUDE.md for Phase 4, remove dead_code allow on SearchPlan"
```
