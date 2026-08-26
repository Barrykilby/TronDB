use thiserror::Error;

#[derive(Debug, Error)]
pub enum WalError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialisation error: {0}")]
    Serialisation(String),

    #[error("CRC32 mismatch at LSN {lsn}: expected {expected:#010x}, got {got:#010x}")]
    CrcMismatch { lsn: u64, expected: u32, got: u32 },

    #[error("corrupt WAL segment: {0}")]
    CorruptSegment(String),

    #[error("invalid segment header: {0}")]
    InvalidHeader(String),

    #[error("WAL record truncated at LSN {0}")]
    Truncated(u64),
}
