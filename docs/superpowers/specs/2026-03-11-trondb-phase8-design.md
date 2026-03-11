# TronDB Phase 8: UPDATE, WAL Replay, HNSW Persistence

**Goal:** Close the three most impactful practical gaps — metadata mutation, crash recovery completeness, and startup performance.

**Scope:** UPDATE statement, WAL replay for all actively-written record types, HNSW index snapshotting.

**Non-scope:** Multi-node distribution (Phase 9), temporal queries / edge confidence updates / bidirectional TRAVERSE (Phase 10).

---

## 1. UPDATE Statement

### Syntax

```sql
UPDATE 'entity_id' IN collection SET field = value, field2 = value2;
```

- Metadata-only updates. Vector changes use re-INSERT (upsert).
- Values: string literals (single-quoted), integers, floats, booleans (`true`/`false`).
- Multiple assignments in one statement, comma-separated.

### AST

```rust
UpdateStmt {
    entity_id: String,
    collection: String,
    assignments: Vec<(String, Literal)>,
}
```

New `Statement::Update` variant wrapping `UpdateStmt`.

### Planner

```rust
UpdateEntityPlan {
    entity_id: String,
    collection: String,
    assignments: Vec<(String, Literal)>,
}
```

New `Plan::UpdateEntity` variant. No strategy selection needed — always a point lookup + write-back.

### Executor

1. Read entity from Fjall by `(collection, entity_id)`
2. Validate entity exists (error if not)
3. **Capture old field values** for all indexed fields (needed for step 8's remove operation — must happen before mutation)
4. Apply assignments to `entity.metadata` map
5. WAL append `RecordType::EntityWrite` with the updated entity (reuses existing record type — no new WAL format)
6. WAL commit — crash between here and step 7 is safe because replay of `EntityWrite` re-applies the full entity blob
7. Write updated entity to Fjall
8. Update affected field indexes: remove old entries using captured pre-mutation values, insert new entries using post-mutation values
9. Return `QueryResult` with "updated" status

### NULL handling

`SET field = NULL` is out of scope for Phase 8. Only concrete values (string, int, float, bool) are supported. Removing a metadata key requires DELETE + re-INSERT.

### Why EntityWrite, not a new record type

The WAL payload is the full entity blob. Whether it was created by INSERT or modified by UPDATE, the replay logic is identical: deserialise and overwrite in Fjall. A new record type would add complexity with no benefit.

---

## 2. WAL Replay Completeness

### Current State

`replay_wal_records` handles 6 of 17 record types. The rest silently no-op via `_ => {}`.

### Record Type Categories

**Category A — Core executor (Phase 8):**

| Record Type | Code | Current | Fix |
|---|---|---|---|
| EntityDelete | 0x11 | Placeholder (empty arm) | Deserialise entity_id, call `store.delete_entity()`. Also call `delete_from_tier(NVMe)` and `delete_from_tier(Archive)` to clean up warm/archive tier data. HNSW/field index cleanup unnecessary — indexes rebuilt from Fjall after replay. |

**Category B — Routing layer (Phase 8):**

| Record Type | Code | Current | Fix |
|---|---|---|---|
| AffinityGroupCreate | 0x60 | Skipped | Route to `AffinityIndex::replay_affinity_record()` |
| AffinityGroupMember | 0x61 | Skipped | Route to `AffinityIndex::replay_affinity_record()` |
| AffinityGroupRemove | 0x62 | Skipped | Route to `AffinityIndex::replay_affinity_record()` |
| TierMigration | 0x70 | Skipped | State-inspection recovery (see below) |

**Category C — Phase 10 (defer):**

| Record Type | Code | Reason |
|---|---|---|
| EdgeInferred | 0x31 | Not yet written anywhere |
| EdgeConfidenceUpdate | 0x32 | Not yet written anywhere |
| EdgeConfirm | 0x33 | Not yet written anywhere |

**Category D — Already handled or no action needed:**

| Record Type | Code | Reason |
|---|---|---|
| TxBegin/Commit/Abort | 0x01-0x03 | Recovery already filters to committed records |
| EntityWrite | 0x10 | Already handled in replay |
| ReprWrite/ReprDirty | 0x20-0x21 | No separate representation write path exists |
| EdgeWrite | 0x30 | Already handled in replay |
| EdgeDelete | 0x34 | Already handled in replay |
| LocationUpdate | 0x40 | Already handled in replay |
| SchemaCreateColl | 0x50 | Already handled in replay |
| SchemaCreateEdgeType | 0x51 | Already handled in replay |
| Checkpoint | 0xFF | Control record, not a data mutation |

### Wiring: Engine returns unhandled records

`replay_wal_records` returns `(usize, Vec<WalRecord>)` — replayed count + unhandled records. `Engine::open()` returns `(Engine, Vec<WalRecord>)` instead of just `Engine`.

The caller (routing layer) **must** process the unhandled records before constructing the queryable system. This enforces correct startup sequencing at the type level — no window of inconsistent state, no "remember to call" API.

**Call site wiring:** `SemanticRouter::new()` in `trondb-routing` currently calls `Engine::open(config).await?`. This becomes:

```rust
let (engine, pending_records) = Engine::open(config).await?;
// Process routing-layer WAL records before exposing engine
let affinity = AffinityIndex::new();
let mut tier_recovery = Vec::new();
for record in &pending_records {
    match record.record_type {
        RecordType::AffinityGroupCreate
        | RecordType::AffinityGroupMember
        | RecordType::AffinityGroupRemove => {
            affinity.replay_affinity_record(record.record_type, &record.payload, record.lsn);
        }
        RecordType::TierMigration => tier_recovery.push(record),
        _ => {} // Unknown future records — safe to skip
    }
}
// Process tier recovery (state-inspection, see below)
// ... then construct SemanticRouter with engine + affinity
```

`trondb-cli` also calls `Engine::open()` directly (without routing). This call site simply discards the unhandled records: `let (engine, _pending) = Engine::open(config).await?;` — acceptable because the CLI doesn't use routing features.

### Startup Sequence (revised)

1. Load location table snapshot
2. Core WAL replay (EntityWrite, EntityDelete, SchemaCreate*, EdgeWrite, EdgeDelete, LocationUpdate)
3. HNSW snapshot load + incremental catch-up (see Section 3)
4. Rebuild SparseIndex + FieldIndex from Fjall
5. Return `(Engine, unhandled_wal_records)` to caller
6. Routing layer processes AffinityGroup* + TierMigration records
7. System becomes queryable

### TierMigration Replay: State-Inspection Recovery

TierMigration is written as intent-only (no completion record). Replay cannot distinguish "completed" from "crashed mid-migration" without inspecting actual tier state.

**Recovery strategy — check source tier:**

| Source exists? | Target exists? | Action |
|---|---|---|
| Yes | No | Re-execute migration from scratch |
| Yes | Yes | Partial completion — delete source, update location |
| No | Yes | Already completed — update location table to match |
| No | No | Entity subsequently deleted — no action |

This is safe because each migration step is individually idempotent:
- Write to target tier: overwrite is safe
- Remove from HNSW: no-op if not present
- Update location: overwrite is safe
- Delete from source: no-op if already gone

**Constraint:** TierMigration currently operates on `repr_index: 0` only (hardcoded in migrator.rs:133-135). The `TierMigrationPayload` does not contain `repr_index`. Recovery logic matches this constraint — check tier state for `repr_index: 0` only. Multi-representation tier migration is deferred to a future phase.

---

## 3. HNSW Persistence

### Problem

Every startup does a full Fjall scan per collection (lib.rs:82-154), deserialising every entity and re-inserting every dense vector into a fresh HNSW graph. This is O(n log n) in graph construction and gets slower as the database grows.

### Approach

Snapshot HNSW indexes to disk. On startup, load from snapshot and apply incremental WAL catch-up. Fall back to full rebuild if snapshot is missing or corrupt.

### Snapshot Format

Directory: `{data_dir}/hnsw_snapshots/`

Per HNSW index (one per collection+representation):
- `{collection}_{repr_name}.hnsw.graph` — graph structure (neighbor lists, layers) via `hnsw_rs` `hnswio` dump/load
- `{collection}_{repr_name}.hnsw.data` — vector data via `hnsw_rs` `hnswio` dump/load
- `{collection}_{repr_name}.hnsw.idmap` — TronDB's ID mapping state (bincode-serialised):
  - `id_to_idx: HashMap<String, usize>` (LogicalId → HNSW internal index)
  - `idx_to_id: HashMap<usize, String>` (reverse mapping)
  - `next_idx: usize` (next available internal index)
  - `tombstones: HashSet<String>` (deleted-but-not-yet-compacted entries)
- `{collection}_{repr_name}.hnsw.meta` — JSON sidecar:

```json
{
  "version": 1,
  "entity_count": 42000,
  "lsn": 158923,
  "checksum": "sha256:...",
  "max_nb_connection": 16,
  "max_elements": 100000,
  "ef_construction": 200
}
```

The `.idmap` file is critical — `hnsw_rs` dump/load only persists the graph structure and vector data, not TronDB's `HnswIndex` wrapper state (`id_to_idx`, `idx_to_id`, `next_idx`, `tombstones`). Without the ID maps, search results would return internal indices that cannot be resolved to `LogicalId`s.

The `.meta` version field and HNSW parameter fingerprint (`max_nb_connection`, `max_elements`, `ef_construction`) enable safe rejection of snapshots created with incompatible parameters — triggering fallback rebuild rather than silent corruption.

### Startup: Snapshot Load + Incremental Catch-Up

For each collection + dense representation:

1. **Try load snapshot:** Read `.meta`, verify checksum and parameter compatibility, load `.graph` + `.data` + `.idmap`
2. **Incremental catch-up:** Scan WAL records where `lsn > meta.lsn`:
   - `EntityWrite` in this collection: deserialise, insert vector into loaded graph + update ID maps
   - `EntityDelete` in this collection: tombstone the HNSW slot + update ID maps
3. **Fallback:** If snapshot missing, corrupt, parameter mismatch, or load fails — log warning, full Fjall scan rebuild (current behavior)

**Atomicity:** Snapshot write captures the current WAL LSN, then serialises the graph and ID maps. The snapshot is written to temporary files first, then atomically renamed. This ensures a crash during snapshot write produces either the old snapshot or the new one, never a partial write. The LSN captured at the start means incremental catch-up may re-insert a few entities that are already in the snapshot — this is safe because `HnswIndex::insert()` checks `id_to_idx.contains_key(id)` and skips duplicates.

This avoids the full Fjall scan when the snapshot is recent (the common case).

### Snapshot Triggers

- **Clean shutdown:** `Engine::close()` or Drop impl
- **Periodic background task:** Configurable interval, default 5 minutes
- **Bulk insert threshold:** After 1000+ entity insertions since last snapshot

### What's NOT Persisted (Phase 8)

- **SparseIndex:** Simple DashMap, rebuilds quickly from Fjall
- **FieldIndex:** Fjall-backed already (partition scan), fast rebuild
- **AdjacencyIndex:** Rebuilt from Fjall edge partitions, fast rebuild

HNSW is the only expensive reconstruction — graph building is O(n log n) vs O(n) for the others.

### Failure Mode

Snapshot loading failure (corruption, version mismatch, missing files) is non-fatal. Log a warning, delete the bad snapshot, and fall back to full rebuild. Never block startup on a bad snapshot.

---

## Architecture Summary

```
Startup Sequence (Phase 8):

  Location Table Snapshot ──► Core WAL Replay ──► HNSW Snapshot Load
        (existing)             (+ EntityDelete)    (+ incremental catch-up)
                                     │                      │
                                     ▼                      ▼
                              SparseIndex +            Fallback: full
                              FieldIndex rebuild       Fjall scan
                                     │
                                     ▼
                              Return (Engine, unhandled_records)
                                     │
                                     ▼
                              Routing Layer Init
                              (AffinityGroup* + TierMigration replay)
                                     │
                                     ▼
                              System queryable
```

**Key invariant:** The system is never queryable until all WAL records have been processed — both core and routing layer records. The `(Engine, Vec<WalRecord>)` return type enforces this at compile time.
