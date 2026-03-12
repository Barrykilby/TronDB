use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use thiserror::Error;

use crate::types::VectorData;

// ---------------------------------------------------------------------------
// Vectoriser errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum VectoriserError {
    #[error("encoding failed: {0}")]
    EncodeFailed(String),

    #[error("operation not supported: {0}")]
    NotSupported(String),

    #[error("model not loaded: {0}")]
    ModelNotLoaded(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("auth error: {0}")]
    Auth(String),
}

// ---------------------------------------------------------------------------
// FieldSet — input to encode()
// ---------------------------------------------------------------------------

/// Named field values passed to the vectoriser for encoding.
/// Keys are field names, values are their current string representations.
pub type FieldSet = HashMap<String, String>;

// ---------------------------------------------------------------------------
// Vectoriser trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Vectoriser: Send + Sync {
    /// Unique identifier for this vectoriser instance (e.g. "onnx-dense:bge-small-en-v1.5")
    fn id(&self) -> &str;

    /// Model identifier (e.g. "bge-small-en-v1.5", "splade-v3", "passthrough")
    fn model_id(&self) -> &str;

    /// Output dimensions (dense) or 0 (sparse/passthrough)
    fn output_size(&self) -> usize;

    /// Whether this produces dense or sparse vectors
    fn output_kind(&self) -> VectorKind;

    /// Generate a vector from entity fields
    async fn encode(&self, fields: &FieldSet) -> Result<VectorData, VectoriserError>;

    /// Batch encode for throughput (default: sequential)
    async fn encode_batch(&self, batch: &[FieldSet]) -> Result<Vec<VectorData>, VectoriserError> {
        let mut results = Vec::with_capacity(batch.len());
        for fields in batch {
            results.push(self.encode(fields).await?);
        }
        Ok(results)
    }

    /// Whether MRL (Matryoshka) truncation is supported
    fn supports_mrl(&self) -> bool { false }

    /// Encode a query string (for SEARCH NEAR 'text')
    async fn encode_query(&self, query: &str) -> Result<VectorData, VectoriserError>;

    /// Optional: incremental update (delta encoding). Returns None to signal full recompute needed.
    async fn encode_delta(&self, _prev: &VectorData, _changed_fields: &FieldSet)
        -> Result<Option<VectorData>, VectoriserError> {
        Ok(None) // default: full recompute via encode()
    }
}

// NOTE: The spec (§3.1) defines VectorOutput::Dense(Vec<f64>). We intentionally use the
// existing VectorData::Dense(Vec<f32>) to avoid f64→f32 conversions throughout the engine.
// The entire pipeline (HNSW, quantisation, storage) operates on f32. This is a deliberate
// divergence — do not "fix" it to f64.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorKind {
    Dense,
    Sparse,
}

// ---------------------------------------------------------------------------
// VectoriserRegistry — maps collection:repr to vectoriser instance
// ---------------------------------------------------------------------------

/// Registry mapping "{collection}:{repr_name}" keys to vectoriser instances.
pub struct VectoriserRegistry {
    vectorisers: DashMap<String, Arc<dyn Vectoriser>>,
}

impl VectoriserRegistry {
    pub fn new() -> Self {
        Self {
            vectorisers: DashMap::new(),
        }
    }

    /// Register a vectoriser for a collection's representation.
    pub fn register(&self, collection: &str, repr_name: &str, vectoriser: Arc<dyn Vectoriser>) {
        let key = format!("{collection}:{repr_name}");
        self.vectorisers.insert(key, vectoriser);
    }

    /// Look up the vectoriser for a collection's representation.
    pub fn get(&self, collection: &str, repr_name: &str) -> Option<Arc<dyn Vectoriser>> {
        let key = format!("{collection}:{repr_name}");
        self.vectorisers.get(&key).map(|v| Arc::clone(&v))
    }

    /// Check if a representation has a registered vectoriser.
    pub fn has(&self, collection: &str, repr_name: &str) -> bool {
        let key = format!("{collection}:{repr_name}");
        self.vectorisers.contains_key(&key)
    }

    /// Remove all vectorisers for a collection.
    pub fn remove_collection(&self, collection: &str) {
        let prefix = format!("{collection}:");
        self.vectorisers.retain(|k, _| !k.starts_with(&prefix));
    }
}

impl Default for VectoriserRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Recipe hash — staleness detection for mutation cascade
// ---------------------------------------------------------------------------

/// Compute a recipe hash from the vectoriser model ID and contributing field names.
/// The hash changes when the model or the field list changes.
/// Used for staleness detection in the mutation cascade.
pub fn compute_recipe_hash(model_id: &str, fields: &[String]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"recipe:v1:");
    hasher.update(model_id.as_bytes());
    hasher.update(b":");
    // Sort fields for determinism
    let mut sorted_fields = fields.to_vec();
    sorted_fields.sort();
    for (i, field) in sorted_fields.iter().enumerate() {
        if i > 0 { hasher.update(b","); }
        hasher.update(field.as_bytes());
    }
    hasher.finalize().into()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal mock to verify the trait is object-safe.
    struct FakeVectoriser;

    #[async_trait]
    impl Vectoriser for FakeVectoriser {
        fn id(&self) -> &str { "fake" }
        fn model_id(&self) -> &str { "fake-model" }
        fn output_size(&self) -> usize { 3 }
        fn output_kind(&self) -> VectorKind { VectorKind::Dense }

        async fn encode(&self, _fields: &FieldSet) -> Result<VectorData, VectoriserError> {
            Ok(VectorData::Dense(vec![0.1, 0.2, 0.3]))
        }

        async fn encode_query(&self, _query: &str) -> Result<VectorData, VectoriserError> {
            Ok(VectorData::Dense(vec![0.1, 0.2, 0.3]))
        }
    }

    #[test]
    fn trait_is_object_safe() {
        // If this compiles, the trait is object-safe
        let _: Box<dyn Vectoriser> = Box::new(FakeVectoriser);
    }

    #[test]
    fn registry_add_and_get() {
        let registry = VectoriserRegistry::new();
        let v: Arc<dyn Vectoriser> = Arc::new(FakeVectoriser);
        registry.register("venues", "identity", v);

        assert!(registry.has("venues", "identity"));
        assert!(!registry.has("venues", "nonexistent"));

        let retrieved = registry.get("venues", "identity").unwrap();
        assert_eq!(retrieved.id(), "fake");
    }

    #[test]
    fn registry_remove_collection() {
        let registry = VectoriserRegistry::new();
        let v: Arc<dyn Vectoriser> = Arc::new(FakeVectoriser);
        registry.register("venues", "identity", Arc::clone(&v));
        registry.register("venues", "sparse", Arc::clone(&v));
        registry.register("events", "identity", v);

        registry.remove_collection("venues");

        assert!(!registry.has("venues", "identity"));
        assert!(!registry.has("venues", "sparse"));
        assert!(registry.has("events", "identity"));
    }

    #[tokio::test]
    async fn encode_returns_vector() {
        let v = FakeVectoriser;
        let fields = FieldSet::from([("name".into(), "Test Venue".into())]);
        let result = v.encode(&fields).await.unwrap();
        match result {
            VectorData::Dense(d) => assert_eq!(d.len(), 3),
            _ => panic!("expected Dense"),
        }
    }

    #[tokio::test]
    async fn encode_batch_default_impl() {
        let v = FakeVectoriser;
        let batch = vec![
            FieldSet::from([("name".into(), "A".into())]),
            FieldSet::from([("name".into(), "B".into())]),
        ];
        let results = v.encode_batch(&batch).await.unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn recipe_hash_is_deterministic() {
        let fields = vec!["name".into(), "description".into()];
        let a = compute_recipe_hash("bge-small-en-v1.5", &fields);
        let b = compute_recipe_hash("bge-small-en-v1.5", &fields);
        assert_eq!(a, b);
    }

    #[test]
    fn recipe_hash_changes_with_model() {
        let fields = vec!["name".into()];
        let a = compute_recipe_hash("bge-small-en-v1.5", &fields);
        let b = compute_recipe_hash("jina-v4", &fields);
        assert_ne!(a, b);
    }

    #[test]
    fn recipe_hash_changes_with_fields() {
        let a = compute_recipe_hash("bge", &vec!["name".into()]);
        let b = compute_recipe_hash("bge", &vec!["name".into(), "description".into()]);
        assert_ne!(a, b);
    }

    #[test]
    fn recipe_hash_is_field_order_independent() {
        let a = compute_recipe_hash("bge", &vec!["name".into(), "description".into()]);
        let b = compute_recipe_hash("bge", &vec!["description".into(), "name".into()]);
        assert_eq!(a, b);
    }

    #[test]
    fn recipe_hash_is_not_all_zeros() {
        let h = compute_recipe_hash("bge", &vec!["name".into()]);
        assert_ne!(h, [0u8; 32]);
    }
}
