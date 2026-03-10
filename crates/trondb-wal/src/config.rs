use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct WalConfig {
    pub wal_dir: PathBuf,
    pub segment_size_bytes: usize,
    pub buffer_capacity_bytes: usize,
    pub flush_interval_ms: u64,
    pub checkpoint_interval_ms: u64,
    pub retention_ms: u64,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            wal_dir: PathBuf::from("./trondb_data/wal"),
            segment_size_bytes: 256 * 1024 * 1024, // 256 MiB
            buffer_capacity_bytes: 64 * 1024 * 1024, // 64 MiB
            flush_interval_ms: 10,
            checkpoint_interval_ms: 60_000,
            retention_ms: 3_600_000,
        }
    }
}
