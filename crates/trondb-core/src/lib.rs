pub mod error;
pub mod executor;
pub mod hybrid;
pub mod planner;
pub mod result;
pub mod store;
pub mod edge;
pub mod field_index;
pub mod index;
pub mod location;
pub mod sparse_index;
pub mod types;

use std::path::PathBuf;
use std::sync::Arc;

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
    pub snapshot_interval_secs: u64,
}

pub struct Engine {
    executor: Executor,
    _snapshot_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Engine {
    pub async fn open(config: EngineConfig) -> Result<Self, EngineError> {
        let store = FjallStore::open(&config.data_dir)?;

        // Load Location Table from snapshot if available
        let snap_path = config.data_dir.join("location_table.snap");
        let (location, snap_lsn) = if snap_path.exists() {
            let bytes = tokio::fs::read(&snap_path).await
                .map_err(|e| EngineError::Storage(e.to_string()))?;
            let (lt, lsn) = location::LocationTable::restore(&bytes)?;
            eprintln!("Location Table: restored {} entries from snapshot (LSN {lsn})", lt.len());
            (lt, lsn)
        } else {
            (location::LocationTable::new(), 0)
        };

        // Replay committed WAL records
        let recovery = WalRecovery::recover(&config.wal.wal_dir).await?;
        let wal = WalWriter::open(config.wal).await?;
        let location = Arc::new(location);
        let executor = Executor::new(store, wal, Arc::clone(&location));

        // Filter records to only replay those after snapshot LSN
        let records_to_replay: Vec<_> = recovery
            .records
            .iter()
            .filter(|r| r.lsn > snap_lsn)
            .cloned()
            .collect();

        let replayed = executor.replay_wal_records(&records_to_replay)?;
        if replayed > 0 {
            eprintln!("WAL recovery: replayed {replayed} records");
        }

        // Rebuild HNSW indexes from Fjall
        for collection_name in executor.collections() {
            if let Ok(entities) = executor.scan_collection(&collection_name) {
                if let Ok(dims) = executor.get_collection_dimensions(&collection_name) {
                    for entity in &entities {
                        if !entity.representations.is_empty() {
                            let index = executor.indexes()
                                .entry(collection_name.clone())
                                .or_insert_with(|| crate::index::HnswIndex::new(dims));
                            for repr in &entity.representations {
                                index.insert(&entity.id, &repr.vector);
                            }
                        }
                    }
                }
            }
        }

        // Rebuild AdjacencyIndex from Fjall
        let edge_type_list = executor.list_edge_types();
        for et in &edge_type_list {
            if let Ok(edges) = executor.scan_edges(&et.name) {
                for edge in &edges {
                    executor.adjacency().insert(
                        &edge.from_id,
                        &edge.edge_type,
                        &edge.to_id,
                        edge.confidence,
                    );
                }
            }
            executor.edge_types().insert(et.name.clone(), et.clone());
        }

        // Spawn background snapshot task
        let snapshot_handle = if config.snapshot_interval_secs > 0 {
            let interval = std::time::Duration::from_secs(config.snapshot_interval_secs);
            let snap_path = config.data_dir.join("location_table.snap");
            let location_arc = Arc::clone(&location);
            let wal_head = executor.wal_head_lsn();

            Some(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.tick().await; // skip first immediate tick
                loop {
                    ticker.tick().await;
                    let lsn = wal_head; // In Phase 3, WAL head at startup is sufficient
                    match location_arc.snapshot(lsn) {
                        Ok(bytes) => {
                            if let Err(e) = tokio::fs::write(&snap_path, &bytes).await {
                                eprintln!("Location Table snapshot failed: {e}");
                            }
                        }
                        Err(e) => {
                            eprintln!("Location Table snapshot failed: {e}");
                        }
                    }
                }
            }))
        } else {
            None
        };

        Ok(Self {
            executor,
            _snapshot_handle: snapshot_handle,
        })
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

    pub fn location(&self) -> &location::LocationTable {
        self.executor.location()
    }

    pub fn wal_head_lsn(&self) -> u64 {
        self.executor.wal_head_lsn()
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
            snapshot_interval_secs: 0,
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
    async fn search_returns_results() {
        let (engine, _dir) = test_engine().await;

        engine
            .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'The Shard') VECTOR [1.0, 0.0, 0.0];",
            )
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v2', 'Big Ben') VECTOR [0.0, 1.0, 0.0];",
            )
            .await
            .unwrap();

        let result = engine
            .execute_tql("SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] LIMIT 2;")
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 2);
        // First result should be v1 (exact match)
        assert_eq!(
            result.rows[0].values.get("id"),
            Some(&Value::String("v1".into()))
        );
        // Should have a score
        assert!(result.rows[0].score.is_some());
        assert!(result.rows[0].score.unwrap() > 0.9);
    }

    #[tokio::test]
    async fn search_confidence_filtering() {
        let (engine, _dir) = test_engine().await;

        engine
            .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'Close') VECTOR [0.9, 0.1, 0.0];",
            )
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v2', 'Far') VECTOR [0.0, 0.0, 1.0];",
            )
            .await
            .unwrap();

        // High confidence threshold should filter out the distant vector
        let result = engine
            .execute_tql("SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] CONFIDENCE > 0.5 LIMIT 10;")
            .await
            .unwrap();

        // Only the close vector should pass the threshold
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("id"),
            Some(&Value::String("v1".into()))
        );
    }

    #[tokio::test]
    async fn search_empty_collection_returns_empty() {
        let (engine, _dir) = test_engine().await;

        engine
            .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
            .await
            .unwrap();

        let result = engine
            .execute_tql("SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] LIMIT 5;")
            .await
            .unwrap();

        assert!(result.rows.is_empty());
    }

    #[tokio::test]
    async fn explain_search_shows_hnsw() {
        let (engine, _dir) = test_engine().await;

        let result = engine
            .execute_tql("EXPLAIN SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] LIMIT 5;")
            .await
            .unwrap();

        let strategy_row = result
            .rows
            .iter()
            .find(|r| r.values.get("property") == Some(&Value::String("strategy".into())))
            .expect("should have 'strategy' property");

        assert_eq!(
            strategy_row.values.get("value"),
            Some(&Value::String("HNSW".into()))
        );

        let mode_row = result
            .rows
            .iter()
            .find(|r| r.values.get("property") == Some(&Value::String("mode".into())))
            .expect("should have 'mode' property");

        assert_eq!(
            mode_row.values.get("value"),
            Some(&Value::String("Probabilistic".into()))
        );
    }

    #[tokio::test]
    async fn search_works_after_restart() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("data"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
        };

        // Insert data
        {
            let engine = Engine::open(config.clone()).await.unwrap();
            engine
                .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
                .await
                .unwrap();
            engine
                .execute_tql(
                    "INSERT INTO venues (id, name) VALUES ('v1', 'Test') VECTOR [1.0, 0.0, 0.0];",
                )
                .await
                .unwrap();
        }

        // Reopen — HNSW index should be rebuilt from Fjall
        {
            let engine = Engine::open(config).await.unwrap();
            let result = engine
                .execute_tql("SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] LIMIT 1;")
                .await
                .unwrap();

            assert_eq!(result.rows.len(), 1);
            assert_eq!(
                result.rows[0].values.get("id"),
                Some(&Value::String("v1".into()))
            );
        }
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
            snapshot_interval_secs: 0,
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
            snapshot_interval_secs: 0,
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

    #[tokio::test]
    async fn insert_with_vector_creates_location_entry() {
        let (engine, _dir) = test_engine().await;

        engine
            .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'Test') VECTOR [0.1, 0.2, 0.3];",
            )
            .await
            .unwrap();

        // Check Location Table has an entry for v1 repr 0
        let key = crate::location::ReprKey {
            entity_id: crate::types::LogicalId::from_string("v1"),
            repr_index: 0,
        };
        let desc = engine.location().get(&key).expect("should have location entry");
        assert_eq!(desc.tier, crate::location::Tier::Fjall);
        assert_eq!(desc.state, crate::location::LocState::Clean);
        assert_eq!(desc.encoding, crate::location::Encoding::Float32);
    }

    #[tokio::test]
    async fn insert_without_vector_has_no_location_entry() {
        let (engine, _dir) = test_engine().await;

        engine
            .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
            .await
            .unwrap();

        engine
            .execute_tql("INSERT INTO venues (id, name) VALUES ('v1', 'Test');")
            .await
            .unwrap();

        assert_eq!(engine.location().len(), 0);
    }

    #[tokio::test]
    async fn create_edge_type_and_traverse() {
        let (engine, _dir) = test_engine().await;

        engine.execute_tql("CREATE COLLECTION people WITH DIMENSIONS 3;").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p1', 'Alice');").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p2', 'Bob');").await.unwrap();

        engine.execute_tql("CREATE EDGE knows FROM people TO people;").await.unwrap();
        engine.execute_tql("INSERT EDGE knows FROM 'p1' TO 'p2';").await.unwrap();

        let result = engine.execute_tql("TRAVERSE knows FROM 'p1';").await.unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("name"),
            Some(&Value::String("Bob".into()))
        );
    }

    #[tokio::test]
    async fn delete_edge_removes_from_traverse() {
        let (engine, _dir) = test_engine().await;

        engine.execute_tql("CREATE COLLECTION people WITH DIMENSIONS 3;").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p1', 'Alice');").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p2', 'Bob');").await.unwrap();

        engine.execute_tql("CREATE EDGE knows FROM people TO people;").await.unwrap();
        engine.execute_tql("INSERT EDGE knows FROM 'p1' TO 'p2';").await.unwrap();
        engine.execute_tql("DELETE EDGE knows FROM 'p1' TO 'p2';").await.unwrap();

        let result = engine.execute_tql("TRAVERSE knows FROM 'p1';").await.unwrap();
        assert!(result.rows.is_empty());
    }

    #[tokio::test]
    async fn insert_edge_with_metadata() {
        let (engine, _dir) = test_engine().await;

        engine.execute_tql("CREATE COLLECTION people WITH DIMENSIONS 3;").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p1', 'Alice');").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p2', 'Bob');").await.unwrap();

        engine.execute_tql("CREATE EDGE knows FROM people TO people;").await.unwrap();
        engine.execute_tql("INSERT EDGE knows FROM 'p1' TO 'p2' WITH (since = '2024');").await.unwrap();

        // Edge was created successfully (metadata stored in Fjall)
        let result = engine.execute_tql("TRAVERSE knows FROM 'p1';").await.unwrap();
        assert_eq!(result.rows.len(), 1);
    }

    #[tokio::test]
    async fn insert_edge_nonexistent_type_fails() {
        let (engine, _dir) = test_engine().await;

        engine.execute_tql("CREATE COLLECTION people WITH DIMENSIONS 3;").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p1', 'Alice');").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('p2', 'Bob');").await.unwrap();

        let result = engine.execute_tql("INSERT EDGE knows FROM 'p1' TO 'p2';").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn traverse_nonexistent_edge_type_fails() {
        let (engine, _dir) = test_engine().await;
        let result = engine.execute_tql("TRAVERSE knows FROM 'p1';").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn traverse_depth_gt_1_fails() {
        let (engine, _dir) = test_engine().await;

        engine.execute_tql("CREATE COLLECTION people WITH DIMENSIONS 3;").await.unwrap();
        engine.execute_tql("CREATE EDGE knows FROM people TO people;").await.unwrap();

        let result = engine.execute_tql("TRAVERSE knows FROM 'p1' DEPTH 2;").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn explain_traverse_shows_adjacency_index() {
        let (engine, _dir) = test_engine().await;

        let result = engine
            .execute_tql("EXPLAIN TRAVERSE knows FROM 'p1';")
            .await
            .unwrap();

        let strategy_row = result.rows.iter()
            .find(|r| r.values.get("property") == Some(&Value::String("strategy".into())))
            .expect("should have 'strategy' property");
        assert_eq!(
            strategy_row.values.get("value"),
            Some(&Value::String("AdjacencyIndex".into()))
        );

        let mode_row = result.rows.iter()
            .find(|r| r.values.get("property") == Some(&Value::String("mode".into())))
            .expect("should have 'mode' property");
        assert_eq!(
            mode_row.values.get("value"),
            Some(&Value::String("Deterministic".into()))
        );
    }

    #[tokio::test]
    async fn edges_survive_restart() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("data"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
        };

        // Insert edge
        {
            let engine = Engine::open(config.clone()).await.unwrap();
            engine.execute_tql("CREATE COLLECTION people WITH DIMENSIONS 3;").await.unwrap();
            engine.execute_tql("INSERT INTO people (id, name) VALUES ('p1', 'Alice');").await.unwrap();
            engine.execute_tql("INSERT INTO people (id, name) VALUES ('p2', 'Bob');").await.unwrap();
            engine.execute_tql("CREATE EDGE knows FROM people TO people;").await.unwrap();
            engine.execute_tql("INSERT EDGE knows FROM 'p1' TO 'p2';").await.unwrap();
        }

        // Reopen — AdjacencyIndex should be rebuilt
        {
            let engine = Engine::open(config).await.unwrap();
            let result = engine.execute_tql("TRAVERSE knows FROM 'p1';").await.unwrap();
            assert_eq!(result.rows.len(), 1);
            assert_eq!(
                result.rows[0].values.get("name"),
                Some(&Value::String("Bob".into()))
            );
        }
    }

    #[tokio::test]
    async fn location_table_snapshot_and_restore() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        let config = EngineConfig {
            data_dir: data_dir.clone(),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
        };

        // Insert data
        {
            let engine = Engine::open(config.clone()).await.unwrap();
            engine
                .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
                .await
                .unwrap();
            engine
                .execute_tql(
                    "INSERT INTO venues (id, name) VALUES ('v1', 'Test') VECTOR [0.1, 0.2, 0.3];",
                )
                .await
                .unwrap();

            // Manually write snapshot
            let snap_bytes = engine.location().snapshot(engine.wal_head_lsn()).unwrap();
            std::fs::write(data_dir.join("location_table.snap"), &snap_bytes).unwrap();
        }

        // Reopen — should restore from snapshot
        {
            let engine = Engine::open(config).await.unwrap();
            let key = crate::location::ReprKey {
                entity_id: crate::types::LogicalId::from_string("v1"),
                repr_index: 0,
            };
            let desc = engine.location().get(&key).expect("should have location entry after restore");
            assert_eq!(desc.tier, crate::location::Tier::Fjall);
        }
    }
}
