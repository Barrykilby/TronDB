# TronDB Phase 3 — Location Table

## Goal

Build the Location Table: TronDB's control fabric that decouples logical entity identity from physical storage location. DashMap-backed, WAL-logged, periodically snapshotted, always RAM-resident.

## Source Requirements

- v3 design doc §4 (Location Table structure, properties, problems solved)
- v4 design doc §3.3, Table 3, Table 5, Table 13, Table 19 Phase 3 row
- v4 Table 19: "Tier descriptors, state machine, localhost addressing. DashMap-backed, WAL-logged, periodically snapshotted."

## Architecture

The Location Table is the **control fabric**, separate from the **data fabric** (Fjall). It holds a `LocationDescriptor` for every Representation of every Entity. It lives entirely in RAM (DashMap), is WAL-logged for durability, and periodically snapshotted for fast restart.

### Two Planes (v4 Table 3)

| Plane | Contains | Properties |
|-------|----------|------------|
| Control Fabric | Location Table, HNSW topology (Phase 4+), edge metadata (Phase 5+), confidence scores (Phase 6+) | Always in RAM. DashMap. WAL-logged. Sub-microsecond lookup. |
| Data Fabric | Entity bytes, vectors, structural data | Lives in Fjall (Phase 2-3), tiered in Phase 7+. Routed through control fabric. |

## Types

### Tier

Where data physically lives. Phase 3 uses `Fjall` only.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tier { Ram, Fjall, NVMe, Archive }

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self { Tier::Ram => "Ram", Tier::Fjall => "Fjall", Tier::NVMe => "NVMe", Tier::Archive => "Archive" }
    }
}
```

### LocState — State Machine

The Location Table's `LocState` is the **authoritative state** for where a representation stands in the tier lifecycle. The existing `ReprState` on `Representation` tracks whether the representation's *vector data* needs recomputation (it's a data-plane concern). `LocState` tracks whether the representation's *location descriptor* is in a valid serving state (control-plane concern). In Phase 4, when a mutation marks a `ReprState` as `Dirty`, the corresponding `LocState` transitions `Clean → Dirty` in the Location Table simultaneously. They are kept in sync but serve different planes.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LocState { Clean, Dirty, Recomputing, Migrating }
```

Valid transitions:
- `Clean → Dirty` — entity mutated, representation stale (Phase 4+)
- `Dirty → Recomputing` — background rebuild starts (Phase 4+)
- `Recomputing → Clean` — rebuild complete
- `Clean → Migrating` — tier promotion/demotion (Phase 7+)
- `Migrating → Clean` — migration confirmed

Invalid transitions return `EngineError::InvalidStateTransition`.

Phase 3: all descriptors start and remain `Clean`. State machine is implemented and tested but not actively driven until later phases.

### Encoding

Vector encoding at this tier. Phase 3 uses `Float32` only. Phase 7 adds `Int8` and `Binary` for NVMe. The `Encoding` on the `LocationDescriptor` describes what format the vector is stored in at the tier indicated by `Tier`. It must match the actual storage format — the Location Table is metadata about data, not the data itself.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Encoding { Float32, Int8, Binary }
```

### NodeAddress

Network-transparent addressing (v4 Table 5). Single-node uses localhost. Multi-node populates real addresses. Code path identical in both cases.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeAddress {
    pub host: String,  // "localhost" in single-node
    pub port: u16,
}

impl NodeAddress {
    pub fn localhost() -> Self {
        Self { host: "localhost".into(), port: 0 }
    }
}
```

### LocationDescriptor (v3 §4.1)

One per Representation per Entity.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationDescriptor {
    pub tier: Tier,
    pub node_address: NodeAddress,
    pub state: LocState,
    pub version: u64,           // incremented on every mutation to this descriptor
    pub encoding: Encoding,
}
```

The `version` field is incremented whenever the descriptor is modified (state transition, tier change, re-registration). It enables optimistic concurrency in future phases and is used in snapshot consistency checks.

### ReprKey

Composite key identifying a specific representation of a specific entity. Requires `Hash + Eq` for use as DashMap key.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReprKey {
    pub entity_id: LogicalId,
    pub repr_index: u32,
}
```

## LocationTable

```rust
pub struct LocationTable {
    // Primary index: direct lookup by (entity_id, repr_index)
    descriptors: DashMap<ReprKey, LocationDescriptor>,
    // Secondary index: entity_id → set of repr_indices (avoids full scan for get_entity/remove_entity)
    entity_reprs: DashMap<LogicalId, Vec<u32>>,
    // Monotonic mutation counter, used as snapshot LSN
    mutation_counter: AtomicU64,
}
```

The secondary index `entity_reprs` maps each entity to its repr indices, avoiding O(n) full scans when looking up or removing all representations for an entity. Both maps are updated atomically on register/remove.

### API

```rust
impl LocationTable {
    pub fn new() -> Self;

    // Register a representation's location. Upsert semantics: overwrites if exists.
    // Increments version on the descriptor if overwriting.
    pub fn register(&self, key: ReprKey, descriptor: LocationDescriptor);

    // Look up a specific representation
    pub fn get(&self, key: &ReprKey) -> Option<LocationDescriptor>;

    // Get all representations for an entity (uses secondary index, not a scan)
    pub fn get_entity(&self, entity_id: &LogicalId) -> EntityLocation;

    // Transition state (enforces valid transitions)
    // Returns EngineError::InvalidStateTransition on invalid transition
    // Returns EngineError::LocationNotFound if key doesn't exist
    pub fn transition(&self, key: &ReprKey, new_state: LocState) -> Result<(), EngineError>;

    // Update tier (for promotion/demotion)
    // Returns EngineError::LocationNotFound if key doesn't exist
    pub fn update_tier(&self, key: &ReprKey, tier: Tier, encoding: Encoding) -> Result<(), EngineError>;

    // Remove all descriptors for an entity (uses secondary index)
    pub fn remove_entity(&self, entity_id: &LogicalId);

    // Snapshot to bytes (MessagePack)
    // Returns current mutation_counter as the snapshot LSN
    pub fn snapshot(&self) -> Result<(u64, Vec<u8>), EngineError>;

    // Restore from snapshot bytes
    pub fn restore(bytes: &[u8]) -> Result<(Self, u64), EngineError>;

    // Entry count (number of descriptors)
    pub fn len(&self) -> usize;

    // Current mutation counter (used as snapshot LSN)
    pub fn mutation_counter(&self) -> u64;
}
```

`EntityLocation` is a transient query return type, not used in the snapshot path. Snapshots serialise `Vec<(ReprKey, LocationDescriptor)>` directly.

## Error Handling

New `EngineError` variants:

```rust
#[error("invalid state transition: {from:?} → {to:?}")]
InvalidStateTransition { from: LocState, to: LocState },

#[error("location not found: {0}")]
LocationNotFound(String),
```

## WAL Integration

Location Table mutations use `RecordType::LocationUpdate` (0x40), already defined in `trondb-wal`. Payload: MessagePack-encoded struct containing `ReprKey` + `LocationDescriptor`.

### INSERT write path (single WAL transaction)

The Location Table registration is part of the **same WAL transaction** as the entity write. The sequence within one tx_id:

1. WAL `TxBegin`
2. WAL `EntityWrite` (entity payload)
3. WAL `LocationUpdate` (for each representation)
4. WAL `TxCommit` (flush + fsync)
5. Apply entity to Fjall
6. Apply descriptor to DashMap
7. Fjall persist

If crash occurs between steps 4 and 6, WAL recovery replays both the entity and the location update atomically.

### Standalone Location Table updates (state transitions, tier changes)

For state transitions and tier changes (Phase 4+), the write path is:

1. WAL `TxBegin`
2. WAL `LocationUpdate`
3. WAL `TxCommit` (flush + fsync)
4. Apply to DashMap

### Recovery

On startup, after Fjall store is opened:
1. Load Location Table snapshot if present → get snapshot LSN
2. Run `WalRecovery::recover()` → get committed records
3. Filter `LocationUpdate` records with LSN > snapshot LSN
4. Replay those records into the DashMap

The snapshot LSN (from `LocationTable.mutation_counter`) is stored in the snapshot header alongside the WAL head LSN at snapshot time. Recovery filters by WAL LSN, not mutation counter — the snapshot header records the WAL head LSN at snapshot time.

## Snapshots

### Format

```
[8 bytes: magic "TRONLOC1"]
[8 bytes: WAL head LSN at snapshot time (u64 BE)]
[8 bytes: entry count (u64 BE)]
[remainder: MessagePack Vec<(ReprKey, LocationDescriptor)>]
```

File: `{data_dir}/location_table.snap`

### Lifecycle

- **On startup:** Load snapshot if present, then replay WAL `LocationUpdate` records with LSN > snapshot's WAL head LSN.
- **Periodic:** Background Tokio task snapshots every N seconds. Default 60s. Configured via `EngineConfig::snapshot_interval_secs: u64`.
- **On shutdown:** Snapshot before exit (best-effort).

## Executor Integration

### INSERT

Within the same WAL transaction as the entity write, append a `LocationUpdate` record for each representation. After WAL commit, apply both entity to Fjall and descriptor to DashMap.

Descriptor values:
- `tier: Tier::Fjall`
- `node_address: NodeAddress::localhost()`
- `state: LocState::Clean`
- `version: 1`
- `encoding: Encoding::Float32`

Entities with no vector/representation: no Location Table entry (the Location Table tracks representations, not entities without them).

### FETCH

Consult Location Table to resolve where entity representations live. In Phase 3 everything resolves to Fjall/localhost — but the lookup path is real and will route to different tiers in Phase 7+.

### EXPLAIN

Currently hardcoded `tier: "Fjall"`. Change `QueryStats.tier` from `&'static str` to `String`. Phase 3 reads tier from Location Table via `Tier::as_str()`, making EXPLAIN output dynamic.

## Crate Placement

- New file: `crates/trondb-core/src/location.rs`
- DashMap added as workspace dependency
- No new crate needed — Location Table is engine-internal

## Configuration

New field on `EngineConfig`:

```rust
pub snapshot_interval_secs: u64,  // default: 60
```

## What Phase 3 Does NOT Build

- Tier promotion/demotion logic (Phase 7)
- HNSW integration (Phase 4)
- Multi-node addressing (Phase 9-10)
- Co-location/affinity (routing doc scope)
- Temporal fields on descriptors (Phase 8+)
- Dirty marking on entity mutation (Phase 4)

## Testing

- Unit tests for LocationTable CRUD (register, get, get_entity, remove_entity)
- Unit tests for state machine transitions (all valid transitions pass, invalid transitions error)
- Unit tests for snapshot/restore round-trip (data integrity, LSN preserved)
- Unit tests for secondary index consistency (register/remove keeps entity_reprs in sync)
- Integration test: insert with vector → Location Table entry exists with correct descriptor
- Integration test: snapshot → restart → Location Table restored from snapshot + WAL replay
- Integration test: LocationUpdate records in WAL → replayed into Location Table on recovery
- Integration test: single WAL transaction contains both EntityWrite and LocationUpdate
