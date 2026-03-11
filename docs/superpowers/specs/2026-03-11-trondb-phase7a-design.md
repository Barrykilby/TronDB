# TronDB Phase 7a: Tiered Storage + Vector Quantisation

## Goal

Add warm (NVMe) and cold (Archive) storage tiers with Int8 and Binary vector quantisation, a background migration pipeline, and automatic promotion/demotion driven by access patterns and capacity limits.

## Architecture

TronDB's two-plane design — Control Fabric (RAM) vs Data Fabric (Fjall) — already has the infrastructure for tiered storage. The Location Table tracks `Tier` (Ram/Fjall/NVMe/Archive), `Encoding` (Float32/Int8/Binary/Sparse), and `LocState` (including `Migrating`). Phase 7a activates this dormant infrastructure.

All tiers use Fjall as the storage backend. Tiers are logical, distinguished by partition naming and vector encoding. No new storage dependencies.

### Tier Naming Convention

The existing `Tier` enum uses physical names (Fjall/NVMe/Archive). Phase 7a maps these to user-facing tier names:

| User-facing | Enum variant | Partition prefix | Encoding |
|-------------|-------------|-----------------|----------|
| Hot | `Tier::Fjall` | `{collection}` (existing, no prefix) | Float32 |
| Warm | `Tier::NVMe` | `warm.{collection}` | Int8 |
| Archive | `Tier::Archive` | `archive.{collection}` | Binary |

Throughout this spec, "Hot" means `Tier::Fjall`, "Warm" means `Tier::NVMe`, "Archive" means `Tier::Archive`. The enum variants are not renamed to avoid breaking existing code.

### Existing Infrastructure (Built in Phases 3-6)

| Component | Status | Location |
|-----------|--------|----------|
| `Tier` enum (Ram/Fjall/NVMe/Archive) | Defined, only Fjall used | `location.rs` |
| `Encoding` enum (Float32/Int8/Binary/Sparse) | Defined, Float32+Sparse used | `location.rs` |
| `LocState::Migrating` + transitions | Defined, untested in production paths | `location.rs` |
| `LocationTable::update_tier()` | Implemented, unit-tested | `location.rs` |
| `LruTracker` (access timestamps) | RAM-only (`HashMap<EntityId, Instant>`), per-entity | `trondb-routing/src/eviction.rs` |
| `select_eviction_candidates()` | Returns candidates, no demotion action | `trondb-routing/src/eviction.rs` |
| WAL logging + snapshot for Location Table | Active | `executor.rs`, `location.rs` |

### New Components

| Component | Responsibility | Location |
|-----------|---------------|----------|
| `quantise` module | Int8/Binary transcoding | `trondb-core/src/quantise.rs` |
| `TierConfig` | Capacity limits, timing thresholds | `trondb-routing/src/config.rs` |
| `TierMigrator` | Background demotion/promotion pipeline | `trondb-routing/src/migrator.rs` |
| Tiered store methods | Per-tier partition read/write/delete | `trondb-core/src/store.rs` |
| Tiered executor read path | Tier-aware FETCH dispatch | `trondb-core/src/executor.rs` |
| TQL DEMOTE/PROMOTE/EXPLAIN TIERS | Manual tier control | `trondb-tql/src/` |
| WAL `TierMigration` record | Migration durability | `trondb-wal/src/record.rs` |

---

## 1. Vector Quantisation

### 1.1 Int8 Scalar Quantisation

Map each Float32 dimension to a u8 (0-255) using per-vector min/max linear scaling.

**Storage format:**
```
[min: f32 LE][max: f32 LE][dims: u8...]
```
- Overhead: 8 bytes (min + max scalars)
- Per dimension: 1 byte (vs 4 bytes for Float32)
- Size reduction: ~75%

**Quantisation:**
```
scale = 255.0 / (max - min)
quantised[i] = round((float[i] - min) * scale) as u8
```

**Dequantisation (reconstruction):**
```
float[i] = (quantised[i] as f32 / 255.0) * (max - min) + min
```

Edge case: if `max == min` (constant vector), store `min` and all zeros. Dequantise returns the constant.

### 1.2 Binary Quantisation

Map each Float32 dimension to a single bit based on sign.

**Storage format:**
```
[packed_bits: u8...]
```
- Size: `ceil(dims / 8)` bytes
- Size reduction: ~97%

**Quantisation:**
```
bit[i] = if float[i] >= 0.0 { 1 } else { 0 }
```

Packed MSB-first into bytes.

**No dequantisation.** Binary encoding is too lossy for meaningful reconstruction. Binary-tier entities can only be served via direct FETCH (returning raw binary representation) or promoted to warm/hot before serving.

### 1.3 Transcoding API

New module: `crates/trondb-core/src/quantise.rs`

```rust
pub struct QuantisedInt8 {
    pub min: f32,
    pub max: f32,
    pub data: Vec<u8>,
}

pub struct QuantisedBinary {
    pub data: Vec<u8>,
    pub dimensions: usize,
}

pub fn quantise_int8(vector: &[f32]) -> QuantisedInt8;
pub fn quantise_binary(vector: &[f32]) -> QuantisedBinary;
pub fn dequantise_int8(q: &QuantisedInt8) -> Vec<f32>;

// Serialisation for Fjall storage
impl QuantisedInt8 {
    pub fn to_bytes(&self) -> Vec<u8>;
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, EngineError>;
}

impl QuantisedBinary {
    pub fn to_bytes(&self) -> Vec<u8>;
    pub fn from_bytes(bytes: &[u8], dimensions: usize) -> Result<Self, EngineError>;
}
```

### 1.4 Search on Quantised Vectors

- **Hot tier (Float32):** HNSW search as today. Only hot-tier vectors are in the HNSW index.
- **Warm tier (Int8):** Not in HNSW. Entities are FETCHable by ID. On access, auto-promoted to hot (dequantised, re-inserted into HNSW).
- **Archive tier (Binary):** Not in HNSW. FETCHable by ID (returns binary representation metadata). On access, promoted to warm.
- **SEARCH only returns hot-tier results.** Warm/archive entities are invisible to SEARCH until promoted.

---

## 2. Tiered Storage Architecture

### 2.1 Partition Scheme

One Fjall partition per tier per collection:

| Tier | Partition Name | Encoding | HNSW Indexed |
|------|---------------|----------|--------------|
| Hot | `{collection}` (existing) | Float32 | Yes |
| Warm | `warm.{collection}` | Int8 | No |
| Archive | `archive.{collection}` | Binary | No |

Note: The hot tier uses `{collection}` directly as the partition name (e.g. `venues`), matching the existing convention. There is no `data.` prefix.

Partitions are created lazily on first write to a tier for a given collection.

### 2.2 Read Path

```
FETCH entity_id FROM collection
  → LocationTable.get(entity_id)
  → match tier:
      Hot    → read from {collection} partition, return Float32
      Warm   → read from warm.{collection}, dequantise Int8 → Float32, return
               if promote_on_access: async promote to hot
      Archive → read from archive.{collection}, return metadata + binary repr
               if promote_on_access: async promote to warm
```

FETCH always returns data regardless of tier. The response includes a `tier` field so the caller knows where the data came from.

### 2.3 Write Path

Unchanged. All INSERTs write to hot tier (`data.{collection}`) with Float32 encoding. Demotion to warm/archive happens asynchronously via the migration pipeline.

### 2.4 Configuration

```rust
pub struct TierConfig {
    /// Max entities in hot tier per collection (default: 100_000)
    pub hot_capacity: usize,
    /// Max entities in warm tier per collection (default: 1_000_000)
    pub warm_capacity: usize,
    /// Seconds idle before hot → warm demotion (default: 86400 = 24h)
    pub demote_after_secs: u64,
    /// Seconds idle in warm before warm → archive (default: 604800 = 7d)
    pub archive_after_secs: u64,
    /// Auto-promote warm → hot on FETCH (default: true)
    pub promote_on_access: bool,
    /// Max entities to migrate per cycle (default: 100)
    pub migration_batch_size: usize,
    /// Migration cycle interval in ms (default: 60_000)
    pub migration_interval_ms: u64,
}
```

`TierConfig` is a standalone config struct (like `HealthConfig`, `ColocationConfig`). It lives alongside the other configs in `trondb-routing/src/config.rs`. The `SemanticRouter` holds it and passes it to the `TierMigrator` on construction.

---

## 3. Migration Pipeline

### 3.1 TierMigrator

New component in `trondb-routing`: a tokio background task that runs demotion cycles on a timer.

```rust
pub struct TierMigrator {
    config: TierConfig,
    engine: Arc<Engine>,
    lru_tracker: Arc<LruTracker>,
    affinity: Arc<AffinityIndex>,
    health_cache: Arc<HealthCache>,
}
```

`TierMigrator` accesses Fjall and HNSW through new public methods on `Engine`:
- `Engine::read_tiered(collection, entity_id, tier)` — delegates to `store.read_tiered()`
- `Engine::write_tiered(collection, entity_id, tier, data)` — delegates to `store.write_tiered()`
- `Engine::delete_from_tier(collection, entity_id, tier)` — delegates to `store.delete_from_tier()`
- `Engine::remove_from_hnsw(collection, entity_id)` — delegates to `indexes.get(collection).remove(id)`
- `Engine::insert_into_hnsw(collection, entity_id, vector)` — delegates to `indexes.get(collection).insert(id, vector)`
- `Engine::location_table()` — returns `&LocationTable` (already partially exposed)

### 3.2 Demotion Cycle (scheduled, every `migration_interval_ms`)

1. Scan LRU tracker for hot-tier entities with `last_accessed` older than `demote_after_secs`
2. Sort candidates by eviction priority (Phase 6 ordering: ungrouped LRU first, explicit groups last)
3. Take up to `migration_batch_size` candidates
4. For each candidate (where `key` is a `ReprKey { entity_id, repr_index }`):
   a. `LocationTable::transition(&key, LocState::Migrating)`
   b. WAL log intent: `TierMigration { entity_id, collection, from: Fjall, to: NVMe, encoding: Int8 }`
   c. Read Float32 vector from hot partition
   d. `quantise_int8(vector)` → write Int8 bytes to warm partition
   e. Remove entity from HNSW index (tombstone)
   f. `LocationTable::update_tier(&key, Tier::NVMe, Encoding::Int8)`
   g. `LocationTable::transition(&key, LocState::Clean)`
   h. Delete from hot partition

   WAL is logged at step (b) before data movement, recording intent. On crash between (b) and (h), recovery detects the intent record and checks both partitions to resolve.
5. Same pattern for warm → archive using `archive_after_secs` and `quantise_binary`

### 3.3 Promotion (triggered on access)

When the executor reads a non-hot entity:

**Warm → Hot:**
1. Read Int8 from warm partition
2. Dequantise to Float32
3. Write Float32 to hot partition
4. Insert into HNSW index
5. WAL log: `TierMigration { entity_id, from: Warm, to: Hot, encoding: Float32 }`
6. `LocationTable::update_tier(key, Tier::Fjall, Encoding::Float32)`
7. Delete from warm partition
8. If hot tier over capacity → trigger demotion of 1 LRU ungrouped entity

**Archive tier:** Binary quantisation is destructive — no meaningful promotion is possible without the original Float32 data. FETCH from archive returns the entity's scalar fields and a `tier: "Archive"` marker but no vector data. To restore an archived entity to hot, the user must re-INSERT with the original vector.

### 3.4 Crash Recovery

WAL `TierMigration` records log intent (step b in demotion), not completion. On startup:

1. WAL replay processes `TierMigration` records
2. For each record, check Location Table state:
   - `LocState::Migrating` → migration was in progress
     - Check if entity exists in target tier partition
     - If yes: migration completed data copy but didn't finish — complete it (update Location Table, delete from source)
     - If no: migration didn't reach data copy — rollback (set `LocState::Clean` on source)
   - `LocState::Clean` with target tier → migration completed fully, no action needed
   - `LocState::Clean` with source tier → migration was rolled back, no action needed
3. After replay, scan Location Table for any remaining `Migrating` entries not covered by WAL records — set to `Clean` (orphaned state)
4. Clean up orphaned data: scan warm/archive partitions for entity IDs that have no Location Table entry pointing to that tier — delete them

This handles all crash points: before data copy, after data copy but before Location Table update, and after Location Table update but before source deletion.

### 3.5 LRU Tracker Persistence

Phase 6's `LruTracker` is RAM-only (`HashMap<EntityId, Instant>`). Phase 7a needs durable access timestamps for demotion decisions that survive restarts.

**Approach:** Store `last_accessed` as a field on `LocationDescriptor`. The Location Table is already WAL-logged and snapshotted. Add:
```rust
pub struct LocationDescriptor {
    // ... existing fields ...
    #[serde(default)]  // backward-compatible: old snapshots deserialize with 0
    pub last_accessed: u64,  // Unix epoch seconds
}
```

Update `last_accessed` on every FETCH/SEARCH hit. This piggybacks on existing Location Table persistence — no new WAL record types needed for access tracking.

**Snapshot compatibility:** The `#[serde(default)]` attribute ensures old `TRONLOC1` snapshots (without `last_accessed`) deserialize successfully with a default value of 0. Entities with `last_accessed = 0` are treated as "never accessed" and are first in line for demotion. No snapshot format version bump is needed.

**LruTracker migration:** Phase 6's `LruTracker` (RAM-only `HashMap<EntityId, Instant>`) continues to serve as the in-process fast cache. On FETCH/SEARCH, both the `LruTracker` and `LocationDescriptor.last_accessed` are updated. The `TierMigrator` reads `last_accessed` from the Location Table for demotion decisions (durable across restarts), while the router uses `LruTracker` for eviction priority (fast, in-process).

---

## 4. Store + Executor Changes

### 4.1 FjallStore Extensions

```rust
impl FjallStore {
    /// Open or create a tier-specific partition for a collection
    pub fn open_tier_partition(&self, collection: &str, tier: Tier) -> Result<PartitionHandle, EngineError>;

    /// Read entity data from a specific tier's partition
    pub fn read_tiered(&self, collection: &str, entity_id: &LogicalId, tier: Tier) -> Result<Option<Vec<u8>>, EngineError>;

    /// Write entity data to a specific tier's partition
    pub fn write_tiered(&self, collection: &str, entity_id: &LogicalId, tier: Tier, data: &[u8]) -> Result<(), EngineError>;

    /// Delete entity from a specific tier's partition
    pub fn delete_from_tier(&self, collection: &str, entity_id: &LogicalId, tier: Tier) -> Result<(), EngineError>;
}
```

Partition names (see Tier Naming Convention table in Architecture section):
- Hot: `{collection}` (existing, no prefix)
- Warm: `warm.{collection}`
- Archive: `archive.{collection}`

New method for tier stats:
```rust
pub fn tier_entity_count(&self, collection: &str, tier: Tier) -> Result<usize, EngineError>;
```

### 4.2 Executor Read Path Changes

**This is a fundamental change to the FETCH path.** Currently, FETCH reads directly from Fjall via `self.store.get()` / `self.store.scan()` and never consults the Location Table. Phase 7a replaces this with a Location Table lookup → tier-dispatched read. New flow:

```rust
// In executor FETCH handling:
let location = self.location_table.get_entity(&entity_id);
for (repr_idx, descriptor) in location.representations.iter() {
    match descriptor.tier {
        Tier::Fjall => {
            // Hot path — read Float32 as today
            let data = self.store.read(&collection, &entity_id)?;
            // ... existing logic ...
        }
        Tier::NVMe => {
            // Warm path — read Int8, dequantise
            let bytes = self.store.read_tiered(&collection, &entity_id, Tier::NVMe)?;
            let quantised = QuantisedInt8::from_bytes(&bytes)?;
            let vector = dequantise_int8(&quantised);
            // ... build response with tier: "Warm" ...
            if self.tier_config.promote_on_access {
                // Spawn async promotion task
            }
        }
        Tier::Archive => {
            // Archive path — return metadata only
            // ... build response with tier: "Archive", no vector data ...
        }
        Tier::Ram => {
            // Same as Fjall for now
        }
    }
}
```

### 4.3 HNSW Index

No changes to HNSW itself. Only hot-tier vectors are indexed. On demotion, the entity is removed from HNSW. On promotion to hot, the entity is re-inserted into HNSW.

HNSW removal requires adding a method to `HnswIndex`:
```rust
impl HnswIndex {
    pub fn remove(&self, id: &LogicalId);
}
```

Note: `hnsw_rs` doesn't support removal natively. Options:
- **A) Tombstone approach:** Mark the ID as removed in `id_to_idx`/`idx_to_id`, filter results. The HNSW graph retains the point but it's excluded from results. Wastes some memory but avoids rebuild.
- **B) Periodic rebuild:** After N removals, rebuild the entire HNSW index from hot-tier data.

**Decision: Tombstone + periodic rebuild.** Tombstone for immediate removal, rebuild when tombstone count exceeds 10% of index size. Rebuild runs in the migration cycle.

Rebuild strategy: build new HNSW index in the background from hot-tier data, then swap under a brief lock. This doubles RAM usage temporarily during rebuild. The `HnswIndex` struct tracks `tombstone_count: AtomicUsize` and exposes `needs_rebuild() -> bool`.

---

## 5. TQL Extensions

### 5.1 New Tokens

```
DEMOTE    — priority 10, case-insensitive
PROMOTE   — priority 10, case-insensitive
TIERS     — priority 10, case-insensitive
WARM      — priority 10, case-insensitive
ARCHIVE   — priority 10, case-insensitive
```

`TO`, `FROM`, `EXPLAIN` already exist.

### 5.2 New Statements

```sql
-- Force-demote an entity to warm tier
DEMOTE 'entity_id' FROM collection TO WARM;

-- Force-demote to archive tier
DEMOTE 'entity_id' FROM collection TO ARCHIVE;

-- Force-promote back to hot (only from warm — archive requires re-INSERT)
PROMOTE 'entity_id' FROM collection;

-- View tier distribution for a collection
EXPLAIN TIERS collection;
```

Note: `EXPLAIN TIERS` does not follow the current recursive `EXPLAIN <statement>` parser pattern. The parser must special-case: after consuming `EXPLAIN`, check if the next token is `TIERS` and parse `ExplainTiersStmt` directly rather than recursing into statement dispatch.

### 5.3 AST

```rust
pub struct DemoteStmt {
    pub entity_id: String,
    pub collection: String,
    pub target_tier: TierTarget,  // Warm or Archive
}

pub struct PromoteStmt {
    pub entity_id: String,
    pub collection: String,
}

pub struct ExplainTiersStmt {
    pub collection: String,
}

pub enum TierTarget {
    Warm,
    Archive,
}
```

### 5.4 Plan Variants

```rust
pub enum Plan {
    // ... existing variants ...
    Demote(DemotePlan),
    Promote(PromotePlan),
    ExplainTiers(ExplainTiersPlan),
}
```

---

## 6. WAL Integration

### 6.1 New Record Type

```rust
pub enum RecordType {
    // ... existing ...
    TierMigration = 0x70,
}
```

### 6.2 Payload

```rust
#[derive(Serialize, Deserialize)]
pub struct TierMigrationPayload {
    pub entity_id: String,
    pub collection: String,
    pub from_tier: Tier,
    pub to_tier: Tier,
    pub encoding: Encoding,
}
```

`Tier` and `Encoding` already derive `Serialize`/`Deserialize`, so they serialise directly via MessagePack — no string conversion needed.

### 6.3 Replay

On startup, WAL replay processes `TierMigration` records:
- Verify entity exists in the target tier's partition
- If yes: update Location Table to match (migration completed)
- If no: entity is still in source tier (migration was incomplete) — set `LocState::Clean` on source

---

## 7. Integration with Phase 6 Routing

### 7.0 Concurrent FETCH During Migration

When a FETCH hits an entity in `LocState::Migrating`, the entity's data still exists in the source tier partition (deletion is the last migration step). FETCH reads from the source tier partition, ignoring the `Migrating` state. The response includes `"tier": "Migrating"` to indicate the entity is in transit. No blocking or errors.

### 7.1 Eviction → Demotion Bridge

Phase 6's `select_eviction_candidates()` returns entities to evict from hot tier. Phase 7a connects this to the migration pipeline:
- Instead of just tracking candidates, eviction triggers demotion to warm
- The eviction priority order is preserved: ungrouped LRU → implicit groups → explicit groups
- Explicit affinity groups are demoted last (they were placed together intentionally)

### 7.2 HealthSignal Extension

Add to `HealthSignal`:
```rust
pub warm_entity_count: u64,
pub archive_entity_count: u64,
```

`LocalNode::health_snapshot()` populates these using the new `Engine::tier_entity_count(collection, tier)` method (which delegates to `FjallStore::tier_entity_count`).

### 7.3 EXPLAIN Extension

EXPLAIN output for queries adds tier information:
```
tier_distribution:
  hot: 45000
  warm: 120000
  archive: 500000
```

---

## 8. Response Format Changes

FETCH responses include a `tier` field:
```
{ "id": "entity_1", "tier": "Hot", "name": "...", "vector": [...] }
{ "id": "entity_2", "tier": "Warm", "name": "...", "vector": [...] }  // dequantised
{ "id": "entity_3", "tier": "Archive", "name": "..." }  // no vector, lossy
```

---

## 9. Deferred (Not Phase 7a)

- Multi-hop TRAVERSE (DEPTH > 1) → Phase 7b
- Entity deletion → Phase 7b
- Range/prefix FETCH → Phase 7b
- Edge confidence decay → Phase 7b
- Real NVMe/SSD tiering (separate mount points) → future
- Per-collection tier config → future
- Sharding → post-v1

---

## 10. File Summary

| File | Action | Description |
|------|--------|-------------|
| `crates/trondb-core/src/quantise.rs` | Create | Int8/Binary quantisation + dequantisation |
| `crates/trondb-core/src/store.rs` | Modify | Tiered partition methods |
| `crates/trondb-core/src/executor.rs` | Modify | Tier-aware FETCH read path |
| `crates/trondb-core/src/index.rs` | Modify | HNSW tombstone removal + rebuild |
| `crates/trondb-core/src/location.rs` | Modify | Add `last_accessed` to LocationDescriptor |
| `crates/trondb-routing/src/config.rs` | Modify | Add TierConfig |
| `crates/trondb-routing/src/migrator.rs` | Create | TierMigrator background task |
| `crates/trondb-routing/src/node.rs` | Modify | HealthSignal warm/archive counts |
| `crates/trondb-routing/src/eviction.rs` | Modify | Bridge to demotion pipeline |
| `crates/trondb-tql/src/token.rs` | Modify | DEMOTE, PROMOTE, TIERS, WARM, ARCHIVE tokens |
| `crates/trondb-tql/src/ast.rs` | Modify | DemoteStmt, PromoteStmt, ExplainTiersStmt |
| `crates/trondb-tql/src/parser.rs` | Modify | Parse DEMOTE/PROMOTE/EXPLAIN TIERS |
| `crates/trondb-core/src/planner.rs` | Modify | Demote, Promote, ExplainTiers plan variants |
| `crates/trondb-wal/src/record.rs` | Modify | TierMigration record type |
| `CLAUDE.md` | Modify | Phase 7a documentation |
