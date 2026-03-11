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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecayConfig {
    pub decay_fn: Option<DecayFn>,
    pub decay_rate: Option<f64>,
    pub floor: Option<f64>,
    pub promote_threshold: Option<f64>,
    pub prune_threshold: Option<f64>,
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self {
            decay_fn: None,
            decay_rate: None,
            floor: None,
            promote_threshold: None,
            prune_threshold: None,
        }
    }
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
}

pub struct AdjacencyIndex {
    forward: DashMap<(LogicalId, String), Vec<AdjEntry>>,
}

impl AdjacencyIndex {
    pub fn new() -> Self {
        Self {
            forward: DashMap::new(),
        }
    }

    pub fn insert(&self, from_id: &LogicalId, edge_type: &str, to_id: &LogicalId, confidence: f32) {
        let key = (from_id.clone(), edge_type.to_string());
        let entry = AdjEntry {
            to_id: to_id.clone(),
            confidence,
        };
        self.forward
            .entry(key)
            .or_insert_with(Vec::new)
            .push(entry);
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
    }

    pub fn get(&self, from_id: &LogicalId, edge_type: &str) -> Vec<AdjEntry> {
        let key = (from_id.clone(), edge_type.to_string());
        self.forward
            .get(&key)
            .map(|v| v.clone())
            .unwrap_or_default()
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
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0);
        idx.insert(&make_id("v1"), "knows", &make_id("v3"), 1.0);

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
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0);
        idx.insert(&make_id("v1"), "knows", &make_id("v3"), 1.0);

        idx.remove(&make_id("v1"), "knows", &make_id("v2"));
        let results = idx.get(&make_id("v1"), "knows");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].to_id, make_id("v3"));
    }

    #[test]
    fn remove_last_edge_cleans_key() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0);
        idx.remove(&make_id("v1"), "knows", &make_id("v2"));
        assert!(idx.is_empty());
    }

    #[test]
    fn different_edge_types_separate() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0);
        idx.insert(&make_id("v1"), "likes", &make_id("v3"), 0.8);

        assert_eq!(idx.get(&make_id("v1"), "knows").len(), 1);
        assert_eq!(idx.get(&make_id("v1"), "likes").len(), 1);
    }

    #[test]
    fn len_counts_all_edges() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0);
        idx.insert(&make_id("v1"), "knows", &make_id("v3"), 1.0);
        idx.insert(&make_id("v2"), "likes", &make_id("v1"), 0.5);
        assert_eq!(idx.len(), 3);
    }
}
