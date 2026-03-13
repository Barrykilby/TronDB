use std::collections::HashMap;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::types::{LogicalId, Value};

/// A list of `(edge_type, entity_id)` pairs returned by edge queries.
pub type EdgeList = Vec<(String, LogicalId)>;

// ---------------------------------------------------------------------------
// EdgeSource — provenance of an edge
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeSource {
    Structural,
    Inferred,
    Confirmed,
}

impl Default for EdgeSource {
    fn default() -> Self {
        EdgeSource::Structural
    }
}

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
    #[serde(default)]
    pub source: EdgeSource,
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
    #[serde(default)]
    pub inference_config: InferenceConfig,
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
// InferenceConfig — controls background inference for an edge type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceConfig {
    pub auto: bool,
    pub confidence_floor: f32,
    pub limit: usize,
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            auto: false,
            confidence_floor: 0.5,
            limit: 10,
        }
    }
}

// ---------------------------------------------------------------------------
// Decay computation
// ---------------------------------------------------------------------------

/// Compute effective confidence after decay.
/// `base_confidence`: original confidence (typically 1.0)
/// `elapsed_millis`: time since edge creation in milliseconds
/// `config`: the edge type's decay configuration
pub fn effective_confidence(base_confidence: f32, elapsed_millis: u64, config: &DecayConfig) -> f32 {
    if elapsed_millis == 0 || config.decay_fn.is_none() {
        return base_confidence;
    }

    let elapsed_secs = elapsed_millis as f64 / 1000.0;
    let rate = config.decay_rate.unwrap_or(0.0);
    let floor = config.floor.unwrap_or(0.0);

    let decayed = match config.decay_fn.as_ref().unwrap() {
        DecayFn::Exponential => (base_confidence as f64) * (-rate * elapsed_secs).exp(),
        DecayFn::Linear => (base_confidence as f64) * (1.0 - rate * elapsed_secs),
        DecayFn::Step => {
            if elapsed_secs > rate {
                floor
            } else {
                base_confidence as f64
            }
        }
    };

    decayed.max(floor).min(1.0) as f32
}

// ---------------------------------------------------------------------------
// AdjacencyIndex — RAM index for fast TRAVERSE
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AdjEntry {
    pub to_id: LogicalId,
    pub confidence: f32,
    pub created_at: u64,
    pub source: EdgeSource,
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

    pub fn insert(&self, from_id: &LogicalId, edge_type: &str, to_id: &LogicalId, confidence: f32, created_at: u64, source: EdgeSource) {
        let key = (from_id.clone(), edge_type.to_string());
        let entry = AdjEntry {
            to_id: to_id.clone(),
            confidence,
            created_at,
            source,
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
    pub fn edges_involving(&self, entity_id: &LogicalId) -> (EdgeList, EdgeList) {
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

    /// Remove all adjacency entries for a given edge type.
    pub fn remove_edge_type(&self, edge_type: &str) {
        self.forward.retain(|k, _| k.1 != edge_type);
        self.backward.retain(|k, _| k.1 != edge_type);
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
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0, EdgeSource::Structural);
        idx.insert(&make_id("v1"), "knows", &make_id("v3"), 1.0, 0, EdgeSource::Structural);

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
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0, EdgeSource::Structural);
        idx.insert(&make_id("v1"), "knows", &make_id("v3"), 1.0, 0, EdgeSource::Structural);

        idx.remove(&make_id("v1"), "knows", &make_id("v2"));
        let results = idx.get(&make_id("v1"), "knows");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].to_id, make_id("v3"));
    }

    #[test]
    fn remove_last_edge_cleans_key() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0, EdgeSource::Structural);
        idx.remove(&make_id("v1"), "knows", &make_id("v2"));
        assert!(idx.is_empty());
    }

    #[test]
    fn different_edge_types_separate() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0, EdgeSource::Structural);
        idx.insert(&make_id("v1"), "likes", &make_id("v3"), 0.8, 0, EdgeSource::Structural);

        assert_eq!(idx.get(&make_id("v1"), "knows").len(), 1);
        assert_eq!(idx.get(&make_id("v1"), "likes").len(), 1);
    }

    #[test]
    fn len_counts_all_edges() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0, EdgeSource::Structural);
        idx.insert(&make_id("v1"), "knows", &make_id("v3"), 1.0, 0, EdgeSource::Structural);
        idx.insert(&make_id("v2"), "likes", &make_id("v1"), 0.5, 0, EdgeSource::Structural);
        assert_eq!(idx.len(), 3);
    }

    #[test]
    fn backward_index_populated_on_insert() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0, EdgeSource::Structural);
        let backwards = idx.get_backward(&make_id("v2"), "knows");
        assert_eq!(backwards.len(), 1);
        assert_eq!(backwards[0], make_id("v1"));
    }

    #[test]
    fn backward_index_remove() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0, EdgeSource::Structural);
        idx.remove(&make_id("v1"), "knows", &make_id("v2"));
        let backwards = idx.get_backward(&make_id("v2"), "knows");
        assert!(backwards.is_empty());
    }

    #[test]
    fn edges_involving_entity() {
        let idx = AdjacencyIndex::new();
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 0, EdgeSource::Structural);
        idx.insert(&make_id("v3"), "knows", &make_id("v1"), 1.0, 0, EdgeSource::Structural);
        idx.insert(&make_id("v1"), "likes", &make_id("v4"), 1.0, 0, EdgeSource::Structural);

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
            source: EdgeSource::Structural,
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
        idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0, 42, EdgeSource::Structural);
        let results = idx.get(&make_id("v1"), "knows");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].created_at, 42);
    }

    // -----------------------------------------------------------------------
    // Decay function tests
    // -----------------------------------------------------------------------

    #[test]
    fn exponential_decay() {
        let config = DecayConfig {
            decay_fn: Some(DecayFn::Exponential),
            decay_rate: Some(0.001),
            floor: Some(0.1),
            promote_threshold: None,
            prune_threshold: Some(0.05),
        };
        // After 1000 seconds: e^(-0.001 * 1000) = e^(-1) ≈ 0.368
        let result = effective_confidence(1.0, 1_000_000, &config);
        assert!((result - 0.368).abs() < 0.01);
    }

    #[test]
    fn linear_decay() {
        let config = DecayConfig {
            decay_fn: Some(DecayFn::Linear),
            decay_rate: Some(0.001),
            floor: Some(0.1),
            promote_threshold: None,
            prune_threshold: None,
        };
        // After 500 seconds: 1.0 * (1 - 0.001 * 500) = 0.5
        let result = effective_confidence(1.0, 500_000, &config);
        assert!((result - 0.5).abs() < 0.01);
    }

    #[test]
    fn linear_decay_respects_floor() {
        let config = DecayConfig {
            decay_fn: Some(DecayFn::Linear),
            decay_rate: Some(0.01),
            floor: Some(0.2),
            promote_threshold: None,
            prune_threshold: None,
        };
        let result = effective_confidence(1.0, 1_000_000, &config);
        assert!((result - 0.2).abs() < 0.01);
    }

    #[test]
    fn step_decay() {
        let config = DecayConfig {
            decay_fn: Some(DecayFn::Step),
            decay_rate: Some(3600.0), // threshold: 3600 seconds
            floor: Some(0.1),
            promote_threshold: None,
            prune_threshold: None,
        };
        assert!((effective_confidence(1.0, 1_000_000, &config) - 1.0).abs() < 0.01); // Before threshold
        assert!((effective_confidence(1.0, 5_000_000, &config) - 0.1).abs() < 0.01); // After threshold
    }

    #[test]
    fn no_decay_config_returns_original() {
        let config = DecayConfig::default();
        let result = effective_confidence(1.0, 999_999_999, &config);
        assert!((result - 1.0).abs() < 0.001);
    }

    #[test]
    fn zero_elapsed_no_decay() {
        let config = DecayConfig {
            decay_fn: Some(DecayFn::Exponential),
            decay_rate: Some(0.1),
            floor: Some(0.0),
            promote_threshold: None,
            prune_threshold: None,
        };
        let result = effective_confidence(1.0, 0, &config);
        assert!((result - 1.0).abs() < 0.001);
    }

    // -----------------------------------------------------------------------
    // EdgeSource tests
    // -----------------------------------------------------------------------

    #[test]
    fn edge_source_defaults_to_structural() {
        let source = EdgeSource::default();
        assert_eq!(source, EdgeSource::Structural);
    }

    #[test]
    fn edge_deserializes_without_source_field() {
        let json = r#"{"from_id":"a","to_id":"b","edge_type":"test","confidence":1.0,"metadata":{},"created_at":0}"#;
        let edge: Edge = serde_json::from_str(json).unwrap();
        assert_eq!(edge.source, EdgeSource::Structural);
    }

    #[test]
    fn edge_serializes_with_source_field() {
        let edge = Edge {
            from_id: make_id("a"),
            to_id: make_id("b"),
            edge_type: "test".into(),
            confidence: 0.8,
            metadata: HashMap::new(),
            created_at: 0,
            source: EdgeSource::Inferred,
        };
        let json = serde_json::to_string(&edge).unwrap();
        assert!(json.contains("Inferred"));
    }

    // -----------------------------------------------------------------------
    // InferenceConfig tests
    // -----------------------------------------------------------------------

    #[test]
    fn edge_type_deserializes_without_inference_config() {
        let json = r#"{"name":"test","from_collection":"a","to_collection":"b","decay_config":{"decay_fn":null,"decay_rate":null,"floor":null,"promote_threshold":null,"prune_threshold":null}}"#;
        let et: EdgeType = serde_json::from_str(json).unwrap();
        assert!(!et.inference_config.auto);
        assert_eq!(et.inference_config.confidence_floor, 0.5);
        assert_eq!(et.inference_config.limit, 10);
    }

    #[test]
    fn inference_config_defaults_sensible() {
        let config = InferenceConfig::default();
        assert!(!config.auto);
        assert_eq!(config.confidence_floor, 0.5);
        assert_eq!(config.limit, 10);
    }
}
