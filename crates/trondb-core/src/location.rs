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

impl Default for LocationTable {
    fn default() -> Self {
        Self::new()
    }
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
        let mut entry = self.entity_reprs.entry(key.entity_id).or_default();
        let indices = entry.value_mut();
        if !indices.contains(&key.repr_index) {
            indices.push(key.repr_index);
            indices.sort_unstable();
        }

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

    /// Snapshot the Location Table to bytes.
    /// The caller provides the current WAL head LSN.
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
        out.extend_from_slice(b"TRONLOC1");
        out.extend_from_slice(&wal_head_lsn.to_be_bytes());
        out.extend_from_slice(&count.to_be_bytes());
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
}

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
        assert_eq!(lt.len(), 1);
    }

    #[test]
    fn get_entity_returns_all_reprs() {
        let lt = LocationTable::new();
        let eid = LogicalId::from_string("e1");
        lt.register(make_key("e1", 0), make_descriptor());
        lt.register(make_key("e1", 1), make_descriptor());
        lt.register(make_key("e2", 0), make_descriptor());

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
        assert!(lt.get(&make_key("e2", 0)).is_some());
        assert_eq!(lt.len(), 1);
    }

    #[test]
    fn valid_state_transitions() {
        let lt = LocationTable::new();
        let key = make_key("e1", 0);
        lt.register(key.clone(), make_descriptor());

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
        lt.register(key.clone(), make_descriptor());

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
        assert_eq!(desc.version, 2);
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
        lt.register(key.clone(), make_descriptor());

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
}
