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
}
