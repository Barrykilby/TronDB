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

/// Fraction of live entries that tombstones must exceed before a rebuild is
/// triggered. 10% = 0.10.
const TOMBSTONE_REBUILD_THRESHOLD: f64 = 0.10;

// ---------------------------------------------------------------------------
// HnswIndex — one per collection
// ---------------------------------------------------------------------------

pub struct HnswIndex {
    inner: Mutex<Hnsw<'static, f32, DistCosine>>,
    dimensions: usize,
    id_to_idx: DashMap<LogicalId, usize>,
    idx_to_id: DashMap<usize, LogicalId>,
    next_idx: AtomicUsize,
    tombstones: DashMap<LogicalId, ()>,
    tombstone_count: AtomicUsize,
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
            tombstones: DashMap::new(),
            tombstone_count: AtomicUsize::new(0),
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

    /// Mark an entity as tombstoned (logically removed from the index).
    /// The entry remains in the underlying HNSW graph but is filtered from
    /// search results. Calling remove on a non-existent id is a no-op.
    pub fn remove(&self, id: &LogicalId) {
        if !self.id_to_idx.contains_key(id) {
            return;
        }
        // Only count a new tombstone if not already tombstoned.
        if self.tombstones.insert(id.clone(), ()).is_none() {
            self.tombstone_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Returns the current number of tombstoned entries.
    pub fn tombstone_count(&self) -> usize {
        self.tombstone_count.load(Ordering::SeqCst)
    }

    /// Returns true when tombstones exceed TOMBSTONE_REBUILD_THRESHOLD of the
    /// live (non-tombstoned) entry count.
    pub fn needs_rebuild(&self) -> bool {
        let ts = self.tombstone_count.load(Ordering::SeqCst);
        if ts == 0 {
            return false;
        }
        let total = self.id_to_idx.len();
        let live = total.saturating_sub(ts);
        if live == 0 {
            return ts > 0;
        }
        (ts as f64 / live as f64) > TOMBSTONE_REBUILD_THRESHOLD
    }

    /// Rebuild the HNSW index from scratch using only the provided live data.
    /// All existing data and tombstones are cleared.
    pub fn rebuild(&self, live_data: &[(LogicalId, Vec<f32>)]) {
        let new_hnsw = Hnsw::new(
            MAX_NB_CONNECTION,
            MAX_ELEMENTS,
            MAX_LAYER,
            EF_CONSTRUCTION,
            DistCosine,
        );

        self.id_to_idx.clear();
        self.idx_to_id.clear();
        self.tombstones.clear();
        self.tombstone_count.store(0, Ordering::SeqCst);
        self.next_idx.store(0, Ordering::SeqCst);

        for (id, vector) in live_data {
            let idx = self.next_idx.fetch_add(1, Ordering::SeqCst);
            self.id_to_idx.insert(id.clone(), idx);
            self.idx_to_id.insert(idx, id.clone());
            new_hnsw.insert((vector.as_slice(), idx));
        }

        *self.inner.lock().unwrap() = new_hnsw;
    }

    /// Search for k nearest neighbours.
    /// Returns (LogicalId, similarity_score) sorted by descending similarity.
    /// similarity = 1.0 - cosine_distance.
    /// Tombstoned entries are excluded from results.
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
                self.idx_to_id.get(&idx).and_then(|id| {
                    if self.tombstones.contains_key(id.value()) {
                        None
                    } else {
                        Some((id.clone(), similarity))
                    }
                })
            })
            .collect();

        // Sort by descending similarity
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results
    }

    pub fn len(&self) -> usize {
        self.id_to_idx
            .len()
            .saturating_sub(self.tombstone_count.load(Ordering::SeqCst))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
        assert!(results.len() >= 2 && results.len() <= 3);
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

    #[test]
    fn remove_excludes_from_search() {
        let idx = HnswIndex::new(3);
        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        idx.insert(&make_id("e2"), &[0.0, 1.0, 0.0]);
        idx.insert(&make_id("e3"), &[0.9, 0.1, 0.0]);

        idx.remove(&make_id("e1"));

        let results = idx.search(&[1.0, 0.0, 0.0], 3);
        assert!(results.iter().all(|(id, _)| id != &make_id("e1")));
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let idx = HnswIndex::new(3);
        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        idx.remove(&make_id("nonexistent"));
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn tombstone_count_tracking() {
        let idx = HnswIndex::new(3);
        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        idx.insert(&make_id("e2"), &[0.0, 1.0, 0.0]);
        assert_eq!(idx.tombstone_count(), 0);
        idx.remove(&make_id("e1"));
        assert_eq!(idx.tombstone_count(), 1);
        assert!(idx.needs_rebuild()); // 1 tombstone, 1 live = 50% > 10%
    }

    #[test]
    fn rebuild_clears_tombstones() {
        let idx = HnswIndex::new(3);
        idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        idx.insert(&make_id("e2"), &[0.0, 1.0, 0.0]);
        idx.insert(&make_id("e3"), &[0.5, 0.5, 0.0]);
        idx.remove(&make_id("e1"));

        let live_data: Vec<(LogicalId, Vec<f32>)> = vec![
            (make_id("e2"), vec![0.0, 1.0, 0.0]),
            (make_id("e3"), vec![0.5, 0.5, 0.0]),
        ];
        idx.rebuild(&live_data);

        assert_eq!(idx.len(), 2);
        assert_eq!(idx.tombstone_count(), 0);
        assert!(!idx.needs_rebuild());

        let results = idx.search(&[0.0, 1.0, 0.0], 2);
        assert_eq!(results[0].0, make_id("e2"));
    }
}
