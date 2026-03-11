use std::collections::HashSet;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use trondb_wal::{RecordType, WalWriter};

use crate::config::ColocationConfig;
use crate::error::RouterError;
use crate::node::{AffinityGroupId, EntityId, NodeId};

// ---------------------------------------------------------------------------
// WAL payload types (explicit groups only; implicit is RAM-only)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct AffinityGroupCreatePayload {
    pub group_id: String,
}

#[derive(Serialize, Deserialize)]
pub struct AffinityGroupMemberPayload {
    pub group_id: String,
    pub entity_id: String,
}

#[derive(Serialize, Deserialize)]
pub struct AffinityGroupRemovePayload {
    pub entity_id: String,
    pub group_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AffinitySource {
    Explicit,
    Learned,
}

#[derive(Debug, Clone)]
pub struct AffinityGroup {
    pub id: AffinityGroupId,
    pub members: HashSet<EntityId>,
    pub source: AffinitySource,
    pub target_node: Option<NodeId>,
    pub created_at: i64,
    pub last_seen: i64,
}

pub struct AffinityIndex {
    explicit: DashMap<EntityId, AffinityGroupId>,
    implicit: DashMap<(EntityId, EntityId), f32>,
    groups: DashMap<AffinityGroupId, AffinityGroup>,
}

impl AffinityIndex {
    pub fn new() -> Self {
        Self {
            explicit: DashMap::new(),
            implicit: DashMap::new(),
            groups: DashMap::new(),
        }
    }

    pub fn create_group(&self, id: AffinityGroupId) -> Result<(), RouterError> {
        if self.groups.contains_key(&id) {
            return Err(RouterError::AffinityGroupAlreadyExists(id.as_str().to_owned()));
        }
        let now = now_ms();
        self.groups.insert(
            id.clone(),
            AffinityGroup {
                id,
                members: HashSet::new(),
                source: AffinitySource::Explicit,
                target_node: None,
                created_at: now,
                last_seen: now,
            },
        );
        Ok(())
    }

    pub fn add_to_group(
        &self,
        entity: &EntityId,
        group_id: &AffinityGroupId,
        max_size: usize,
    ) -> Result<(), RouterError> {
        let mut group = self
            .groups
            .get_mut(group_id)
            .ok_or_else(|| RouterError::AffinityGroupNotFound(group_id.as_str().to_owned()))?;
        if group.members.len() >= max_size {
            return Err(RouterError::AffinityGroupFull(
                group_id.as_str().to_owned(),
                max_size,
            ));
        }
        group.members.insert(entity.clone());
        group.last_seen = now_ms();
        self.explicit.insert(entity.clone(), group_id.clone());
        Ok(())
    }

    pub fn remove_from_group(&self, entity: &EntityId) {
        if let Some((_, group_id)) = self.explicit.remove(entity) {
            if let Some(mut group) = self.groups.get_mut(&group_id) {
                group.members.remove(entity);
            }
        }
    }

    pub fn group_for(&self, entity: &EntityId) -> Option<AffinityGroupId> {
        self.explicit.get(entity).map(|r| r.clone())
    }

    pub fn get_group(&self, id: &AffinityGroupId) -> Option<AffinityGroup> {
        self.groups.get(id).map(|r| r.clone())
    }

    pub fn preferred_node(&self, entity: &EntityId) -> Option<NodeId> {
        let group_id = self.explicit.get(entity)?;
        let group = self.groups.get(&*group_id)?;
        group.target_node.clone()
    }

    // --- Co-occurrence tracking ---

    pub fn record_cooccurrence(&self, results: &[EntityId]) {
        if results.len() < 2 {
            return;
        }
        let increment = 1.0 / (results.len() - 1) as f32;
        for i in 0..results.len() {
            for j in (i + 1)..results.len() {
                let key = canonical_pair(&results[i], &results[j]);
                self.implicit
                    .entry(key)
                    .and_modify(|s| *s += increment)
                    .or_insert(increment);
            }
        }
    }

    pub fn cooccurrence_score(&self, a: &EntityId, b: &EntityId) -> f32 {
        let key = canonical_pair(a, b);
        self.implicit.get(&key).map(|r| *r).unwrap_or(0.0)
    }

    pub fn promote_and_decay(&self, cfg: &ColocationConfig) {
        let mut to_promote = Vec::new();
        for entry in self.implicit.iter() {
            if *entry.value() >= cfg.learn_threshold {
                to_promote.push(entry.key().clone());
            }
        }
        for (a, b) in to_promote {
            self.create_implicit_group(&a, &b, cfg.max_group_size);
        }
        for mut entry in self.implicit.iter_mut() {
            *entry.value_mut() *= cfg.decay_factor;
        }
        self.implicit.retain(|_, score| *score >= 0.01);
    }

    fn create_implicit_group(&self, a: &EntityId, b: &EntityId, max_size: usize) {
        let a_group = self.explicit.get(a).map(|r| r.clone());
        let b_group = self.explicit.get(b).map(|r| r.clone());

        match (a_group, b_group) {
            (Some(_), Some(_)) => {}
            (Some(gid), None) => {
                if let Some(mut g) = self.groups.get_mut(&gid) {
                    if g.members.len() < max_size {
                        g.members.insert(b.clone());
                        self.explicit.insert(b.clone(), gid);
                    }
                }
            }
            (None, Some(gid)) => {
                if let Some(mut g) = self.groups.get_mut(&gid) {
                    if g.members.len() < max_size {
                        g.members.insert(a.clone());
                        self.explicit.insert(a.clone(), gid);
                    }
                }
            }
            (None, None) => {
                let gid = AffinityGroupId::from_string(&format!(
                    "learned_{}_{}",
                    a.as_str(),
                    b.as_str()
                ));
                let now = now_ms();
                let mut members = HashSet::new();
                members.insert(a.clone());
                members.insert(b.clone());
                self.groups.insert(
                    gid.clone(),
                    AffinityGroup {
                        id: gid.clone(),
                        members,
                        source: AffinitySource::Learned,
                        target_node: None,
                        created_at: now,
                        last_seen: now,
                    },
                );
                self.explicit.insert(a.clone(), gid.clone());
                self.explicit.insert(b.clone(), gid);
            }
        }
    }

    pub fn implicit_count(&self) -> usize {
        self.implicit.len()
    }

    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    // -----------------------------------------------------------------------
    // WAL-logged mutations (explicit groups only)
    // -----------------------------------------------------------------------

    pub async fn create_group_logged(
        &self,
        id: AffinityGroupId,
        wal: &WalWriter,
    ) -> Result<(), RouterError> {
        self.create_group(id.clone())?;
        let payload = rmp_serde::to_vec_named(&AffinityGroupCreatePayload {
            group_id: id.as_str().to_owned(),
        })
        .unwrap();
        let tx = wal.next_tx_id();
        wal.append(RecordType::TxBegin, "affinity", tx, 1, vec![]);
        wal.append(RecordType::AffinityGroupCreate, "affinity", tx, 1, payload);
        wal.commit(tx)
            .await
            .map_err(|e| RouterError::Engine(e.into()))?;
        Ok(())
    }

    pub async fn add_to_group_logged(
        &self,
        entity: &EntityId,
        group_id: &AffinityGroupId,
        max_size: usize,
        wal: &WalWriter,
    ) -> Result<(), RouterError> {
        self.add_to_group(entity, group_id, max_size)?;
        let payload = rmp_serde::to_vec_named(&AffinityGroupMemberPayload {
            group_id: group_id.as_str().to_owned(),
            entity_id: entity.as_str().to_owned(),
        })
        .unwrap();
        let tx = wal.next_tx_id();
        wal.append(RecordType::TxBegin, "affinity", tx, 1, vec![]);
        wal.append(RecordType::AffinityGroupMember, "affinity", tx, 1, payload);
        wal.commit(tx)
            .await
            .map_err(|e| RouterError::Engine(e.into()))?;
        Ok(())
    }

    pub async fn remove_from_group_logged(
        &self,
        entity: &EntityId,
        wal: &WalWriter,
    ) -> Result<(), RouterError> {
        let group_id = self.group_for(entity);
        self.remove_from_group(entity);
        if let Some(gid) = group_id {
            let payload = rmp_serde::to_vec_named(&AffinityGroupRemovePayload {
                entity_id: entity.as_str().to_owned(),
                group_id: gid.as_str().to_owned(),
            })
            .unwrap();
            let tx = wal.next_tx_id();
            wal.append(RecordType::TxBegin, "affinity", tx, 1, vec![]);
            wal.append(RecordType::AffinityGroupRemove, "affinity", tx, 1, payload);
            wal.commit(tx)
                .await
                .map_err(|e| RouterError::Engine(e.into()))?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // WAL replay (called during startup recovery)
    // -----------------------------------------------------------------------

    pub fn replay_affinity_record(
        &self,
        record_type: RecordType,
        payload: &[u8],
        max_group_size: usize,
    ) {
        match record_type {
            RecordType::AffinityGroupCreate => {
                if let Ok(p) =
                    rmp_serde::from_slice::<AffinityGroupCreatePayload>(payload)
                {
                    let _ = self.create_group(AffinityGroupId::from_string(&p.group_id));
                }
            }
            RecordType::AffinityGroupMember => {
                if let Ok(p) =
                    rmp_serde::from_slice::<AffinityGroupMemberPayload>(payload)
                {
                    let gid = AffinityGroupId::from_string(&p.group_id);
                    let eid = EntityId::from_string(&p.entity_id);
                    let _ = self.add_to_group(&eid, &gid, max_group_size);
                }
            }
            RecordType::AffinityGroupRemove => {
                if let Ok(p) =
                    rmp_serde::from_slice::<AffinityGroupRemovePayload>(payload)
                {
                    let eid = EntityId::from_string(&p.entity_id);
                    self.remove_from_group(&eid);
                }
            }
            _ => {}
        }
    }
}

impl Default for AffinityIndex {
    fn default() -> Self {
        Self::new()
    }
}

fn canonical_pair(a: &EntityId, b: &EntityId) -> (EntityId, EntityId) {
    if a.as_str() <= b.as_str() {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use trondb_core::types::LogicalId;

    fn eid(s: &str) -> EntityId {
        LogicalId::from_string(s)
    }

    #[test]
    fn create_and_get_group() {
        let idx = AffinityIndex::new();
        let gid = AffinityGroupId::from_string("g1");
        idx.create_group(gid.clone()).unwrap();
        let group = idx.get_group(&gid).unwrap();
        assert_eq!(group.source, AffinitySource::Explicit);
        assert!(group.members.is_empty());
    }

    #[test]
    fn duplicate_group_errors() {
        let idx = AffinityIndex::new();
        let gid = AffinityGroupId::from_string("g1");
        idx.create_group(gid.clone()).unwrap();
        assert!(idx.create_group(gid).is_err());
    }

    #[test]
    fn add_and_remove_member() {
        let idx = AffinityIndex::new();
        let gid = AffinityGroupId::from_string("g1");
        idx.create_group(gid.clone()).unwrap();
        idx.add_to_group(&eid("e1"), &gid, 500).unwrap();
        assert_eq!(idx.group_for(&eid("e1")), Some(gid.clone()));
        let group = idx.get_group(&gid).unwrap();
        assert!(group.members.contains(&eid("e1")));
        idx.remove_from_group(&eid("e1"));
        assert_eq!(idx.group_for(&eid("e1")), None);
    }

    #[test]
    fn group_full_error() {
        let idx = AffinityIndex::new();
        let gid = AffinityGroupId::from_string("g1");
        idx.create_group(gid.clone()).unwrap();
        idx.add_to_group(&eid("e1"), &gid, 2).unwrap();
        idx.add_to_group(&eid("e2"), &gid, 2).unwrap();
        let err = idx.add_to_group(&eid("e3"), &gid, 2);
        assert!(matches!(err, Err(RouterError::AffinityGroupFull(_, 2))));
    }

    #[test]
    fn canonical_pair_ordering() {
        let (a, b) = canonical_pair(&eid("b"), &eid("a"));
        assert_eq!(a, eid("a"));
        assert_eq!(b, eid("b"));
    }

    #[test]
    fn cooccurrence_symmetric() {
        let idx = AffinityIndex::new();
        idx.record_cooccurrence(&[eid("a"), eid("b"), eid("c")]);
        assert!((idx.cooccurrence_score(&eid("a"), &eid("b")) - 0.5).abs() < 1e-6);
        assert!((idx.cooccurrence_score(&eid("b"), &eid("a")) - 0.5).abs() < 1e-6);
        assert!((idx.cooccurrence_score(&eid("a"), &eid("c")) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn cooccurrence_accumulates() {
        let idx = AffinityIndex::new();
        idx.record_cooccurrence(&[eid("x"), eid("y")]);
        idx.record_cooccurrence(&[eid("x"), eid("y")]);
        assert!((idx.cooccurrence_score(&eid("x"), &eid("y")) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn promote_and_decay_creates_learned_group() {
        let idx = AffinityIndex::new();
        let cfg = ColocationConfig {
            learn_threshold: 0.70,
            decay_factor: 0.95,
            max_group_size: 500,
            ..ColocationConfig::default()
        };
        for _ in 0..5 {
            idx.record_cooccurrence(&[eid("p"), eid("q")]);
        }
        idx.promote_and_decay(&cfg);
        assert!(idx.group_for(&eid("p")).is_some());
        assert!(idx.group_for(&eid("q")).is_some());
        assert_eq!(idx.group_for(&eid("p")), idx.group_for(&eid("q")));
    }

    #[test]
    fn decay_prunes_low_scores() {
        let idx = AffinityIndex::new();
        let cfg = ColocationConfig {
            learn_threshold: 10.0,
            decay_factor: 0.001,
            ..ColocationConfig::default()
        };
        idx.record_cooccurrence(&[eid("a"), eid("b")]);
        assert_eq!(idx.implicit_count(), 1);
        idx.promote_and_decay(&cfg);
        assert_eq!(idx.implicit_count(), 0);
    }

    #[test]
    fn preferred_node_returns_group_target() {
        let idx = AffinityIndex::new();
        let gid = AffinityGroupId::from_string("g1");
        idx.create_group(gid.clone()).unwrap();
        idx.add_to_group(&eid("e1"), &gid, 500).unwrap();
        assert_eq!(idx.preferred_node(&eid("e1")), None);
        idx.groups.get_mut(&gid).unwrap().target_node =
            Some(crate::node::NodeId::from_string("node-1"));
        assert_eq!(
            idx.preferred_node(&eid("e1")),
            Some(crate::node::NodeId::from_string("node-1"))
        );
    }

    #[test]
    fn affinity_group_wal_replay() {
        use trondb_wal::RecordType;

        let idx = AffinityIndex::new();
        let gid = AffinityGroupId::from_string("g1");

        // Simulate WAL replay: create group
        let create_payload = rmp_serde::to_vec_named(&AffinityGroupCreatePayload {
            group_id: "g1".into(),
        })
        .unwrap();
        idx.replay_affinity_record(RecordType::AffinityGroupCreate, &create_payload, 500);
        assert!(idx.get_group(&gid).is_some());

        // Simulate WAL replay: add member
        let member_payload = rmp_serde::to_vec_named(&AffinityGroupMemberPayload {
            group_id: "g1".into(),
            entity_id: "e1".into(),
        })
        .unwrap();
        idx.replay_affinity_record(RecordType::AffinityGroupMember, &member_payload, 500);
        assert_eq!(idx.group_for(&eid("e1")), Some(gid.clone()));

        // Simulate WAL replay: remove member
        let remove_payload = rmp_serde::to_vec_named(&AffinityGroupRemovePayload {
            entity_id: "e1".into(),
            group_id: "g1".into(),
        })
        .unwrap();
        idx.replay_affinity_record(RecordType::AffinityGroupRemove, &remove_payload, 500);
        assert_eq!(idx.group_for(&eid("e1")), None);
    }
}
