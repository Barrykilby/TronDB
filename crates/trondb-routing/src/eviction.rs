use std::collections::HashMap;
use std::time::Instant;
use crate::affinity::{AffinityIndex, AffinitySource};
use crate::node::EntityId;

pub struct LruTracker {
    access_times: HashMap<EntityId, Instant>,
}

impl LruTracker {
    pub fn new() -> Self {
        Self { access_times: HashMap::new() }
    }

    pub fn touch(&mut self, entity: &EntityId) {
        self.access_times.insert(entity.clone(), Instant::now());
    }

    pub fn last_access(&self, entity: &EntityId) -> Option<Instant> {
        self.access_times.get(entity).copied()
    }
}

impl Default for LruTracker {
    fn default() -> Self { Self::new() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EvictionPriority {
    Ungrouped = 1,
    ImplicitGroup = 2,
    ExplicitGroup = 3,
}

pub fn eviction_priority(entity: &EntityId, affinity: &AffinityIndex) -> EvictionPriority {
    match affinity.group_for(entity) {
        None => EvictionPriority::Ungrouped,
        Some(gid) => {
            match affinity.get_group(&gid) {
                Some(group) if group.source == AffinitySource::Learned => EvictionPriority::ImplicitGroup,
                _ => EvictionPriority::ExplicitGroup,
            }
        }
    }
}

pub fn select_eviction_candidates(
    hot_entities: &[EntityId],
    needed: usize,
    affinity: &AffinityIndex,
    lru: &LruTracker,
) -> Vec<EntityId> {
    if needed == 0 { return vec![]; }
    let mut candidates: Vec<(EntityId, EvictionPriority, Instant)> = hot_entities
        .iter()
        .map(|e| {
            let prio = eviction_priority(e, affinity);
            let last = lru.last_access(e).unwrap_or(Instant::now());
            (e.clone(), prio, last)
        })
        .collect();
    candidates.sort_by(|a, b| a.1.cmp(&b.1).then(a.2.cmp(&b.2)));
    candidates.into_iter().take(needed).map(|(e, _, _)| e).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::affinity::AffinityIndex;
    use crate::node::AffinityGroupId;
    use trondb_core::types::LogicalId;

    fn eid(s: &str) -> EntityId { LogicalId::from_string(s) }

    #[test]
    fn ungrouped_evicted_first() {
        let affinity = AffinityIndex::new();
        let gid = AffinityGroupId::from_string("g1");
        affinity.create_group(gid.clone()).unwrap();
        affinity.add_to_group(&eid("grouped"), &gid, 500).unwrap();
        let lru = LruTracker::new();
        let hot = vec![eid("ungrouped"), eid("grouped")];
        let evict = select_eviction_candidates(&hot, 1, &affinity, &lru);
        assert_eq!(evict, vec![eid("ungrouped")]);
    }

    #[test]
    fn lru_ordering_within_priority() {
        let affinity = AffinityIndex::new();
        let mut lru = LruTracker::new();
        lru.touch(&eid("older"));
        std::thread::sleep(std::time::Duration::from_millis(10));
        lru.touch(&eid("newer"));
        let hot = vec![eid("newer"), eid("older")];
        let evict = select_eviction_candidates(&hot, 1, &affinity, &lru);
        assert_eq!(evict, vec![eid("older")]);
    }

    #[test]
    fn eviction_priority_ordering() {
        assert!(EvictionPriority::Ungrouped < EvictionPriority::ImplicitGroup);
        assert!(EvictionPriority::ImplicitGroup < EvictionPriority::ExplicitGroup);
    }

    #[test]
    fn select_zero_needed_returns_empty() {
        let affinity = AffinityIndex::new();
        let lru = LruTracker::new();
        let evict = select_eviction_candidates(&[eid("a")], 0, &affinity, &lru);
        assert!(evict.is_empty());
    }
}
