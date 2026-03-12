use async_trait::async_trait;
use sha2::{Digest, Sha256};
use trondb_core::types::VectorData;
use trondb_core::vectoriser::{FieldSet, VectorKind, Vectoriser, VectoriserError};

/// Deterministic mock vectoriser for tests.
/// Produces a reproducible vector by hashing input fields.
/// Not a real embedding model — just consistent fake data.
pub struct MockVectoriser {
    dimensions: usize,
}

impl MockVectoriser {
    pub fn new(dimensions: usize) -> Self {
        Self { dimensions }
    }

    /// Generate a deterministic vector from a string by hashing it.
    fn hash_to_vector(&self, input: &str) -> Vec<f32> {
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        let hash = hasher.finalize();

        // Expand hash bytes into f32 values in [-1, 1]
        let mut vector = Vec::with_capacity(self.dimensions);
        for i in 0..self.dimensions {
            let byte_idx = i % 32;
            let raw = hash[byte_idx] as f32 / 128.0 - 1.0; // maps 0..255 to -1..~0.99
            vector.push(raw);
        }
        vector
    }
}

#[async_trait]
impl Vectoriser for MockVectoriser {
    fn id(&self) -> &str { "mock" }
    fn model_id(&self) -> &str { "mock-model" }
    fn output_size(&self) -> usize { self.dimensions }
    fn output_kind(&self) -> VectorKind { VectorKind::Dense }

    async fn encode(&self, fields: &FieldSet) -> Result<VectorData, VectoriserError> {
        // Sort fields for determinism, concatenate
        let mut pairs: Vec<_> = fields.iter().collect();
        pairs.sort_by_key(|(k, _)| (*k).clone());
        let combined: String = pairs.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("|");
        Ok(VectorData::Dense(self.hash_to_vector(&combined)))
    }

    async fn encode_query(&self, query: &str) -> Result<VectorData, VectoriserError> {
        Ok(VectorData::Dense(self.hash_to_vector(query)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn encode_is_deterministic() {
        let v = MockVectoriser::new(8);
        let fields = FieldSet::from([("name".into(), "Jazz Club".into())]);
        let a = v.encode(&fields).await.unwrap();
        let b = v.encode(&fields).await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn encode_different_input_different_output() {
        let v = MockVectoriser::new(8);
        let a = v.encode(&FieldSet::from([("name".into(), "Jazz".into())])).await.unwrap();
        let b = v.encode(&FieldSet::from([("name".into(), "Rock".into())])).await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn encode_query_works() {
        let v = MockVectoriser::new(4);
        let result = v.encode_query("live jazz in Bristol").await.unwrap();
        match result {
            VectorData::Dense(d) => assert_eq!(d.len(), 4),
            _ => panic!("expected Dense"),
        }
    }

    #[tokio::test]
    async fn output_dimensions_match() {
        let v = MockVectoriser::new(384);
        let fields = FieldSet::from([("x".into(), "y".into())]);
        match v.encode(&fields).await.unwrap() {
            VectorData::Dense(d) => assert_eq!(d.len(), 384),
            _ => panic!("expected Dense"),
        }
    }
}
