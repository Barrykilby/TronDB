use std::collections::HashMap;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::types::{LogicalId, Value};

// ---------------------------------------------------------------------------
// Edge — a directional relationship between two entities
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from_id: LogicalId,
    pub to_id: LogicalId,
    pub edge_type: String,
    pub confidence: f32,
    pub metadata: HashMap<String, Value>,
    #[serde(default)]
    pub created_at: u64,
}

// ---------------------------------------------------------------------------
// EdgeType — schema declaration for a class of edges
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeType {
    pub name: String,
    pub from_collection: String,
    pub to_collection: String,
    pub decay_config: DecayConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecayConfig {
    pub decay_fn: Option<DecayFn>,
    pub decay_rate: Option<f64>,
    pub floor: Option<f64>,
    pub promote_threshold: Option<f64>,
    pub prune_threshold: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DecayFn {
    Exponential,
    Linear,
    Step,
}

// ---------------------------------------------------------------------------
// AdjacencyIndex — RAM index for fast TRAVERSE
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AdjEntry {
    pub to_id: LogicalId,
    pub confidence: f32,
    pub created_at: u64,
}

pub struct AdjacencyIndex {
    forward: DashMap<(LogicalId, String), Vec<AdjEntry>>,
    backward: DashMap<(LogicalId, String), Vec<LogicalId>>,
}

impl AdjacencyIndex {
    pub fn new() -> Self {
        Self {
            forward: DashMap::new(),
            backward: DashMap::new(),
        }
    }

    pub fn insert(&self, from_id: &LogicalId, edge_type: &str, to_id: &LogicalId, confidence: f32, created_at: u64) {
        let key = (from_id.clone(), edge_type.to_string());
        let entry = AdjEntry {
            to_id: to_id.clone(),
            confidence,
            created_at,
        };
        self.forward
            .entry(key)
            .or_default()
            .push(entry);

        // Maintain backward index: (to_id, edge_type) → from_id
        let bkey = (to_id.clone(), edge_type.to_string());
        self.backward
            .entry(bkey)
            .or_default()
            .push(from_id.clone());
    }

    pub fn remove(&self, from_id: &LogicalId, edge_type: &str, to_id: &LogicalId) {
        let key = (from_id.clone(), edge_type.to_string());
        if let Some(mut entries) = self.forward.get_mut(&key) {
            entries.retain(|e| e.to_id != *to_id);
            if entries.is_empty() {
                drop(entries);
                self.forward.remove(&key);
            }
        }

        // Clean backward index
        let bkey = (to_id.clone(), edge_type.to_string());
        if let Some(mut sources) = self.backward.get_mut(&bkey) {
            sources.retain(|id| id != from_id);
            if sources.is_empty() {
                drop(sources);
                self.backward.remove(&bkey);
            }
        }
    }

    pub fn get(&self, from_id: &LogicalId, edge_type: &str) -> Vec<AdjEntry> {
        let key = (from_id.clone(), edge_type.to_string());
        self.forward
            .get(&key)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// Returns all source entity IDs that have an edge of `edge_type` pointing TO `to_id`.
    pub fn get_backward(&self, to_id: &LogicalId, edge_type: &str) -> Vec<LogicalId> {
        let key = (to_id.clone(), edge_type.to_string());
        self.backward
            .get(&key)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// Returns all edges involving `entity_id` as either source or target.
    ///
    /// Returns `(forward_edges, backward_edges)` where:
    /// - `forward_edges`: `(edge_type, to_id)` pairs where `entity_id` is the source
    /// - `backward_edges`: `(edge_type, from_id)` pairs where `entity_id` is the target
    pub fn edges_involving(&self, entity_id: &LogicalId) -> (Vec<(String, LogicalId)>, Vec<(String, LogicalId)>) {
        // Forward: all (edge_type, to_id) pairs where entity_id is the source
        let mut forward_edges = Vec::new();
        for entry in self.forward.iter() {
            let (from_id, edge_type) = entry.key();
            if from_id == entity_id {
                for adj in entry.value() {
                    forward_edges.push((edge_type.clone(), adj.to_id.clone()));
                }
            }
        }

        // Backward: all (edge_type, from_id) pairs where entity_id is the target
        let mut backward_edges = Vec::new();
        for entry in self.backward.iter() {
            let (to_id, edge_type) = entry.key();
            if to_id == entity_id {
                for from_id in entry.value() {
                    backward_edges.push((edge_type.clone(), from_id.clone()));
                }
            }
        }

        (forward_edges, backward_edges)
    }

    pub fn len(&self) -> usize {
        self.forward.iter().map(|e| e.value().len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }
}

impl Default for AdjacencyIndex {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_id(s: &str) -> LogicalId {
        LogicalId::from_string(s)
    }

    #[test]
    fn insert_and_get() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0);
        idx.insert(&make_id("v1"), "knows", &make_id("v3"), 1.0, 0);

        let results = idx.get(&make_id("v1"), "knows");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn get_empty() {
        let idx = AdjacencyIndex::new();
        let results = idx.get(&make_id("v1"), "knows");
        assert!(results.is_empty());
    }

    #[test]
    fn remove_edge() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0);
        idx.insert(&make_id("v1"), "knows", &make_id("v3"), 1.0, 0);

        idx.remove(&make_id("v1"), "knows", &make_id("v2"));
        let results = idx.get(&make_id("v1"), "knows");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].to_id, make_id("v3"));
    }

    #[test]
    fn remove_last_edge_cleans_key() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0);
        idx.remove(&make_id("v1"), "knows", &make_id("v2"));
        assert!(idx.is_empty());
    }

    #[test]
    fn different_edge_types_separate() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0);
        idx.insert(&make_id("v1"), "likes", &make_id("v3"), 0.8, 0);

        assert_eq!(idx.get(&make_id("v1"), "knows").len(), 1);
        assert_eq!(idx.get(&make_id("v1"), "likes").len(), 1);
    }

    #[test]
    fn len_counts_all_edges() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0);
        idx.insert(&make_id("v1"), "knows", &make_id("v3"), 1.0, 0);
        idx.insert(&make_id("v2"), "likes", &make_id("v1"), 0.5, 0);
        assert_eq!(idx.len(), 3);
    }

    #[test]
    fn backward_index_populated_on_insert() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0);
        let backwards = idx.get_backward(&make_id("v2"), "knows");
        assert_eq!(backwards.len(), 1);
        assert_eq!(backwards[0], make_id("v1"));
    }

    #[test]
    fn backward_index_remove() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0);
        idx.remove(&make_id("v1"), "knows", &make_id("v2"));
        let backwards = idx.get_backward(&make_id("v2"), "knows");
        assert!(backwards.is_empty());
    }

    #[test]
    fn edges_involving_entity() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0);
        idx.insert(&make_id("v3"), "knows", &make_id("v1"), 1.0, 0);
        idx.insert(&make_id("v1"), "likes", &make_id("v4"), 1.0, 0);

        let (forward, backward) = idx.edges_involving(&make_id("v1"));
        assert_eq!(forward.len(), 2); // knows->v2, likes->v4
        assert_eq!(backward.len(), 1); // v3->knows->v1
    }

    #[test]
    fn edge_created_at_default_zero() {
        let edge = Edge {
            from_id: make_id("a"),
            to_id: make_id("b"),
            edge_type: "knows".into(),
            confidence: 1.0,
            metadata: HashMap::new(),
            created_at: 0,
        };
        let bytes = rmp_serde::to_vec_named(&edge).unwrap();
        let restored: Edge = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(restored.created_at, 0);
    }

    #[test]
    fn edge_deserialize_without_created_at() {
        // Simulate an old edge without created_at by serializing a struct
        // that lacks the field, then deserializing as Edge
        #[derive(Serialize)]
        struct OldEdge {
            from_id: LogicalId,
            to_id: LogicalId,
            edge_type: String,
            confidence: f32,
            metadata: HashMap<String, Value>,
        }
        let old = OldEdge {
            from_id: make_id("a"),
            to_id: make_id("b"),
            edge_type: "knows".into(),
            confidence: 1.0,
            metadata: HashMap::new(),
        };
        let bytes = rmp_serde::to_vec_named(&old).unwrap();
        let restored: Edge = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(restored.created_at, 0);
    }

    #[test]
    fn adj_entry_stores_created_at() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 42);
        let results = idx.get(&make_id("v1"), "knows");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].created_at, 42);
    }
}
