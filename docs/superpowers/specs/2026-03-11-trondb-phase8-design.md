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
3. Apply assignments to `entity.metadata` map
4. WAL append `RecordType::EntityWrite` with the updated entity (reuses existing record type — no new WAL format)
5. WAL commit
6. Write updated entity to Fjall
7. Update affected field indexes: remove old values, insert new values
8. Return `QueryResult` with "updated" status

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
| EntityDelete | 0x11 | Placeholder (empty arm) | Deserialise entity_id, call `store.delete_entity()`. HNSW/field index cleanup unnecessary — indexes rebuilt from Fjall after replay. |

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

**Category D — No action needed:**

| Record Type | Code | Reason |
|---|---|---|
| TxBegin/Commit/Abort | 0x01-0x03 | Recovery already filters to committed records |
| ReprWrite/ReprDirty | 0x20-0x21 | No separate representation write path exists |
| Checkpoint | 0xFF | Control record, not a data mutation |

### Wiring: Engine returns unhandled records

`replay_wal_records` returns `(usize, Vec<WalRecord>)` — replayed count + unhandled records. `Engine::open()` returns `(Engine, Vec<WalRecord>)` instead of just `Engine`.

The caller (routing layer) **must** process the unhandled records before constructing the queryable system. This enforces correct startup sequencing at the type level — no window of inconsistent state, no "remember to call" API.

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

---

## 3. HNSW Persistence

### Problem

Every startup does a full Fjall scan per collection (lib.rs:82-154), deserialising every entity and re-inserting every dense vector into a fresh HNSW graph. This is O(n log n) in graph construction and gets slower as the database grows.

### Approach

Snapshot HNSW indexes to disk. On startup, load from snapshot and apply incremental WAL catch-up. Fall back to full rebuild if snapshot is missing or corrupt.

### Snapshot Format

Directory: `{data_dir}/hnsw_snapshots/`

Per HNSW index (one per collection+representation):
- `{collection}_{repr_name}.hnsw.graph` — graph structure (neighbor lists, layers)
- `{collection}_{repr_name}.hnsw.data` — vector data
- `{collection}_{repr_name}.hnsw.meta` — JSON sidecar:

```json
{
  "entity_count": 42000,
  "lsn": 158923,
  "checksum": "sha256:..."
}
```

Uses `hnsw_rs` built-in `dump`/`load` via `hnswio` module.

### Startup: Snapshot Load + Incremental Catch-Up

For each collection + dense representation:

1. **Try load snapshot:** Read `.meta`, verify checksum, load `.graph` + `.data`
2. **Incremental catch-up:** Scan WAL records where `lsn > meta.lsn`:
   - `EntityWrite` in this collection: deserialise, insert vector into loaded graph
   - `EntityDelete` in this collection: tombstone the HNSW slot
3. **Fallback:** If snapshot missing, corrupt, or load fails — log warning, full Fjall scan rebuild (current behavior)

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
