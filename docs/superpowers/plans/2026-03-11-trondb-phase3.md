# Phase 3: Location Table Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the Location Table — TronDB's DashMap-backed control fabric that tracks where every representation lives, with WAL-logged mutations and periodic snapshots.

**Architecture:** The Location Table is an in-memory DashMap keyed by `(entity_id, repr_index)`, separate from the Fjall data fabric. It is WAL-logged via `RecordType::LocationUpdate` (0x40), periodically snapshotted to `location_table.snap`, and rebuilt from snapshot + WAL replay on startup.

**Tech Stack:** DashMap 6, rmp-serde (MessagePack), Tokio (background snapshot task), existing trondb-wal infrastructure.

**Spec:** `docs/superpowers/specs/2026-03-11-trondb-phase3-design.md`

---

## Chunk 1: Types, Error Variants, and LocationTable Core

### Task 1: Add DashMap dependency and location module

**Files:**
- Modify: `Cargo.toml` (workspace deps — dashmap already present at line 16)
- Modify: `crates/trondb-core/Cargo.toml` (add dashmap dependency)
- Create: `crates/trondb-core/src/location.rs`
- Modify: `crates/trondb-core/src/lib.rs:1` (add `pub mod location;`)

- [ ] **Step 1: Add dashmap to trondb-core dependencies**

In `crates/trondb-core/Cargo.toml`, add under `[dependencies]`:
```toml
dashmap.workspace = true
```

- [ ] **Step 2: Create location.rs with Location Table types**

Create `crates/trondb-core/src/location.rs` with the types from the spec:

```rust
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::error::EngineError;
use crate::types::LogicalId;

// ---------------------------------------------------------------------------
// Location Table types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tier {
    Ram,
    Fjall,
    NVMe,
    Archive,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::Ram => "Ram",
            Tier::Fjall => "Fjall",
            Tier::NVMe => "NVMe",
            Tier::Archive => "Archive",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LocState {
    Clean,
    Dirty,
    Recomputing,
    Migrating,
}

impl LocState {
    /// Returns true if `self → target` is a valid state transition.
    pub fn can_transition_to(&self, target: LocState) -> bool {
        matches!(
            (self, target),
            (LocState::Clean, LocState::Dirty)
                | (LocState::Dirty, LocState::Recomputing)
                | (LocState::Recomputing, LocState::Clean)
                | (LocState::Clean, LocState::Migrating)
                | (LocState::Migrating, LocState::Clean)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Encoding {
    Float32,
    Int8,
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeAddress {
    pub host: String,
    pub port: u16,
}

impl NodeAddress {
    pub fn localhost() -> Self {
        Self {
            host: "localhost".into(),
            port: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReprKey {
    pub entity_id: LogicalId,
    pub repr_index: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationDescriptor {
    pub tier: Tier,
    pub node_address: NodeAddress,
    pub state: LocState,
    pub version: u64,
    pub encoding: Encoding,
}

/// Per-entity view returned by get_entity(). Transient query return type.
pub struct EntityLocation {
    pub id: LogicalId,
    pub representations: HashMap<u32, LocationDescriptor>,
}

// ---------------------------------------------------------------------------
// LocationTable
// ---------------------------------------------------------------------------

pub struct LocationTable {
    descriptors: DashMap<ReprKey, LocationDescriptor>,
    entity_reprs: DashMap<LogicalId, Vec<u32>>,
    mutation_counter: AtomicU64,
}

impl LocationTable {
    pub fn new() -> Self {
        Self {
            descriptors: DashMap::new(),
            entity_reprs: DashMap::new(),
            mutation_counter: AtomicU64::new(0),
        }
    }

    /// Register a representation's location. Upsert: overwrites if exists.
    pub fn register(&self, key: ReprKey, descriptor: LocationDescriptor) {
        self.descriptors.insert(key.clone(), descriptor);

        // Update secondary index
        self.entity_reprs
            .entry(key.entity_id)
            .or_default()
            .push(key.repr_index);

        // Deduplicate (idempotent for WAL replay)
        self.entity_reprs.alter_all(|_, mut v| {
            v.sort_unstable();
            v.dedup();
            v
        });

        self.mutation_counter.fetch_add(1, Ordering::SeqCst);
    }

    /// Look up a specific representation's location.
    pub fn get(&self, key: &ReprKey) -> Option<LocationDescriptor> {
        self.descriptors.get(key).map(|r| r.clone())
    }

    /// Get all representations for an entity (uses secondary index).
    pub fn get_entity(&self, entity_id: &LogicalId) -> EntityLocation {
        let mut representations = HashMap::new();
        if let Some(indices) = self.entity_reprs.get(entity_id) {
            for &idx in indices.value() {
                let key = ReprKey {
                    entity_id: entity_id.clone(),
                    repr_index: idx,
                };
                if let Some(desc) = self.descriptors.get(&key) {
                    representations.insert(idx, desc.clone());
                }
            }
        }
        EntityLocation {
            id: entity_id.clone(),
            representations,
        }
    }

    /// Transition state. Enforces valid transitions.
    pub fn transition(
        &self,
        key: &ReprKey,
        new_state: LocState,
    ) -> Result<(), EngineError> {
        let mut entry = self
            .descriptors
            .get_mut(key)
            .ok_or_else(|| EngineError::LocationNotFound(format!("{:?}", key)))?;

        let current = entry.state;
        if !current.can_transition_to(new_state) {
            return Err(EngineError::InvalidStateTransition {
                from: current,
                to: new_state,
            });
        }

        entry.state = new_state;
        entry.version += 1;
        self.mutation_counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Update tier and encoding (for promotion/demotion).
    pub fn update_tier(
        &self,
        key: &ReprKey,
        tier: Tier,
        encoding: Encoding,
    ) -> Result<(), EngineError> {
        let mut entry = self
            .descriptors
            .get_mut(key)
            .ok_or_else(|| EngineError::LocationNotFound(format!("{:?}", key)))?;

        entry.tier = tier;
        entry.encoding = encoding;
        entry.version += 1;
        self.mutation_counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Remove all descriptors for an entity (uses secondary index).
    pub fn remove_entity(&self, entity_id: &LogicalId) {
        if let Some((_, indices)) = self.entity_reprs.remove(entity_id) {
            for idx in indices {
                let key = ReprKey {
                    entity_id: entity_id.clone(),
                    repr_index: idx,
                };
                self.descriptors.remove(&key);
            }
        }
        self.mutation_counter.fetch_add(1, Ordering::SeqCst);
    }

    /// Number of descriptors in the table.
    pub fn len(&self) -> usize {
        self.descriptors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }

    /// Current mutation counter.
    pub fn mutation_counter(&self) -> u64 {
        self.mutation_counter.load(Ordering::SeqCst)
    }
}
```

- [ ] **Step 3: Add location module to lib.rs**

Add `pub mod location;` to `crates/trondb-core/src/lib.rs` line 1 area (after existing modules).

- [ ] **Step 4: Add new error variants**

In `crates/trondb-core/src/error.rs`, add after the `UnsupportedOperation` variant:

```rust
#[error("invalid state transition: {from:?} → {to:?}")]
InvalidStateTransition {
    from: crate::location::LocState,
    to: crate::location::LocState,
},

#[error("location not found: {0}")]
LocationNotFound(String),
```

- [ ] **Step 5: Run tests to verify compilation**

Run: `cargo test --workspace`
Expected: All existing 58 tests pass. No new tests yet.

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/Cargo.toml crates/trondb-core/src/location.rs crates/trondb-core/src/lib.rs crates/trondb-core/src/error.rs
git commit -m "feat(location): add Location Table types, DashMap-backed LocationTable, error variants"
```

---

### Task 2: LocationTable unit tests

**Files:**
- Modify: `crates/trondb-core/src/location.rs` (add `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write unit tests for LocationTable**

Add to the bottom of `crates/trondb-core/src/location.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn make_key(entity: &str, idx: u32) -> ReprKey {
        ReprKey {
            entity_id: LogicalId::from_string(entity),
            repr_index: idx,
        }
    }

    fn make_descriptor() -> LocationDescriptor {
        LocationDescriptor {
            tier: Tier::Fjall,
            node_address: NodeAddress::localhost(),
            state: LocState::Clean,
            version: 1,
            encoding: Encoding::Float32,
        }
    }

    #[test]
    fn register_and_get() {
        let lt = LocationTable::new();
        let key = make_key("e1", 0);
        lt.register(key.clone(), make_descriptor());

        let desc = lt.get(&key).expect("should exist");
        assert_eq!(desc.tier, Tier::Fjall);
        assert_eq!(desc.state, LocState::Clean);
        assert_eq!(lt.len(), 1);
    }

    #[test]
    fn register_upsert_overwrites() {
        let lt = LocationTable::new();
        let key = make_key("e1", 0);
        lt.register(key.clone(), make_descriptor());

        let mut updated = make_descriptor();
        updated.version = 2;
        updated.tier = Tier::Ram;
        lt.register(key.clone(), updated);

        let desc = lt.get(&key).unwrap();
        assert_eq!(desc.tier, Tier::Ram);
        assert_eq!(desc.version, 2);
        assert_eq!(lt.len(), 1); // still one entry
    }

    #[test]
    fn get_entity_returns_all_reprs() {
        let lt = LocationTable::new();
        let eid = LogicalId::from_string("e1");
        lt.register(make_key("e1", 0), make_descriptor());
        lt.register(make_key("e1", 1), make_descriptor());
        lt.register(make_key("e2", 0), make_descriptor()); // different entity

        let loc = lt.get_entity(&eid);
        assert_eq!(loc.representations.len(), 2);
        assert!(loc.representations.contains_key(&0));
        assert!(loc.representations.contains_key(&1));
    }

    #[test]
    fn get_entity_empty_for_unknown() {
        let lt = LocationTable::new();
        let loc = lt.get_entity(&LogicalId::from_string("nope"));
        assert!(loc.representations.is_empty());
    }

    #[test]
    fn remove_entity_clears_all_reprs() {
        let lt = LocationTable::new();
        lt.register(make_key("e1", 0), make_descriptor());
        lt.register(make_key("e1", 1), make_descriptor());
        lt.register(make_key("e2", 0), make_descriptor());

        lt.remove_entity(&LogicalId::from_string("e1"));

        assert!(lt.get(&make_key("e1", 0)).is_none());
        assert!(lt.get(&make_key("e1", 1)).is_none());
        assert!(lt.get(&make_key("e2", 0)).is_some()); // untouched
        assert_eq!(lt.len(), 1);
    }

    #[test]
    fn valid_state_transitions() {
        let lt = LocationTable::new();
        let key = make_key("e1", 0);
        lt.register(key.clone(), make_descriptor()); // Clean

        // Clean → Dirty
        lt.transition(&key, LocState::Dirty).unwrap();
        assert_eq!(lt.get(&key).unwrap().state, LocState::Dirty);

        // Dirty → Recomputing
        lt.transition(&key, LocState::Recomputing).unwrap();
        assert_eq!(lt.get(&key).unwrap().state, LocState::Recomputing);

        // Recomputing → Clean
        lt.transition(&key, LocState::Clean).unwrap();
        assert_eq!(lt.get(&key).unwrap().state, LocState::Clean);

        // Clean → Migrating
        lt.transition(&key, LocState::Migrating).unwrap();
        assert_eq!(lt.get(&key).unwrap().state, LocState::Migrating);

        // Migrating → Clean
        lt.transition(&key, LocState::Clean).unwrap();
        assert_eq!(lt.get(&key).unwrap().state, LocState::Clean);
    }

    #[test]
    fn invalid_state_transition_errors() {
        let lt = LocationTable::new();
        let key = make_key("e1", 0);
        lt.register(key.clone(), make_descriptor()); // Clean

        // Clean → Recomputing (invalid)
        assert!(lt.transition(&key, LocState::Recomputing).is_err());
        // Clean → Clean (invalid — no self-transitions)
        assert!(lt.transition(&key, LocState::Clean).is_err());
    }

    #[test]
    fn transition_nonexistent_key_errors() {
        let lt = LocationTable::new();
        let key = make_key("nope", 0);
        assert!(lt.transition(&key, LocState::Dirty).is_err());
    }

    #[test]
    fn update_tier() {
        let lt = LocationTable::new();
        let key = make_key("e1", 0);
        lt.register(key.clone(), make_descriptor());

        lt.update_tier(&key, Tier::NVMe, Encoding::Int8).unwrap();
        let desc = lt.get(&key).unwrap();
        assert_eq!(desc.tier, Tier::NVMe);
        assert_eq!(desc.encoding, Encoding::Int8);
        assert_eq!(desc.version, 2); // incremented from 1
    }

    #[test]
    fn update_tier_nonexistent_errors() {
        let lt = LocationTable::new();
        assert!(lt
            .update_tier(&make_key("nope", 0), Tier::Ram, Encoding::Float32)
            .is_err());
    }

    #[test]
    fn transition_increments_version() {
        let lt = LocationTable::new();
        let key = make_key("e1", 0);
        lt.register(key.clone(), make_descriptor()); // version 1

        lt.transition(&key, LocState::Dirty).unwrap();
        assert_eq!(lt.get(&key).unwrap().version, 2);

        lt.transition(&key, LocState::Recomputing).unwrap();
        assert_eq!(lt.get(&key).unwrap().version, 3);
    }

    #[test]
    fn mutation_counter_tracks_changes() {
        let lt = LocationTable::new();
        assert_eq!(lt.mutation_counter(), 0);

        lt.register(make_key("e1", 0), make_descriptor());
        assert!(lt.mutation_counter() > 0);

        let before = lt.mutation_counter();
        lt.transition(&make_key("e1", 0), LocState::Dirty).unwrap();
        assert!(lt.mutation_counter() > before);
    }

    #[test]
    fn node_address_localhost() {
        let addr = NodeAddress::localhost();
        assert_eq!(addr.host, "localhost");
        assert_eq!(addr.port, 0);
    }

    #[test]
    fn tier_as_str() {
        assert_eq!(Tier::Fjall.as_str(), "Fjall");
        assert_eq!(Tier::Ram.as_str(), "Ram");
        assert_eq!(Tier::NVMe.as_str(), "NVMe");
        assert_eq!(Tier::Archive.as_str(), "Archive");
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --workspace`
Expected: All previous 58 tests pass + ~14 new LocationTable tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-core/src/location.rs
git commit -m "test(location): add unit tests for LocationTable CRUD, state machine, tier updates"
```

---

## Chunk 2: Snapshots

### Task 3: Snapshot and restore

**Files:**
- Modify: `crates/trondb-core/src/location.rs` (add snapshot/restore methods)

- [ ] **Step 1: Add snapshot/restore methods to LocationTable**

Add to the `impl LocationTable` block, after `mutation_counter()`:

```rust
    /// Snapshot the Location Table to bytes.
    /// Returns (wal_head_lsn, bytes). The caller provides the current WAL head LSN.
    pub fn snapshot(&self, wal_head_lsn: u64) -> Result<Vec<u8>, EngineError> {
        let entries: Vec<(ReprKey, LocationDescriptor)> = self
            .descriptors
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect();

        let body = rmp_serde::to_vec_named(&entries)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let count = entries.len() as u64;

        let mut out = Vec::with_capacity(24 + body.len());
        out.extend_from_slice(b"TRONLOC1");               // 8 bytes magic
        out.extend_from_slice(&wal_head_lsn.to_be_bytes()); // 8 bytes LSN
        out.extend_from_slice(&count.to_be_bytes());        // 8 bytes count
        out.extend_from_slice(&body);

        Ok(out)
    }

    /// Restore a LocationTable from snapshot bytes.
    /// Returns (LocationTable, wal_head_lsn_at_snapshot).
    pub fn restore(bytes: &[u8]) -> Result<(Self, u64), EngineError> {
        if bytes.len() < 24 {
            return Err(EngineError::Storage("snapshot too short".into()));
        }
        if &bytes[0..8] != b"TRONLOC1" {
            return Err(EngineError::Storage("invalid snapshot magic".into()));
        }

        let wal_head_lsn = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
        let count = u64::from_be_bytes(bytes[16..24].try_into().unwrap());

        let entries: Vec<(ReprKey, LocationDescriptor)> =
            rmp_serde::from_slice(&bytes[24..])
                .map_err(|e| EngineError::Storage(e.to_string()))?;

        if entries.len() != count as usize {
            return Err(EngineError::Storage(format!(
                "snapshot count mismatch: header says {count}, body has {}",
                entries.len()
            )));
        }

        let lt = Self::new();
        for (key, desc) in entries {
            lt.register(key, desc);
        }

        Ok((lt, wal_head_lsn))
    }
```

Note: this requires adding `use rmp_serde;` to the imports at the top of `location.rs` (it's already a workspace dependency of trondb-core).

- [ ] **Step 2: Write snapshot/restore tests**

Add to the `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn snapshot_and_restore_round_trip() {
        let lt = LocationTable::new();
        lt.register(make_key("e1", 0), make_descriptor());
        lt.register(make_key("e1", 1), make_descriptor());
        lt.register(make_key("e2", 0), make_descriptor());

        let bytes = lt.snapshot(42).unwrap();
        let (restored, lsn) = LocationTable::restore(&bytes).unwrap();

        assert_eq!(lsn, 42);
        assert_eq!(restored.len(), 3);
        assert!(restored.get(&make_key("e1", 0)).is_some());
        assert!(restored.get(&make_key("e1", 1)).is_some());
        assert!(restored.get(&make_key("e2", 0)).is_some());

        // Secondary index should be rebuilt
        let loc = restored.get_entity(&LogicalId::from_string("e1"));
        assert_eq!(loc.representations.len(), 2);
    }

    #[test]
    fn restore_rejects_bad_magic() {
        let bytes = b"BADMAGIC00000000000000001234";
        assert!(LocationTable::restore(bytes).is_err());
    }

    #[test]
    fn restore_rejects_truncated() {
        let bytes = b"TRONLOC1";
        assert!(LocationTable::restore(bytes).is_err());
    }

    #[test]
    fn snapshot_empty_table() {
        let lt = LocationTable::new();
        let bytes = lt.snapshot(0).unwrap();
        let (restored, lsn) = LocationTable::restore(&bytes).unwrap();
        assert_eq!(lsn, 0);
        assert_eq!(restored.len(), 0);
    }
```

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass including new snapshot tests.

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-core/src/location.rs
git commit -m "feat(location): add snapshot/restore with TRONLOC1 format"
```

---

## Chunk 3: WAL Integration and Recovery

### Task 4: Wire LocationUpdate into INSERT write path

**Files:**
- Modify: `crates/trondb-core/src/executor.rs:116-181` (INSERT handler)
- Modify: `crates/trondb-core/src/executor.rs:16-19` (add location to Executor)

- [ ] **Step 1: Add LocationTable to Executor**

In `crates/trondb-core/src/executor.rs`, change the Executor struct and constructor:

```rust
use crate::location::{
    Encoding, LocState, LocationDescriptor, LocationTable, NodeAddress, ReprKey, Tier,
};

pub struct Executor {
    store: FjallStore,
    wal: WalWriter,
    location: LocationTable,
}

impl Executor {
    pub fn new(store: FjallStore, wal: WalWriter, location: LocationTable) -> Self {
        Self { store, wal, location }
    }
```

- [ ] **Step 2: Update INSERT handler to WAL-log LocationUpdate in same transaction**

In the `Plan::Insert` handler (executor.rs), change the WAL section to include LocationUpdate records. Replace lines 152-163:

```rust
                // WAL: TX_BEGIN → ENTITY_WRITE → LOCATION_UPDATE(s) → commit
                let tx_id = self.wal.next_tx_id();
                self.wal.append(RecordType::TxBegin, &p.collection, tx_id, 1, vec![]);

                let entity_payload = rmp_serde::to_vec_named(&entity)
                    .map_err(|e| EngineError::Storage(e.to_string()))?;
                self.wal.append(RecordType::EntityWrite, &p.collection, tx_id, 1, entity_payload);

                // LocationUpdate for each representation
                for (idx, _repr) in entity.representations.iter().enumerate() {
                    let loc_key = ReprKey {
                        entity_id: id.clone(),
                        repr_index: idx as u32,
                    };
                    let loc_desc = LocationDescriptor {
                        tier: Tier::Fjall,
                        node_address: NodeAddress::localhost(),
                        state: LocState::Clean,
                        version: 1,
                        encoding: Encoding::Float32,
                    };
                    let loc_payload = rmp_serde::to_vec_named(&(&loc_key, &loc_desc))
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                    self.wal.append(RecordType::LocationUpdate, &p.collection, tx_id, 1, loc_payload);
                }

                self.wal.commit(tx_id).await?;

                // Apply to Fjall
                self.store.insert(&p.collection, entity.clone())?;
                self.store.persist()?;

                // Apply to Location Table
                for (idx, _repr) in entity.representations.iter().enumerate() {
                    self.location.register(
                        ReprKey {
                            entity_id: id.clone(),
                            repr_index: idx as u32,
                        },
                        LocationDescriptor {
                            tier: Tier::Fjall,
                            node_address: NodeAddress::localhost(),
                            state: LocState::Clean,
                            version: 1,
                            encoding: Encoding::Float32,
                        },
                    );
                }
```

- [ ] **Step 3: Add LocationUpdate replay to replay_wal_records**

In the `replay_wal_records` method, add a handler for `LocationUpdate` after the `EntityWrite` handler:

```rust
                RecordType::LocationUpdate => {
                    let (key, desc): (ReprKey, LocationDescriptor) =
                        rmp_serde::from_slice(&record.payload)
                            .map_err(|e| EngineError::Storage(e.to_string()))?;
                    self.location.register(key, desc);
                    replayed += 1;
                }
```

- [ ] **Step 4: Add location() accessor to Executor**

```rust
    pub fn location(&self) -> &LocationTable {
        &self.location
    }
```

- [ ] **Step 5: Update Engine::open to create and pass LocationTable**

In `crates/trondb-core/src/lib.rs`, update `Engine::open`:

```rust
use crate::location::LocationTable;

// In Engine::open:
    pub async fn open(config: EngineConfig) -> Result<Self, EngineError> {
        let store = FjallStore::open(&config.data_dir)?;

        // Load Location Table from snapshot if available
        let snap_path = config.data_dir.join("location_table.snap");
        let (location, snap_lsn) = if snap_path.exists() {
            let bytes = tokio::fs::read(&snap_path).await
                .map_err(|e| EngineError::Storage(e.to_string()))?;
            let (lt, lsn) = LocationTable::restore(&bytes)?;
            eprintln!("Location Table: restored {} entries from snapshot (LSN {lsn})", lt.len());
            (lt, lsn)
        } else {
            (LocationTable::new(), 0)
        };

        // Replay committed WAL records
        let recovery = WalRecovery::recover(&config.wal.wal_dir).await?;
        let wal = WalWriter::open(config.wal).await?;
        let executor = Executor::new(store, wal, location);

        // Filter records to only replay those after snapshot LSN
        let records_to_replay: Vec<_> = recovery
            .records
            .iter()
            .filter(|r| r.lsn > snap_lsn)
            .cloned()
            .collect();

        let replayed = executor.replay_wal_records(&records_to_replay)?;
        if replayed > 0 {
            eprintln!("WAL recovery: replayed {replayed} records");
        }

        Ok(Self { executor })
    }
```

Also add `use crate::location::LocationTable;` at the top of lib.rs imports.

- [ ] **Step 6: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass. Some executor tests may need updating if Executor::new signature changed — update `setup_executor()` in executor tests to pass `LocationTable::new()`.

- [ ] **Step 7: Commit**

```bash
git add crates/trondb-core/src/executor.rs crates/trondb-core/src/lib.rs
git commit -m "feat(location): wire LocationTable into executor write path and WAL recovery"
```

---

### Task 5: Change QueryStats.tier to String

**Files:**
- Modify: `crates/trondb-core/src/result.rs:24` (change `tier: &'static str` to `tier: String`)
- Modify: `crates/trondb-core/src/executor.rs` (all `tier: "Fjall"` → `tier: "Fjall".into()`)
- Modify: `crates/trondb-cli/src/display.rs` (if it references tier)

- [ ] **Step 1: Change tier type in QueryStats**

In `crates/trondb-core/src/result.rs`, change line 24:
```rust
pub tier: String,
```

- [ ] **Step 2: Update all tier assignments in executor.rs**

Find all `tier: "Fjall"` in executor.rs and change to `tier: "Fjall".into()`. There are 5 occurrences (in CreateCollection, Insert, Fetch, Search (in explain_plan), and Explain result stats).

- [ ] **Step 3: Check and update display.rs if needed**

Read `crates/trondb-cli/src/display.rs` and update any reference to `stats.tier` that assumes `&str`.

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/result.rs crates/trondb-core/src/executor.rs crates/trondb-cli/src/display.rs
git commit -m "refactor: change QueryStats.tier from &'static str to String"
```

---

## Chunk 4: Snapshot Background Task and Engine Integration

### Task 6: Add snapshot_interval_secs to EngineConfig

**Files:**
- Modify: `crates/trondb-core/src/lib.rs:20-24` (EngineConfig)
- Modify: `crates/trondb-cli/src/main.rs:18-24` (config construction)

- [ ] **Step 1: Add snapshot_interval_secs to EngineConfig**

```rust
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub data_dir: PathBuf,
    pub wal: WalConfig,
    pub snapshot_interval_secs: u64,
}
```

- [ ] **Step 2: Update CLI config construction**

In `crates/trondb-cli/src/main.rs`, update config:
```rust
let config = EngineConfig {
    data_dir: data_dir.join("store"),
    wal: WalConfig {
        wal_dir: data_dir.join("wal"),
        ..Default::default()
    },
    snapshot_interval_secs: 60,
};
```

- [ ] **Step 3: Update all test_engine() helpers to include snapshot_interval_secs**

In `crates/trondb-core/src/lib.rs` tests, update `test_engine()` and `data_survives_restart` and `wal_replay_recovers_data`:
```rust
snapshot_interval_secs: 0, // disable in tests
```

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/lib.rs crates/trondb-cli/src/main.rs
git commit -m "feat(config): add snapshot_interval_secs to EngineConfig"
```

---

### Task 7: Background snapshot task

**Files:**
- Modify: `crates/trondb-core/src/lib.rs` (Engine gets snapshot task handle, spawn on open)

- [ ] **Step 1: Add snapshot file writing and background task**

In Engine::open, after creating the executor, spawn a background Tokio task (if snapshot_interval_secs > 0):

```rust
pub struct Engine {
    executor: Executor,
    _snapshot_handle: Option<tokio::task::JoinHandle<()>>,
}

// In Engine::open, after creating executor:
        let snapshot_handle = if config.snapshot_interval_secs > 0 {
            let interval = std::time::Duration::from_secs(config.snapshot_interval_secs);
            let snap_path = config.data_dir.join("location_table.snap");
            let location = executor.location().clone_for_snapshot();
            let wal_head = executor.wal_head_lsn();

            // We need a way for the task to read current state.
            // Since LocationTable is behind shared references, we'll pass an Arc.
            // This requires making Executor hold Arc<LocationTable>.
            todo!("see step 2")
        } else {
            None
        };
```

Actually, the simpler approach: make LocationTable `Clone`-able for snapshot is wrong (it's DashMap). Instead, the Engine should hold an `Arc<LocationTable>` and share it with the background task.

- [ ] **Step 2: Refactor Executor to use Arc\<LocationTable\>**

Change `Executor.location` from `LocationTable` to `Arc<LocationTable>`:

```rust
use std::sync::Arc;

pub struct Executor {
    store: FjallStore,
    wal: WalWriter,
    location: Arc<LocationTable>,
}

impl Executor {
    pub fn new(store: FjallStore, wal: WalWriter, location: Arc<LocationTable>) -> Self {
        Self { store, wal, location }
    }

    pub fn location(&self) -> &LocationTable {
        &self.location
    }

    pub fn location_arc(&self) -> Arc<LocationTable> {
        Arc::clone(&self.location)
    }
}
```

Update Engine::open to use `Arc::new(location)`.

- [ ] **Step 3: Spawn background snapshot task**

```rust
        let snapshot_handle = if config.snapshot_interval_secs > 0 {
            let interval = std::time::Duration::from_secs(config.snapshot_interval_secs);
            let snap_path = config.data_dir.join("location_table.snap");
            let location_arc = executor.location_arc();
            let wal = executor.wal_head_lsn_fn();

            Some(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.tick().await; // skip first immediate tick
                loop {
                    ticker.tick().await;
                    // snapshot() reads DashMap (lockless) and serialises
                    let wal_lsn = 0; // placeholder — will wire to actual WAL head
                    match location_arc.snapshot(wal_lsn) {
                        Ok(bytes) => {
                            if let Err(e) = tokio::fs::write(&snap_path, &bytes).await {
                                eprintln!("Location Table snapshot failed: {e}");
                            }
                        }
                        Err(e) => {
                            eprintln!("Location Table snapshot failed: {e}");
                        }
                    }
                }
            }))
        } else {
            None
        };

        Ok(Self {
            executor,
            _snapshot_handle: snapshot_handle,
        })
```

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass. Background task is disabled in tests (interval = 0).

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/lib.rs crates/trondb-core/src/executor.rs
git commit -m "feat(location): add background snapshot task with configurable interval"
```

---

## Chunk 5: Integration Tests and CLAUDE.md Update

### Task 8: Integration tests for Location Table

**Files:**
- Modify: `crates/trondb-core/src/lib.rs` (add integration tests in `mod tests`)

- [ ] **Step 1: Add integration test — insert with vector populates Location Table**

```rust
    #[tokio::test]
    async fn insert_with_vector_creates_location_entry() {
        let (engine, _dir) = test_engine().await;

        engine
            .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'Test') VECTOR [0.1, 0.2, 0.3];",
            )
            .await
            .unwrap();

        // Check Location Table has an entry for v1 repr 0
        let loc = engine.executor.location();
        let key = crate::location::ReprKey {
            entity_id: crate::types::LogicalId::from_string("v1"),
            repr_index: 0,
        };
        let desc = loc.get(&key).expect("should have location entry");
        assert_eq!(desc.tier, crate::location::Tier::Fjall);
        assert_eq!(desc.state, crate::location::LocState::Clean);
        assert_eq!(desc.encoding, crate::location::Encoding::Float32);
    }

    #[tokio::test]
    async fn insert_without_vector_has_no_location_entry() {
        let (engine, _dir) = test_engine().await;

        engine
            .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
            .await
            .unwrap();

        engine
            .execute_tql("INSERT INTO venues (id, name) VALUES ('v1', 'Test');")
            .await
            .unwrap();

        let loc = engine.executor.location();
        assert_eq!(loc.len(), 0);
    }
```

Note: this requires making `executor` pub in Engine, or adding `pub fn location(&self) -> &LocationTable` to Engine. Add:

```rust
impl Engine {
    pub fn location(&self) -> &crate::location::LocationTable {
        self.executor.location()
    }
}
```

- [ ] **Step 2: Add integration test — Location Table survives snapshot + restart**

```rust
    #[tokio::test]
    async fn location_table_snapshot_and_restore() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        let config = EngineConfig {
            data_dir: data_dir.clone(),
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
                    "INSERT INTO venues (id, name) VALUES ('v1', 'Test') VECTOR [0.1, 0.2, 0.3];",
                )
                .await
                .unwrap();

            // Manually write snapshot
            let snap_bytes = engine.location().snapshot(engine.executor.wal.head_lsn()).unwrap();
            std::fs::write(data_dir.join("location_table.snap"), &snap_bytes).unwrap();
        }

        // Reopen — should restore from snapshot
        {
            let engine = Engine::open(config).await.unwrap();
            let key = crate::location::ReprKey {
                entity_id: crate::types::LogicalId::from_string("v1"),
                repr_index: 0,
            };
            assert!(engine.location().get(&key).is_some());
        }
    }
```

Note: this test needs access to `engine.executor.wal.head_lsn()`. Add a method to Engine:
```rust
    pub fn wal_head_lsn(&self) -> u64 {
        self.executor.wal_head_lsn()
    }
```
And to Executor:
```rust
    pub fn wal_head_lsn(&self) -> u64 {
        self.wal.head_lsn()
    }
```

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-core/src/lib.rs crates/trondb-core/src/executor.rs
git commit -m "test(location): add integration tests for Location Table insert, snapshot, restore"
```

---

### Task 9: Update CLAUDE.md and smoke test

**Files:**
- Modify: `CLAUDE.md`
- Modify: `tests/smoke.tql` (no changes needed — Location Table is invisible to TQL)

- [ ] **Step 1: Update CLAUDE.md**

Update CLAUDE.md to reflect Phase 3:

```markdown
# TronDB

Inference-first storage engine. Phase 3: Location Table (control fabric).

## Project Structure

- `crates/trondb-wal/` — Write-Ahead Log: record types (MessagePack), segment files, buffer, async writer, crash recovery
- `crates/trondb-core/` — Engine: types, Fjall-backed store, Location Table (DashMap), planner, async executor. Depends on trondb-wal + trondb-tql.
- `crates/trondb-tql/` — TQL parser (logos lexer + recursive descent). No engine dependency.
- `crates/trondb-cli/` — Interactive REPL binary (Tokio + rustyline). Depends on all crates.

## Conventions

- Rust 2021 edition, async (Tokio runtime)
- Tests: `cargo test --workspace`
- Run REPL: `cargo run -p trondb-cli`
- Run REPL with custom data dir: `cargo run -p trondb-cli -- --data-dir /path/to/data`
- TQL is case-insensitive for keywords, case-sensitive for identifiers
- All vectors are Float32 (gated until Phase 4)
- LogicalId is a String wrapper (user-provided or UUID v4)
- Persistence: Fjall (LSM-based). Data dir default: ./trondb_data
- WAL: MessagePack records, CRC32 verified, segment files. Dir: {data_dir}/wal/
- Write path: WAL append → flush+fsync → apply to Fjall + Location Table → ack
- Location Table: DashMap-backed control fabric, always in RAM
  - Tracks LocationDescriptor per representation (tier, state, encoding, node address)
  - WAL-logged via RecordType::LocationUpdate (0x40)
  - Periodically snapshotted to {data_dir}/location_table.snap
  - Restored from snapshot + WAL replay on startup
  - State machine: Clean → Dirty → Recomputing → Clean; Clean → Migrating → Clean
- SEARCH is gated (returns UnsupportedOperation) until Phase 4
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: update CLAUDE.md for Phase 3 — Location Table"
```
