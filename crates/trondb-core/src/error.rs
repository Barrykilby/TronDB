use thiserror::Error;

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

    #[error("storage error: {0}")]
    Storage(String),
}
