pub mod error;
pub mod executor;
pub mod planner;
pub mod result;
pub mod store;
pub mod types;

use std::path::PathBuf;

use error::EngineError;
use executor::Executor;
use result::QueryResult;
use store::FjallStore;
use trondb_wal::{WalConfig, WalWriter};

// ---------------------------------------------------------------------------
// Engine — public API
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub data_dir: PathBuf,
    pub wal: WalConfig,
}

pub struct Engine {
    executor: Executor,
}

impl Engine {
    pub async fn open(config: EngineConfig) -> Result<Self, EngineError> {
        let store = FjallStore::open(&config.data_dir)?;
        let wal = WalWriter::open(config.wal).await?;
        let executor = Executor::new(store, wal);
        Ok(Self { executor })
    }

    pub async fn execute_tql(&self, input: &str) -> Result<QueryResult, EngineError> {
        let stmt =
            trondb_tql::parse(input).map_err(|e| EngineError::InvalidQuery(e.to_string()))?;
        let plan = planner::plan(&stmt)?;
        self.executor.execute(&plan).await
    }

    pub fn collections(&self) -> Vec<String> {
        self.executor.collections()
    }
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;
    use tempfile::TempDir;

    async fn test_engine() -> (Engine, TempDir) {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("data"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
        };
        let engine = Engine::open(config).await.unwrap();
        (engine, dir)
    }

    #[tokio::test]
    async fn end_to_end_create_insert_fetch() {
        let (engine, _dir) = test_engine().await;

        engine
            .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name, city) VALUES ('v1', 'The Shard', 'London') VECTOR [0.1, 0.2, 0.3];",
            )
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name, city) VALUES ('v2', 'Old Trafford', 'Manchester') VECTOR [0.4, 0.5, 0.6];",
            )
            .await
            .unwrap();

        let result = engine
            .execute_tql("FETCH * FROM venues WHERE city = 'London';")
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("name"),
            Some(&Value::String("The Shard".into()))
        );
    }

    #[tokio::test]
    async fn search_returns_unsupported() {
        let (engine, _dir) = test_engine().await;

        let result = engine
            .execute_tql("SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] CONFIDENCE > 0.8;")
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn end_to_end_explain() {
        let (engine, _dir) = test_engine().await;

        let result = engine
            .execute_tql("EXPLAIN FETCH * FROM venues;")
            .await
            .unwrap();

        let mode_row = result
            .rows
            .iter()
            .find(|r| r.values.get("property") == Some(&Value::String("mode".into())))
            .expect("should have 'mode' property");

        assert_eq!(
            mode_row.values.get("value"),
            Some(&Value::String("Deterministic".into()))
        );
    }

    #[tokio::test]
    async fn data_survives_restart() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("data"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
        };

        // Write data
        {
            let engine = Engine::open(config.clone()).await.unwrap();
            engine
                .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
                .await
                .unwrap();
            engine
                .execute_tql(
                    "INSERT INTO venues (id, name) VALUES ('v1', 'The Shard') VECTOR [0.1, 0.2, 0.3];",
                )
                .await
                .unwrap();
        }

        // Reopen and verify
        {
            let engine = Engine::open(config).await.unwrap();
            let result = engine
                .execute_tql("FETCH * FROM venues WHERE id = 'v1';")
                .await
                .unwrap();
            assert_eq!(result.rows.len(), 1);
            assert_eq!(
                result.rows[0].values.get("name"),
                Some(&Value::String("The Shard".into()))
            );
        }
    }
}
