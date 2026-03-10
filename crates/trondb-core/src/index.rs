use std::sync::Mutex;

use crate::types::LogicalId;

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

struct BruteForceIndex {
    vectors: Vec<(LogicalId, Vec<f32>)>,
}

pub struct VectorIndex {
    #[allow(dead_code)]
    dimensions: usize,
    inner: Mutex<BruteForceIndex>,
}

impl VectorIndex {
    pub fn new(dimensions: usize) -> Self {
        Self {
            dimensions,
            inner: Mutex::new(BruteForceIndex {
                vectors: Vec::new(),
            }),
        }
    }

    pub fn insert(&self, id: &LogicalId, vector: &[f32]) {
        let mut inner = self.inner.lock().unwrap();
        // Replace existing entry for same ID
        if let Some(pos) = inner.vectors.iter().position(|(eid, _)| eid == id) {
            inner.vectors[pos].1 = vector.to_vec();
        } else {
            inner.vectors.push((id.clone(), vector.to_vec()));
        }
    }

    pub fn search(&self, query: &[f32], k: usize) -> Vec<(LogicalId, f32)> {
        let inner = self.inner.lock().unwrap();
        let mut results: Vec<(LogicalId, f32)> = inner
            .vectors
            .iter()
            .map(|(id, vec)| (id.clone(), cosine_similarity(query, vec)))
            .collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(k);
        results
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().vectors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_search() {
        let index = VectorIndex::new(3);
        index.insert(&LogicalId::from_str("a"), &[1.0, 0.0, 0.0]);
        index.insert(&LogicalId::from_str("b"), &[0.9, 0.1, 0.0]);
        index.insert(&LogicalId::from_str("c"), &[0.0, 0.0, 1.0]);

        let results = index.search(&[1.0, 0.0, 0.0], 3);
        assert_eq!(results.len(), 3);
        // First result should be exact match "a"
        assert_eq!(results[0].0.as_str(), "a");
        // Second should be "b" (close to [1,0,0])
        assert_eq!(results[1].0.as_str(), "b");
    }

    #[test]
    fn search_empty_index() {
        let index = VectorIndex::new(3);
        let results = index.search(&[1.0, 0.0, 0.0], 5);
        assert!(results.is_empty());
    }

    #[test]
    fn similarity_scores_are_valid() {
        let index = VectorIndex::new(3);
        index.insert(&LogicalId::from_str("x"), &[1.0, 2.0, 3.0]);

        let results = index.search(&[1.0, 2.0, 3.0], 1);
        assert_eq!(results.len(), 1);
        assert!(results[0].1 > 0.99);
    }
}
