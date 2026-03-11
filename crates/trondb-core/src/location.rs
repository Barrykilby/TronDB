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
}
