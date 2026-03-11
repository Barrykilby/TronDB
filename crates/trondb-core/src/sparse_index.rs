use std::collections::HashMap;
use dashmap::DashMap;
use crate::types::LogicalId;

const MIN_WEIGHT: f32 = 0.001;

// ---------------------------------------------------------------------------
// SparseIndex — RAM inverted index for SPLADE-style sparse vectors
// ---------------------------------------------------------------------------

pub struct SparseIndex {
    postings: DashMap<u32, Vec<(LogicalId, f32)>>,
}

impl SparseIndex {
    pub fn new() -> Self {
        Self {
            postings: DashMap::new(),
        }
    }

    pub fn insert(&self, entity_id: &LogicalId, vector: &[(u32, f32)]) {
        for &(dim, weight) in vector {
            if weight.abs() < MIN_WEIGHT {
                continue;
            }
            self.postings
                .entry(dim)
                .or_default()
                .push((entity_id.clone(), weight));
        }
    }

    pub fn remove(&self, entity_id: &LogicalId, vector: &[(u32, f32)]) {
        for &(dim, _) in vector {
            self.postings.entry(dim).and_modify(|entries| {
                entries.retain(|(id, _)| id != entity_id);
            });
            // Atomically remove empty posting lists
            self.postings.remove_if(&dim, |_, v| v.is_empty());
        }
    }

    /// Search by accumulating dot-product scores, return top-k.
    pub fn search(&self, query: &[(u32, f32)], k: usize) -> Vec<(LogicalId, f32)> {
        let mut scores: HashMap<LogicalId, f32> = HashMap::new();

        for &(dim, query_weight) in query {
            if let Some(postings) = self.postings.get(&dim) {
                for (entity_id, posting_weight) in postings.iter() {
                    *scores.entry(entity_id.clone()).or_default() += query_weight * posting_weight;
                }
            }
        }

        let mut results: Vec<(LogicalId, f32)> = scores.into_iter().collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(k);
        results
    }

    pub fn len(&self) -> usize {
        self.postings.iter().map(|e| e.value().len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.postings.is_empty()
    }
}

impl Default for SparseIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_id(s: &str) -> LogicalId {
        LogicalId::from_string(s)
    }

    #[test]
    fn insert_and_search_single() {
        let idx = SparseIndex::new();
        idx.insert(&make_id("e1"), &[(1, 0.8), (42, 0.5)]);

        let results = idx.search(&[(1, 1.0), (42, 1.0)], 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, make_id("e1"));
        // dot product = 0.8*1.0 + 0.5*1.0 = 1.3
        assert!((results[0].1 - 1.3).abs() < 0.01);
    }

    #[test]
    fn search_ranking_order() {
        let idx = SparseIndex::new();
        idx.insert(&make_id("e1"), &[(1, 0.8), (2, 0.2)]);
        idx.insert(&make_id("e2"), &[(1, 0.3), (2, 0.9)]);

        let results = idx.search(&[(1, 1.0)], 10);
        // e1 has higher weight on dim 1
        assert_eq!(results[0].0, make_id("e1"));
        assert_eq!(results[1].0, make_id("e2"));
    }

    #[test]
    fn search_top_k_limits() {
        let idx = SparseIndex::new();
        for i in 0..10 {
            idx.insert(&LogicalId::from_string(&format!("e{i}")), &[(1, i as f32 * 0.1)]);
        }
        let results = idx.search(&[(1, 1.0)], 3);
        assert_eq!(results.len(), 3);
        // Highest scores first
        assert_eq!(results[0].0, LogicalId::from_string("e9"));
    }

    #[test]
    fn remove_entity() {
        let idx = SparseIndex::new();
        idx.insert(&make_id("e1"), &[(1, 0.8), (42, 0.5)]);
        idx.remove(&make_id("e1"), &[(1, 0.8), (42, 0.5)]);
        assert!(idx.search(&[(1, 1.0)], 10).is_empty());
    }

    #[test]
    fn low_weight_filtered() {
        let idx = SparseIndex::new();
        idx.insert(&make_id("e1"), &[(1, 0.0001), (2, 0.5)]);
        // Dim 1 below MIN_WEIGHT, should not be indexed
        let results = idx.search(&[(1, 1.0)], 10);
        assert!(results.is_empty());
        // Dim 2 above threshold
        let results = idx.search(&[(2, 1.0)], 10);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_empty_index() {
        let idx = SparseIndex::new();
        let results = idx.search(&[(1, 1.0)], 10);
        assert!(results.is_empty());
    }

    #[test]
    fn len_counts_postings() {
        let idx = SparseIndex::new();
        idx.insert(&make_id("e1"), &[(1, 0.8), (2, 0.5)]);
        idx.insert(&make_id("e2"), &[(1, 0.3)]);
        assert_eq!(idx.len(), 3); // 2 postings for dim1, 1 for dim2
    }
}
