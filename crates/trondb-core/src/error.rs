use thiserror::Error;
use trondb_wal::WalError;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("collection not found: {0}")]
    CollectionNotFound(String),

    #[error("collection already exists: {0}")]
    CollectionAlreadyExists(String),

    #[error("entity not found: {0}")]
    EntityNotFound(String),

    #[error("dimension mismatch: expected {expected}, got {got}")]
    DimensionMismatch { expected: usize, got: usize },

    #[error("invalid query: {0}")]
    InvalidQuery(String),

    #[error("WAL error: {0}")]
    Wal(#[from] WalError),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("unsupported operation: {0}")]
    UnsupportedOperation(String),

    #[error("invalid state transition: {from:?} → {to:?}")]
    InvalidStateTransition {
        from: crate::location::LocState,
        to: crate::location::LocState,
    },

    #[error("location not found: {0}")]
    LocationNotFound(String),

    #[error("edge type not found: {0}")]
    EdgeTypeNotFound(String),

    #[error("edge type already exists: {0}")]
    EdgeTypeAlreadyExists(String),

    #[error("duplicate index: {0}")]
    DuplicateIndex(String),

    #[error("duplicate representation: {0}")]
    DuplicateRepresentation(String),

    #[error("duplicate field: {0}")]
    DuplicateField(String),

    #[error("field not indexed (ScalarPreFilter requires index): {0}")]
    FieldNotIndexed(String),

    #[error("sparse vector required but collection has no sparse representation: {0}")]
    SparseVectorRequired(String),

    #[error("invalid field type for {field}: expected {expected}, got {got}")]
    InvalidFieldType {
        field: String,
        expected: String,
        got: String,
    },

    #[error("join error: {0}")]
    JoinError(String),

    #[error("internal error: {0}")]
    Internal(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_messages() {
        assert!(EngineError::DuplicateIndex("idx1".into()).to_string().contains("idx1"));
        assert!(EngineError::DuplicateRepresentation("identity".into()).to_string().contains("identity"));
        assert!(EngineError::DuplicateField("status".into()).to_string().contains("status"));
        assert!(EngineError::FieldNotIndexed("city".into()).to_string().contains("city"));
        assert!(EngineError::SparseVectorRequired("venues".into()).to_string().contains("venues"));
        assert!(EngineError::InvalidFieldType {
            field: "age".into(),
            expected: "Int".into(),
            got: "String".into(),
        }.to_string().contains("age"));
    }
}
