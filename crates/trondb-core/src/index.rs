use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anndists::dist::DistCosine;
use dashmap::DashMap;
use hnsw_rs::hnsw::{Hnsw, Neighbour};

use crate::types::LogicalId;

// ---------------------------------------------------------------------------
// HNSW parameters — sensible defaults for single-node
// ---------------------------------------------------------------------------

const MAX_NB_CONNECTION: usize = 16;
const MAX_ELEMENTS: usize = 100_000;
const MAX_LAYER: usize = 16;
const EF_CONSTRUCTION: usize = 200;
const EF_SEARCH: usize = 50;

// ---------------------------------------------------------------------------
// HnswIndex — one per collection
// ---------------------------------------------------------------------------

pub struct HnswIndex {
    inner: Mutex<Hnsw<'static, f32, DistCosine>>,
    dimensions: usize,
    id_to_idx: DashMap<LogicalId, usize>,
    idx_to_id: DashMap<usize, LogicalId>,
    next_idx: AtomicUsize,
}

impl HnswIndex {
    pub fn new(dimensions: usize) -> Self {
        let hnsw = Hnsw::new(
            MAX_NB_CONNECTION,
            MAX_ELEMENTS,
            MAX_LAYER,
            EF_CONSTRUCTION,
            DistCosine,
        );
        Self {
            inner: Mutex::new(hnsw),
            dimensions,
            id_to_idx: DashMap::new(),
            idx_to_id: DashMap::new(),
            next_idx: AtomicUsize::new(0),
        }
    }

    /// Insert a vector for an entity. Idempotent: skips if already indexed.
    pub fn insert(&self, id: &LogicalId, vector: &[f32]) {
        // Idempotent — skip if already in the index
        if self.id_to_idx.contains_key(id) {
            return;
        }

        let idx = self.next_idx.fetch_add(1, Ordering::SeqCst);
        self.id_to_idx.insert(id.clone(), idx);
        self.idx_to_id.insert(idx, id.clone());

        let hnsw = self.inner.lock().unwrap();
        hnsw.insert((vector, idx));
    }

    /// Search for k nearest neighbours.
    /// Returns (LogicalId, similarity_score) sorted by descending similarity.
    /// similarity = 1.0 - cosine_distance.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(LogicalId, f32)> {
        if self.is_empty() {
            return Vec::new();
        }

        let hnsw = self.inner.lock().unwrap();
        let neighbours: Vec<Neighbour> = hnsw.search(query, k, EF_SEARCH);

        let mut results: Vec<(LogicalId, f32)> = neighbours
            .into_iter()
            .filter_map(|n| {
                let idx = n.d_id;
                let similarity = 1.0 - n.distance;
                self.idx_to_id.get(&idx).map(|id| (id.clone(), similarity))
            })
            .collect();

        // Sort by descending similarity
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results
    }

    pub fn len(&self) -> usize {
        self.id_to_idx.len()
    }

    pub fn is_empty(&self) -> bool {
        self.id_to_idx.is_empty()
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
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
    fn insert_and_search() {
        let idx = HnswIndex::new(3);
        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        idx.insert(&make_id("e2"), &[0.0, 1.0, 0.0]);
        idx.insert(&make_id("e3"), &[0.9, 0.1, 0.0]);

        let results = idx.search(&[1.0, 0.0, 0.0], 3);
        assert_eq!(results.len(), 3);
        // e1 should be the closest match (exact vector)
        assert_eq!(results[0].0, make_id("e1"));
        // similarity should be close to 1.0
        assert!(results[0].1 > 0.99);
    }

    #[test]
    fn search_empty_index() {
        let idx = HnswIndex::new(3);
        let results = idx.search(&[1.0, 0.0, 0.0], 5);
        assert!(results.is_empty());
    }

    #[test]
    fn idempotent_insert() {
        let idx = HnswIndex::new(3);
        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]); // duplicate
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn similarity_score_range() {
        let idx = HnswIndex::new(3);
        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        idx.insert(&make_id("e2"), &[0.0, 1.0, 0.0]); // orthogonal

        let results = idx.search(&[1.0, 0.0, 0.0], 2);
        assert_eq!(results.len(), 2);
        // First result (e1) should have high similarity
        assert!(results[0].1 > 0.9);
        // Second result (e2) should have low similarity (orthogonal ≈ 0.0)
        assert!(results[1].1 < 0.1);
    }

    #[test]
    fn results_sorted_descending() {
        let idx = HnswIndex::new(3);
        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        idx.insert(&make_id("e2"), &[0.5, 0.5, 0.0]);
        idx.insert(&make_id("e3"), &[0.0, 1.0, 0.0]);

        let results = idx.search(&[1.0, 0.0, 0.0], 3);
        for w in results.windows(2) {
            assert!(w[0].1 >= w[1].1, "results should be sorted by descending similarity");
        }
    }

    #[test]
    fn len_and_dimensions() {
        let idx = HnswIndex::new(5);
        assert_eq!(idx.dimensions(), 5);
        assert_eq!(idx.len(), 0);
        assert!(idx.is_empty());

        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(idx.len(), 1);
        assert!(!idx.is_empty());
    }
}
