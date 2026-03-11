use trondb_core::error::EngineError;

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("no healthy nodes available for routing")]
    NoCandidates,

    #[error("all nodes overloaded, cluster at capacity")]
    ClusterOverloaded,

    #[error("node overloaded, retry after {retry_after_ms}ms")]
    Backpressure { retry_after_ms: u64 },

    #[error("affinity group '{0}' is full (max {1} entities)")]
    AffinityGroupFull(String, usize),

    #[error("affinity group '{0}' not found")]
    AffinityGroupNotFound(String),

    #[error("affinity group '{0}' already exists")]
    AffinityGroupAlreadyExists(String),

    #[error(transparent)]
    Engine(#[from] EngineError),
}
