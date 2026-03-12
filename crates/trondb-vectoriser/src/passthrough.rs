use async_trait::async_trait;
use trondb_core::types::VectorData;
use trondb_core::vectoriser::{FieldSet, VectorKind, Vectoriser, VectoriserError};

/// Accepts pre-computed vectors at INSERT time. Does not generate vectors.
/// encode() and encode_query() return NotSupported.
/// This formalises the existing TronDB behaviour before Phase 10.
pub struct PassthroughVectoriser {
    dimensions: usize,
    kind: VectorKind,
}

impl PassthroughVectoriser {
    pub fn new_dense(dimensions: usize) -> Self {
        Self { dimensions, kind: VectorKind::Dense }
    }

    pub fn new_sparse() -> Self {
        Self { dimensions: 0, kind: VectorKind::Sparse }
    }
}

#[async_trait]
impl Vectoriser for PassthroughVectoriser {
    fn id(&self) -> &str { "passthrough" }
    fn model_id(&self) -> &str { "passthrough" }
    fn output_size(&self) -> usize { self.dimensions }
    fn output_kind(&self) -> VectorKind { self.kind }

    async fn encode(&self, _fields: &FieldSet) -> Result<VectorData, VectoriserError> {
        Err(VectoriserError::NotSupported(
            "PassthroughVectoriser does not generate vectors — provide vectors at INSERT time".into()
        ))
    }

    async fn encode_query(&self, _query: &str) -> Result<VectorData, VectoriserError> {
        Err(VectoriserError::NotSupported(
            "PassthroughVectoriser does not support natural language queries".into()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_dense_metadata() {
        let v = PassthroughVectoriser::new_dense(384);
        assert_eq!(v.id(), "passthrough");
        assert_eq!(v.model_id(), "passthrough");
        assert_eq!(v.output_size(), 384);
        assert_eq!(v.output_kind(), VectorKind::Dense);
    }

    #[test]
    fn passthrough_sparse_metadata() {
        let v = PassthroughVectoriser::new_sparse();
        assert_eq!(v.output_size(), 0);
        assert_eq!(v.output_kind(), VectorKind::Sparse);
    }

    #[tokio::test]
    async fn encode_returns_not_supported() {
        let v = PassthroughVectoriser::new_dense(384);
        let fields = FieldSet::from([("name".into(), "test".into())]);
        let err = v.encode(&fields).await.unwrap_err();
        assert!(matches!(err, VectoriserError::NotSupported(_)));
    }

    #[tokio::test]
    async fn encode_query_returns_not_supported() {
        let v = PassthroughVectoriser::new_dense(384);
        let err = v.encode_query("test query").await.unwrap_err();
        assert!(matches!(err, VectoriserError::NotSupported(_)));
    }
}
