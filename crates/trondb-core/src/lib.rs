pub mod error;
pub mod executor;
pub mod planner;
pub mod result;
pub mod store;
pub mod location;
pub mod types;

use std::path::PathBuf;

use error::EngineError;
use executor::Executor;
use result::QueryResult;
use store::FjallStore;
use trondb_wal::{WalConfig, WalRecovery, WalWriter};

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

        // Replay committed WAL records into Fjall before accepting queries
        let recovery = WalRecovery::recover(&config.wal.wal_dir).await?;
        let wal = WalWriter::open(config.wal).await?;
        let executor = Executor::new(store, wal);

        let replayed = executor.replay_wal_records(&recovery.records)?;
        if replayed > 0 {
            eprintln!("WAL recovery: replayed {replayed} records");
        }

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
    async fn wal_replay_recovers_data() {
        let dir = TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        let data_dir = dir.path().join("data");

        // Write data via WAL only (simulate crash before Fjall persist by
        // writing WAL records directly, then opening a fresh Fjall store)
        {
            // Create WAL records for a collection + entity
            tokio::fs::create_dir_all(&wal_dir).await.unwrap();
            let wal_config = trondb_wal::WalConfig {
                wal_dir: wal_dir.clone(),
                ..Default::default()
            };
            let writer = trondb_wal::WalWriter::open(wal_config).await.unwrap();

            // Tx1: create collection
            let tx1 = writer.next_tx_id();
            writer.append(trondb_wal::RecordType::TxBegin, "venues", tx1, 1, vec![]);
            let coll_payload = rmp_serde::to_vec_named(&serde_json::json!({
                "name": "venues",
                "dimensions": 3
            }))
            .unwrap();
            writer.append(
                trondb_wal::RecordType::SchemaCreateColl,
                "venues",
                tx1,
                1,
                coll_payload,
            );
            writer.commit(tx1).await.unwrap();

            // Tx2: insert entity
            let tx2 = writer.next_tx_id();
            writer.append(trondb_wal::RecordType::TxBegin, "venues", tx2, 1, vec![]);
            let entity = crate::types::Entity::new(crate::types::LogicalId::from_string("v1"))
                .with_metadata("name", Value::String("Recovered Venue".into()));
            let entity_payload = rmp_serde::to_vec_named(&entity).unwrap();
            writer.append(
                trondb_wal::RecordType::EntityWrite,
                "venues",
                tx2,
                1,
                entity_payload,
            );
            writer.commit(tx2).await.unwrap();
        }

        // Now open engine — Fjall store is empty, WAL has committed records.
        // Engine::open should replay WAL into Fjall.
        let config = EngineConfig {
            data_dir,
            wal: trondb_wal::WalConfig {
                wal_dir,
                ..Default::default()
            },
        };
        let engine = Engine::open(config).await.unwrap();

        // Verify the replayed data is queryable
        let result = engine
            .execute_tql("FETCH * FROM venues WHERE id = 'v1';")
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("name"),
            Some(&Value::String("Recovered Venue".into()))
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
