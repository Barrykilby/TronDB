use serde::Deserialize;
use std::path::{Path, PathBuf};
use trondb_core::{EngineConfig};
use trondb_wal::WalConfig;

// ---------------------------------------------------------------------------
// ConfigError
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ConfigError {
    Toml(toml::de::Error),
    Io(std::io::Error),
    MissingField(String),
    InvalidPeer(String),
    InvalidRole(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Toml(e) => write!(f, "TOML parse error: {e}"),
            ConfigError::Io(e) => write!(f, "I/O error: {e}"),
            ConfigError::MissingField(s) => write!(f, "missing required field: {s}"),
            ConfigError::InvalidPeer(s) => write!(f, "invalid peer entry: {s}"),
            ConfigError::InvalidRole(s) => write!(f, "invalid role: {s}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        ConfigError::Toml(e)
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// NodeRoleConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeRoleConfig {
    Primary,
    Replica,
    Router,
}

impl NodeRoleConfig {
    fn from_str(s: &str) -> Result<Self, ConfigError> {
        match s.to_lowercase().as_str() {
            "primary" => Ok(NodeRoleConfig::Primary),
            "replica" => Ok(NodeRoleConfig::Replica),
            "router" => Ok(NodeRoleConfig::Router),
            other => Err(ConfigError::InvalidRole(other.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// PeerConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct PeerConfig {
    pub node_id: String,
    pub role: NodeRoleConfig,
    pub addr: String,
}

impl PeerConfig {
    /// Parse the `TRONDB_PEERS` env var format:
    /// `"node-2:replica:192.168.1.11:9400,router-1:router:192.168.1.12:9400"`
    ///
    /// Each entry is `<node_id>:<role>:<host>:<port>`.
    pub fn parse_env(s: &str) -> Result<Vec<PeerConfig>, ConfigError> {
        let mut peers = Vec::new();
        for entry in s.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            // Split on ':' — node_id is first, role is second, addr is remainder (host:port)
            let parts: Vec<&str> = entry.splitn(3, ':').collect();
            if parts.len() != 3 {
                return Err(ConfigError::InvalidPeer(format!(
                    "expected node_id:role:host:port, got '{entry}'"
                )));
            }
            let node_id = parts[0].to_string();
            let role = NodeRoleConfig::from_str(parts[1])?;
            let addr = parts[2].to_string();
            peers.push(PeerConfig { node_id, role, addr });
        }
        Ok(peers)
    }
}

// ---------------------------------------------------------------------------
// ReplicationConfig
// ---------------------------------------------------------------------------

fn default_min_ack() -> u32 {
    1
}

fn default_ack_timeout() -> u64 {
    5000
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReplicationConfig {
    #[serde(default = "default_min_ack")]
    pub min_ack_replicas: u32,
    #[serde(default = "default_ack_timeout")]
    pub ack_timeout_ms: u64,
    #[serde(default)]
    pub require_replica: bool,
}

impl Default for ReplicationConfig {
    fn default() -> Self {
        ReplicationConfig {
            min_ack_replicas: default_min_ack(),
            ack_timeout_ms: default_ack_timeout(),
            require_replica: false,
        }
    }
}

// ---------------------------------------------------------------------------
// SnapshotConfig
// ---------------------------------------------------------------------------

fn default_snapshot_interval() -> u64 {
    300
}

#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotConfig {
    #[serde(default = "default_snapshot_interval")]
    pub snapshot_interval_secs: u64,
    #[serde(default = "default_snapshot_interval")]
    pub hnsw_snapshot_interval_secs: u64,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        SnapshotConfig {
            snapshot_interval_secs: default_snapshot_interval(),
            hnsw_snapshot_interval_secs: default_snapshot_interval(),
        }
    }
}

// ---------------------------------------------------------------------------
// ClusterConfig (inner) — what lives under [cluster] in the TOML
// ---------------------------------------------------------------------------

fn default_data_dir() -> PathBuf {
    PathBuf::from("/data/trondb")
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClusterConfig {
    pub node_id: String,
    pub role: NodeRoleConfig,
    pub bind_addr: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default)]
    pub replication: ReplicationConfig,
    #[serde(default)]
    pub snapshots: SnapshotConfig,
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
}

// ---------------------------------------------------------------------------
// TOML wrapper — top-level `[cluster]` table
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TomlRoot {
    cluster: ClusterConfig,
}

// ---------------------------------------------------------------------------
// ClusterConfig methods
// ---------------------------------------------------------------------------

impl ClusterConfig {
    /// Parse from a TOML string that contains a `[cluster]` table.
    pub fn from_toml(s: &str) -> Result<Self, ConfigError> {
        let root: TomlRoot = toml::from_str(s)?;
        Ok(root.cluster)
    }

    /// Load from a TOML file.
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        Self::from_toml(&content)
    }

    /// Construct entirely from `TRONDB_*` environment variables.
    /// Requires at least `TRONDB_NODE_ID`, `TRONDB_ROLE`, `TRONDB_BIND_ADDR`.
    pub fn from_env() -> Result<Self, ConfigError> {
        let node_id = std::env::var("TRONDB_NODE_ID")
            .map_err(|_| ConfigError::MissingField("TRONDB_NODE_ID".to_string()))?;
        let role_str = std::env::var("TRONDB_ROLE")
            .map_err(|_| ConfigError::MissingField("TRONDB_ROLE".to_string()))?;
        let role = NodeRoleConfig::from_str(&role_str)?;
        let bind_addr = std::env::var("TRONDB_BIND_ADDR")
            .map_err(|_| ConfigError::MissingField("TRONDB_BIND_ADDR".to_string()))?;

        let data_dir = std::env::var("TRONDB_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_data_dir());

        let mut config = ClusterConfig {
            node_id,
            role,
            bind_addr,
            data_dir,
            replication: ReplicationConfig::default(),
            snapshots: SnapshotConfig::default(),
            peers: Vec::new(),
        };

        config.apply_env_overrides();
        Ok(config)
    }

    /// Apply all `TRONDB_*` env vars as overrides onto an existing config.
    pub fn apply_env_overrides(&mut self) {
        for (key, value) in std::env::vars() {
            if key.starts_with("TRONDB_") {
                self.apply_env_override(&key, &value);
            }
        }
    }

    /// Apply a single env var override. Useful as a test helper.
    pub fn apply_env_override(&mut self, key: &str, value: &str) {
        match key {
            "TRONDB_NODE_ID" => self.node_id = value.to_string(),
            "TRONDB_ROLE" => {
                if let Ok(role) = NodeRoleConfig::from_str(value) {
                    self.role = role;
                }
            }
            "TRONDB_BIND_ADDR" => self.bind_addr = value.to_string(),
            "TRONDB_DATA_DIR" => self.data_dir = PathBuf::from(value),
            "TRONDB_MIN_ACK_REPLICAS" => {
                if let Ok(v) = value.parse::<u32>() {
                    self.replication.min_ack_replicas = v;
                }
            }
            "TRONDB_ACK_TIMEOUT_MS" => {
                if let Ok(v) = value.parse::<u64>() {
                    self.replication.ack_timeout_ms = v;
                }
            }
            "TRONDB_REQUIRE_REPLICA" => {
                self.replication.require_replica =
                    matches!(value.to_lowercase().as_str(), "true" | "1" | "yes");
            }
            "TRONDB_SNAPSHOT_INTERVAL_SECS" => {
                if let Ok(v) = value.parse::<u64>() {
                    self.snapshots.snapshot_interval_secs = v;
                }
            }
            "TRONDB_HNSW_SNAPSHOT_INTERVAL_SECS" => {
                if let Ok(v) = value.parse::<u64>() {
                    self.snapshots.hnsw_snapshot_interval_secs = v;
                }
            }
            "TRONDB_PEERS" => {
                if let Ok(peers) = PeerConfig::parse_env(value) {
                    self.peers = peers;
                }
            }
            _ => {}
        }
    }

    /// Convert this cluster config into a `trondb-core` `EngineConfig`.
    pub fn to_engine_config(&self) -> EngineConfig {
        EngineConfig {
            data_dir: self.data_dir.join("store"),
            wal: WalConfig {
                wal_dir: self.data_dir.join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: self.snapshots.snapshot_interval_secs,
            hnsw_snapshot_interval_secs: self.snapshots.hnsw_snapshot_interval_secs,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_toml() {
        let toml = r#"
[cluster]
node_id = "node-1"
role = "primary"
bind_addr = "0.0.0.0:9400"
data_dir = "/data/trondb"
"#;
        let config = ClusterConfig::from_toml(toml).unwrap();
        assert_eq!(config.node_id, "node-1");
        assert_eq!(config.role, NodeRoleConfig::Primary);
        assert_eq!(config.bind_addr, "0.0.0.0:9400");
        assert!(config.peers.is_empty());
    }

    #[test]
    fn parse_full_toml_with_peers() {
        let toml = r#"
[cluster]
node_id = "primary"
role = "primary"
bind_addr = "0.0.0.0:9400"
data_dir = "/data/trondb"

[cluster.replication]
min_ack_replicas = 1
ack_timeout_ms = 5000
require_replica = false

[cluster.snapshots]
snapshot_interval_secs = 300
hnsw_snapshot_interval_secs = 300

[[cluster.peers]]
node_id = "replica-1"
role = "replica"
addr = "192.168.1.11:9400"

[[cluster.peers]]
node_id = "router-1"
role = "router"
addr = "192.168.1.12:9400"
"#;
        let config = ClusterConfig::from_toml(toml).unwrap();
        assert_eq!(config.peers.len(), 2);
        assert_eq!(config.replication.min_ack_replicas, 1);
        assert_eq!(config.snapshots.snapshot_interval_secs, 300);
    }

    #[test]
    fn env_vars_override_toml() {
        let toml = r#"
[cluster]
node_id = "from-toml"
role = "primary"
bind_addr = "0.0.0.0:9400"
data_dir = "/data/trondb"
"#;
        let mut config = ClusterConfig::from_toml(toml).unwrap();
        // Simulate env var
        config.apply_env_override("TRONDB_NODE_ID", "from-env");
        assert_eq!(config.node_id, "from-env");
    }

    #[test]
    fn parse_peers_env_var() {
        let peers_str = "node-2:replica:192.168.1.11:9400,router-1:router:192.168.1.12:9400";
        let peers = PeerConfig::parse_env(peers_str).unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].node_id, "node-2");
        assert_eq!(peers[0].role, NodeRoleConfig::Replica);
        assert_eq!(peers[0].addr, "192.168.1.11:9400");
        assert_eq!(peers[1].node_id, "router-1");
        assert_eq!(peers[1].role, NodeRoleConfig::Router);
        assert_eq!(peers[1].addr, "192.168.1.12:9400");
    }

    #[test]
    fn role_dispatches_correctly() {
        assert_eq!(NodeRoleConfig::Primary, NodeRoleConfig::Primary);
        assert_ne!(NodeRoleConfig::Primary, NodeRoleConfig::Replica);
        assert_ne!(NodeRoleConfig::Primary, NodeRoleConfig::Router);
    }
}
