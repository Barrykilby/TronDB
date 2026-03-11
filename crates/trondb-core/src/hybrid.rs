use std::collections::HashMap;
use crate::types::LogicalId;

const DEFAULT_RRF_K: usize = 60;

/// Merge dense and sparse search results using Reciprocal Rank Fusion.
/// Ranks are 1-based (first result has rank 1).
/// Returns merged results sorted by descending RRF score.
pub fn merge_rrf(
    dense_results: &[(LogicalId, f32)],
    sparse_results: &[(LogicalId, f32)],
    rrf_k: usize,
) -> Vec<(LogicalId, f32)> {
    let mut scores: HashMap<LogicalId, f32> = HashMap::new();

    for (rank, (id, _)) in dense_results.iter().enumerate() {
        let rrf_score = 1.0 / (rrf_k as f32 + (rank + 1) as f32); // 1-based rank
        *scores.entry(id.clone()).or_default() += rrf_score;
    }

    for (rank, (id, _)) in sparse_results.iter().enumerate() {
        let rrf_score = 1.0 / (rrf_k as f32 + (rank + 1) as f32);
        *scores.entry(id.clone()).or_default() += rrf_score;
    }

    let mut results: Vec<(LogicalId, f32)> = scores.into_iter().collect();
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results
}

/// Default RRF k parameter.
pub fn default_rrf_k() -> usize {
    DEFAULT_RRF_K
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_id(s: &str) -> LogicalId {
        LogicalId::from_string(s)
    }

    #[test]
    fn rrf_merge_overlapping_results() {
        let dense = vec![
            (make_id("e1"), 0.95),
            (make_id("e2"), 0.80),
            (make_id("e3"), 0.60),
        ];
        let sparse = vec![
            (make_id("e2"), 1.5),
            (make_id("e1"), 1.0),
            (make_id("e4"), 0.5),
        ];

        let merged = merge_rrf(&dense, &sparse, 60);

        // e1: 1/(60+1) + 1/(60+2) = 0.01639 + 0.01613 = 0.03252
        // e2: 1/(60+2) + 1/(60+1) = 0.01613 + 0.01639 = 0.03252
        // Both e1 and e2 appear in both lists — they should have the highest scores
        let top_ids: Vec<&LogicalId> = merged.iter().map(|(id, _)| id).collect();
        assert!(top_ids.contains(&&make_id("e1")));
        assert!(top_ids.contains(&&make_id("e2")));
        assert_eq!(merged.len(), 4); // e1, e2, e3, e4
    }

    #[test]
    fn rrf_merge_disjoint_results() {
        let dense = vec![(make_id("e1"), 0.9)];
        let sparse = vec![(make_id("e2"), 1.0)];

        let merged = merge_rrf(&dense, &sparse, 60);
        assert_eq!(merged.len(), 2);
        // Both should have same score (both rank 1 in their respective lists)
        assert!((merged[0].1 - merged[1].1).abs() < 0.001);
    }

    #[test]
    fn rrf_merge_empty_inputs() {
        let merged = merge_rrf(&[], &[], 60);
        assert!(merged.is_empty());

        let dense = vec![(make_id("e1"), 0.9)];
        let merged = merge_rrf(&dense, &[], 60);
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn rrf_scores_decrease_with_rank() {
        let dense = vec![
            (make_id("e1"), 0.9),
            (make_id("e2"), 0.8),
            (make_id("e3"), 0.7),
        ];
        let merged = merge_rrf(&dense, &[], 60);
        assert!(merged[0].1 > merged[1].1);
        assert!(merged[1].1 > merged[2].1);
    }

    #[test]
    fn entity_in_both_lists_ranks_higher_than_single() {
        let dense = vec![
            (make_id("e1"), 0.9),
            (make_id("e2"), 0.8),
        ];
        let sparse = vec![
            (make_id("e1"), 1.0),
            (make_id("e3"), 0.5),
        ];

        let merged = merge_rrf(&dense, &sparse, 60);
        // e1 is in both lists (rank 1 in both), should be first
        assert_eq!(merged[0].0, make_id("e1"));
    }
}
