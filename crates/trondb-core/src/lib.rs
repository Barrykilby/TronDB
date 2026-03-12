pub mod error;
pub mod executor;
pub mod hnsw_snapshot;
pub mod hybrid;
pub mod planner;
pub mod quantise;
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
use location::Tier;
use result::QueryResult;
use store::FjallStore;
use trondb_wal::{WalConfig, WalRecord, WalRecovery, WalWriter};
use types::LogicalId;

// ---------------------------------------------------------------------------
// Engine — public API
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub data_dir: PathBuf,
    pub wal: WalConfig,
    pub snapshot_interval_secs: u64,
    pub hnsw_snapshot_interval_secs: u64,
}

pub struct Engine {
    executor: Executor,
    data_dir: PathBuf,
    _snapshot_handle: Option<tokio::task::JoinHandle<()>>,
    _hnsw_snapshot_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Engine {
    pub async fn open(config: EngineConfig) -> Result<(Self, Vec<WalRecord>), EngineError> {
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

        let (replayed, unhandled_records) = executor.replay_wal_records(&records_to_replay)?;
        if replayed > 0 {
            eprintln!("WAL recovery: replayed {replayed} records");
        }

        // Load schemas from Fjall into memory
        for schema in executor.store().list_collection_schemas() {
            executor.schemas().insert(schema.name.clone(), schema);
        }

        // Try loading HNSW snapshots before full Fjall scan
        let hnsw_snap_dir = config.data_dir.join("hnsw_snapshots");
        let mut hnsw_snapshot_lsns: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();

        // Rebuild HNSW indexes and SparseIndexes from Fjall
        for collection_name in executor.collections() {
            if let Some(schema_ref) = executor.schemas().get(&collection_name) {
                // Instantiate HNSW indexes for dense representations
                // Try snapshot first, fall back to empty index
                for repr in &schema_ref.representations {
                    if !repr.sparse {
                        if let Some(dims) = repr.dimensions {
                            let hnsw_key = format!("{}:{}", collection_name, repr.name);
                            if !executor.indexes().contains_key(&hnsw_key) {
                                // Try loading from snapshot
                                let mut loaded = false;
                                if hnsw_snap_dir.exists() {
                                    match hnsw_snapshot::load_snapshot(
                                        &hnsw_snap_dir,
                                        &collection_name,
                                        &repr.name,
                                        dims,
                                    ) {
                                        Ok(Some((index, meta))) => {
                                            eprintln!(
                                                "HNSW snapshot: restored {}:{} ({} entities, LSN {})",
                                                collection_name, repr.name, meta.entity_count, meta.lsn
                                            );
                                            hnsw_snapshot_lsns.insert(hnsw_key.clone(), meta.lsn);
                                            executor.indexes().insert(hnsw_key.clone(), index);
                                            loaded = true;
                                        }
                                        Ok(None) => {
                                            // No snapshot files — fall through to empty index
                                        }
                                        Err(e) => {
                                            eprintln!(
                                                "HNSW snapshot: failed to load {}:{}: {e} — rebuilding from scratch",
                                                collection_name, repr.name
                                            );
                                        }
                                    }
                                }
                                if !loaded {
                                    executor.indexes().insert(
                                        hnsw_key,
                                        crate::index::HnswIndex::new(dims),
                                    );
                                }
                            }
                        }
                    }
                }
                // Instantiate SparseIndexes for sparse representations
                for repr in &schema_ref.representations {
                    if repr.sparse {
                        let sparse_key = format!("{}:{}", collection_name, repr.name);
                        executor.sparse_indexes()
                            .entry(sparse_key)
                            .or_default();
                    }
                }
                // Instantiate FieldIndexes for declared indexes
                for idx in &schema_ref.indexes {
                    let fidx_key = format!("{}:{}", collection_name, idx.name);
                    if !executor.field_indexes().contains_key(&fidx_key) {
                        if let Ok(partition) = executor.store().open_field_index_partition(&collection_name, &idx.name) {
                            let field_types: Vec<(String, types::FieldType)> = idx.fields.iter()
                                .filter_map(|f| {
                                    schema_ref.fields.iter()
                                        .find(|sf| sf.name == *f)
                                        .map(|sf| (sf.name.clone(), sf.field_type.clone()))
                                })
                                .collect();
                            executor.field_indexes().insert(fidx_key, crate::field_index::FieldIndex::new(partition, field_types));
                        }
                    }
                }
            }
            // Rebuild index contents from stored entities.
            // Skip HNSW inserts for collections with snapshot-loaded indexes — those
            // are already populated and will be caught up via incremental WAL replay.
            if let Ok(entities) = executor.scan_collection(&collection_name) {
                for entity in &entities {
                    for repr in &entity.representations {
                        match &repr.vector {
                            types::VectorData::Dense(ref vec_f32) => {
                                let hnsw_key = format!("{}:{}", collection_name, repr.name);
                                if !hnsw_snapshot_lsns.contains_key(&hnsw_key) {
                                    if let Some(index) = executor.indexes().get(&hnsw_key) {
                                        index.insert(&entity.id, vec_f32);
                                    }
                                }
                            }
                            types::VectorData::Sparse(ref sv) => {
                                let sparse_key = format!("{}:{}", collection_name, repr.name);
                                if let Some(sidx) = executor.sparse_indexes().get(&sparse_key) {
                                    sidx.insert(&entity.id, sv);
                                }
                            }
                        }
                    }
                    // Rebuild field indexes
                    for entry in executor.field_indexes().iter() {
                        if entry.key().starts_with(&format!("{}:", collection_name)) {
                            let values: Vec<types::Value> = entry.field_types().iter()
                                .filter_map(|(fname, _)| entity.metadata.get(fname).cloned())
                                .collect();
                            if values.len() == entry.field_types().len() {
                                let _ = entry.insert(&entity.id, &values);
                            }
                        }
                    }
                }
            }
        }

        // Incremental WAL catch-up for snapshot-loaded HNSW indexes.
        // For each snapshot-loaded index, replay EntityWrite/EntityDelete WAL
        // records with LSN > snapshot LSN to bring the index up to date.
        if !hnsw_snapshot_lsns.is_empty() {
            for record in &records_to_replay {
                match record.record_type {
                    trondb_wal::RecordType::EntityWrite => {
                        // Check if any snapshot-loaded index covers this collection
                        let has_snapshot = hnsw_snapshot_lsns.keys().any(|k| {
                            k.starts_with(&format!("{}:", record.collection))
                        });
                        if !has_snapshot {
                            continue;
                        }
                        if let Ok(entity) = rmp_serde::from_slice::<types::Entity>(&record.payload) {
                            for repr in &entity.representations {
                                if let types::VectorData::Dense(ref vec_f32) = repr.vector {
                                    let hnsw_key =
                                        format!("{}:{}", record.collection, repr.name);
                                    if let Some(&snap_lsn) = hnsw_snapshot_lsns.get(&hnsw_key)
                                    {
                                        if record.lsn > snap_lsn {
                                            if let Some(index) =
                                                executor.indexes().get(&hnsw_key)
                                            {
                                                index.insert(&entity.id, vec_f32);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    trondb_wal::RecordType::EntityDelete => {
                        let has_snapshot = hnsw_snapshot_lsns.keys().any(|k| {
                            k.starts_with(&format!("{}:", record.collection))
                        });
                        if !has_snapshot {
                            continue;
                        }
                        #[derive(serde::Deserialize)]
                        struct DeletePayload {
                            entity_id: String,
                            collection: String,
                        }
                        if let Ok(payload) =
                            rmp_serde::from_slice::<DeletePayload>(&record.payload)
                        {
                            let entity_id = LogicalId::from_string(&payload.entity_id);
                            for (key, snap_lsn) in &hnsw_snapshot_lsns {
                                if key.starts_with(&format!("{}:", payload.collection))
                                    && record.lsn > *snap_lsn
                                {
                                    if let Some(index) = executor.indexes().get(key) {
                                        index.remove(&entity_id);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
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
                        edge.created_at,
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

        // Spawn background HNSW snapshot task
        let hnsw_snapshot_handle = if config.hnsw_snapshot_interval_secs > 0 {
            let interval = std::time::Duration::from_secs(config.hnsw_snapshot_interval_secs);
            let snap_dir = config.data_dir.join("hnsw_snapshots");
            let indexes_arc = Arc::clone(executor.indexes());
            let wal_arc = executor.wal_writer();

            Some(tokio::spawn(async move {
                let _ = std::fs::create_dir_all(&snap_dir);
                let mut ticker = tokio::time::interval(interval);
                ticker.tick().await; // skip first immediate tick
                loop {
                    ticker.tick().await;
                    let lsn = wal_arc.head_lsn();
                    for entry in indexes_arc.iter() {
                        if let Some((collection, repr_name)) = entry.key().split_once(':') {
                            if let Err(e) = crate::hnsw_snapshot::save_snapshot(
                                &snap_dir, collection, repr_name, entry.value(), lsn,
                            ) {
                                eprintln!("[hnsw_snapshot] periodic save failed for {}: {e}", entry.key());
                            }
                        }
                    }
                }
            }))
        } else {
            None
        };

        Ok((Self {
            executor,
            data_dir: config.data_dir.clone(),
            _snapshot_handle: snapshot_handle,
            _hnsw_snapshot_handle: hnsw_snapshot_handle,
        }, unhandled_records))
    }

    pub fn entity_count(&self) -> usize {
        self.executor.entity_count()
    }

    pub fn collection_count(&self) -> usize {
        self.executor.collection_count()
    }

    /// Execute a pre-planned query. Used by the routing layer.
    pub async fn execute(&self, plan: &planner::Plan) -> Result<QueryResult, EngineError> {
        self.executor.execute(plan).await
    }

    /// Parse TQL and produce a Plan without executing.
    pub fn parse_and_plan(&self, tql: &str) -> Result<planner::Plan, EngineError> {
        let stmt = trondb_tql::parse(tql)
            .map_err(|e| EngineError::InvalidQuery(e.to_string()))?;
        planner::plan(&stmt, self.executor.schemas())
    }

    pub async fn execute_tql(&self, input: &str) -> Result<QueryResult, EngineError> {
        let stmt =
            trondb_tql::parse(input).map_err(|e| EngineError::InvalidQuery(e.to_string()))?;
        let plan = planner::plan(&stmt, self.executor.schemas())?;
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

    /// Save all HNSW index snapshots to disk.
    pub fn save_hnsw_snapshots(&self) -> Result<(), EngineError> {
        let snap_dir = self.data_dir.join("hnsw_snapshots");
        std::fs::create_dir_all(&snap_dir)
            .map_err(|e| EngineError::Storage(format!("create snapshot dir: {e}")))?;

        let lsn = self.executor.wal_head_lsn();

        for entry in self.executor.indexes().iter() {
            let key = entry.key();
            // Key format: "collection:repr_name"
            if let Some((collection, repr_name)) = key.split_once(':') {
                if let Err(e) = hnsw_snapshot::save_snapshot(
                    &snap_dir, collection, repr_name, entry.value(), lsn,
                ) {
                    eprintln!("[hnsw_snapshot] save failed for {key}: {e}");
                }
            }
        }
        Ok(())
    }

    /// Expose the WAL writer for the routing layer to log affinity mutations.
    pub fn wal_writer(&self) -> std::sync::Arc<WalWriter> {
        self.executor.wal_writer()
    }

    /// Read entity data from a specific tier's partition.
    pub fn read_tiered(
        &self,
        collection: &str,
        entity_id: &LogicalId,
        tier: Tier,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        self.executor.store().read_tiered(collection, entity_id, tier)
    }

    /// Write entity data to a specific tier's partition.
    pub fn write_tiered(
        &self,
        collection: &str,
        entity_id: &LogicalId,
        tier: Tier,
        data: &[u8],
    ) -> Result<(), EngineError> {
        self.executor.store().write_tiered(collection, entity_id, tier, data)
    }

    /// Delete entity from a specific tier's partition.
    pub fn delete_from_tier(
        &self,
        collection: &str,
        entity_id: &LogicalId,
        tier: Tier,
    ) -> Result<(), EngineError> {
        self.executor.store().delete_from_tier(collection, entity_id, tier)
    }

    /// Count entities in a specific tier for a collection.
    pub fn tier_entity_count(
        &self,
        collection: &str,
        tier: Tier,
    ) -> Result<usize, EngineError> {
        self.executor.store().tier_entity_count(collection, tier)
    }

    /// Remove an entity from the HNSW index (tombstone).
    pub fn remove_from_hnsw(&self, collection: &str, entity_id: &LogicalId) {
        if let Some(idx) = self.executor.indexes().get(collection) {
            idx.remove(entity_id);
        }
    }

    /// Insert a vector into the HNSW index for a collection.
    pub fn insert_into_hnsw(&self, collection: &str, entity_id: &LogicalId, vector: &[f32]) {
        if let Some(idx) = self.executor.indexes().get(collection) {
            idx.insert(entity_id, vector);
        }
    }

    pub fn list_edge_types(&self) -> Vec<crate::edge::EdgeType> {
        self.executor.list_edge_types()
    }

    pub fn scan_edges(&self, edge_type: &str) -> Result<Vec<crate::edge::Edge>, EngineError> {
        self.executor.scan_edges(edge_type)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Abort background snapshot tasks to prevent them running on a dropping engine
        if let Some(h) = self._hnsw_snapshot_handle.take() {
            h.abort();
        }
        if let Some(h) = self._snapshot_handle.take() {
            h.abort();
        }
        // Save final HNSW snapshot
        if let Err(e) = self.save_hnsw_snapshots() {
            eprintln!("[hnsw_snapshot] shutdown save failed: {e}");
        }
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
            hnsw_snapshot_interval_secs: 0,
        };
        let (engine, _) = Engine::open(config).await.unwrap();
        (engine, dir)
    }

    /// Helper: CREATE COLLECTION with a single dense representation using new block syntax.
    async fn create_simple_collection(engine: &Engine, name: &str, dims: usize) {
        let tql = format!(
            "CREATE COLLECTION {name} (\
                REPRESENTATION default DIMENSIONS {dims} METRIC COSINE\
            );"
        );
        engine.execute_tql(&tql).await.unwrap();
    }

    #[tokio::test]
    async fn end_to_end_create_insert_fetch() {
        let (engine, _dir) = test_engine().await;

        create_simple_collection(&engine, "venues", 3).await;

        engine
            .execute_tql(
                "INSERT INTO venues (id, name, city) VALUES ('v1', 'The Shard', 'London') REPRESENTATION default VECTOR [0.1, 0.2, 0.3];",
            )
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name, city) VALUES ('v2', 'Old Trafford', 'Manchester') REPRESENTATION default VECTOR [0.4, 0.5, 0.6];",
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

        create_simple_collection(&engine, "venues", 3).await;

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'The Shard') REPRESENTATION default VECTOR [1.0, 0.0, 0.0];",
            )
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v2', 'Big Ben') REPRESENTATION default VECTOR [0.0, 1.0, 0.0];",
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

        create_simple_collection(&engine, "venues", 3).await;

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'Close') REPRESENTATION default VECTOR [0.9, 0.1, 0.0];",
            )
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v2', 'Far') REPRESENTATION default VECTOR [0.0, 0.0, 1.0];",
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

        create_simple_collection(&engine, "venues", 3).await;

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
            Some(&Value::String("HnswSearch".into()))
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
            hnsw_snapshot_interval_secs: 0,
        };

        // Insert data
        {
            let (engine, _) = Engine::open(config.clone()).await.unwrap();
            create_simple_collection(&engine, "venues", 3).await;
            engine
                .execute_tql(
                    "INSERT INTO venues (id, name) VALUES ('v1', 'Test') REPRESENTATION default VECTOR [1.0, 0.0, 0.0];",
                )
                .await
                .unwrap();
        }

        // Reopen — HNSW index should be rebuilt from Fjall
        {
            let (engine, _) = Engine::open(config).await.unwrap();
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

            // Tx1: create collection (new schema format)
            let tx1 = writer.next_tx_id();
            writer.append(trondb_wal::RecordType::TxBegin, "venues", tx1, 1, vec![]);
            let schema = crate::types::CollectionSchema {
                name: "venues".into(),
                representations: vec![crate::types::StoredRepresentation {
                    name: "default".into(),
                    model: None,
                    dimensions: Some(3),
                    metric: crate::types::Metric::Cosine,
                    sparse: false,
                }],
                fields: vec![],
                indexes: vec![],
            };
            let coll_payload = rmp_serde::to_vec_named(&schema).unwrap();
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
            hnsw_snapshot_interval_secs: 0,
        };
        let (engine, _) = Engine::open(config).await.unwrap();

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
            hnsw_snapshot_interval_secs: 0,
        };

        // Write data
        {
            let (engine, _) = Engine::open(config.clone()).await.unwrap();
            create_simple_collection(&engine, "venues", 3).await;
            engine
                .execute_tql(
                    "INSERT INTO venues (id, name) VALUES ('v1', 'The Shard') REPRESENTATION default VECTOR [0.1, 0.2, 0.3];",
                )
                .await
                .unwrap();
        }

        // Reopen and verify
        {
            let (engine, _) = Engine::open(config).await.unwrap();
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

        create_simple_collection(&engine, "venues", 3).await;

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'Test') REPRESENTATION default VECTOR [0.1, 0.2, 0.3];",
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

        create_simple_collection(&engine, "venues", 3).await;

        engine
            .execute_tql("INSERT INTO venues (id, name) VALUES ('v1', 'Test');")
            .await
            .unwrap();

        assert_eq!(engine.location().len(), 0);
    }

    #[tokio::test]
    async fn create_edge_type_and_traverse() {
        let (engine, _dir) = test_engine().await;

        create_simple_collection(&engine, "people", 3).await;
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

        create_simple_collection(&engine, "people", 3).await;
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

        create_simple_collection(&engine, "people", 3).await;
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

        create_simple_collection(&engine, "people", 3).await;
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
    async fn traverse_depth_gt_1_returns_multi_hop_results() {
        let (engine, _dir) = test_engine().await;

        create_simple_collection(&engine, "people", 3).await;
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('a', 'Alice');").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('b', 'Bob');").await.unwrap();
        engine.execute_tql("INSERT INTO people (id, name) VALUES ('c', 'Carol');").await.unwrap();

        engine.execute_tql("CREATE EDGE knows FROM people TO people;").await.unwrap();
        engine.execute_tql("INSERT EDGE knows FROM 'a' TO 'b';").await.unwrap();
        engine.execute_tql("INSERT EDGE knows FROM 'b' TO 'c';").await.unwrap();

        // Depth 2 from 'a' should reach both b (hop 1) and c (hop 2)
        let result = engine.execute_tql("TRAVERSE knows FROM 'a' DEPTH 2;").await.unwrap();
        assert_eq!(result.rows.len(), 2);
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
            hnsw_snapshot_interval_secs: 0,
        };

        // Insert edge
        {
            let (engine, _) = Engine::open(config.clone()).await.unwrap();
            create_simple_collection(&engine, "people", 3).await;
            engine.execute_tql("INSERT INTO people (id, name) VALUES ('p1', 'Alice');").await.unwrap();
            engine.execute_tql("INSERT INTO people (id, name) VALUES ('p2', 'Bob');").await.unwrap();
            engine.execute_tql("CREATE EDGE knows FROM people TO people;").await.unwrap();
            engine.execute_tql("INSERT EDGE knows FROM 'p1' TO 'p2';").await.unwrap();
        }

        // Reopen — AdjacencyIndex should be rebuilt
        {
            let (engine, _) = Engine::open(config).await.unwrap();
            let result = engine.execute_tql("TRAVERSE knows FROM 'p1';").await.unwrap();
            assert_eq!(result.rows.len(), 1);
            assert_eq!(
                result.rows[0].values.get("name"),
                Some(&Value::String("Bob".into()))
            );
        }
    }

    #[tokio::test]
    async fn sparse_index_survives_restart() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("data"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
            hnsw_snapshot_interval_secs: 0,
        };

        // Phase 1: Create collection with sparse repr, insert entity
        {
            let (engine, _) = Engine::open(config.clone()).await.unwrap();
            engine.execute_tql("CREATE COLLECTION docs (
                REPRESENTATION sparse_title METRIC INNER_PRODUCT SPARSE true
            );").await.unwrap();
            engine.execute_tql("INSERT INTO docs (id, title) VALUES ('d1', 'Hello')
                REPRESENTATION sparse_title SPARSE [1:0.8, 42:0.5];").await.unwrap();
        }

        // Phase 2: Reopen — sparse index should be rebuilt
        {
            let (engine, _) = Engine::open(config).await.unwrap();
            let result = engine.execute_tql("SEARCH docs NEAR SPARSE [1:1.0] LIMIT 5;").await.unwrap();
            assert_eq!(result.rows.len(), 1);
            assert_eq!(
                result.rows[0].values.get("id"),
                Some(&Value::String("d1".into()))
            );
        }
    }

    #[tokio::test]
    async fn field_index_survives_restart() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("data"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
            hnsw_snapshot_interval_secs: 0,
        };

        {
            let (engine, _) = Engine::open(config.clone()).await.unwrap();
            engine.execute_tql("CREATE COLLECTION venues (
                REPRESENTATION identity DIMENSIONS 3 METRIC COSINE,
                FIELD city TEXT,
                INDEX idx_city ON (city)
            );").await.unwrap();
            engine.execute_tql("INSERT INTO venues (id, city) VALUES ('v1', 'London')
                REPRESENTATION identity VECTOR [0.1, 0.2, 0.3];").await.unwrap();
            engine.execute_tql("INSERT INTO venues (id, city) VALUES ('v2', 'Paris')
                REPRESENTATION identity VECTOR [0.4, 0.5, 0.6];").await.unwrap();
        }

        {
            let (engine, _) = Engine::open(config).await.unwrap();
            let result = engine.execute_tql("FETCH * FROM venues WHERE city = 'London';").await.unwrap();
            assert_eq!(result.rows.len(), 1);
            assert_eq!(
                result.rows[0].values.get("id"),
                Some(&Value::String("v1".into()))
            );
        }
    }

    #[tokio::test]
    async fn hybrid_search_works_after_restart() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("data"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
            hnsw_snapshot_interval_secs: 0,
        };

        {
            let (engine, _) = Engine::open(config.clone()).await.unwrap();
            engine.execute_tql("CREATE COLLECTION docs (
                REPRESENTATION dense DIMENSIONS 3 METRIC COSINE,
                REPRESENTATION sparse METRIC INNER_PRODUCT SPARSE true
            );").await.unwrap();
            engine.execute_tql("INSERT INTO docs (id) VALUES ('d1')
                REPRESENTATION dense VECTOR [0.1, 0.2, 0.3]
                REPRESENTATION sparse SPARSE [1:0.8];").await.unwrap();
        }

        {
            let (engine, _) = Engine::open(config).await.unwrap();
            let result = engine.execute_tql(
                "SEARCH docs NEAR VECTOR [0.1, 0.2, 0.3] NEAR SPARSE [1:1.0] LIMIT 5;"
            ).await.unwrap();
            assert!(!result.rows.is_empty());
        }
    }

    #[tokio::test]
    async fn create_collection_with_fields_and_indexes() {
        let (engine, _dir) = test_engine().await;
        engine.execute_tql("CREATE COLLECTION venues (
            REPRESENTATION identity DIMENSIONS 3 METRIC COSINE,
            FIELD city TEXT,
            FIELD active BOOL,
            INDEX idx_city ON (city)
        );").await.unwrap();

        engine.execute_tql("INSERT INTO venues (id, city, active) VALUES ('v1', 'London', true)
            REPRESENTATION identity VECTOR [0.1, 0.2, 0.3];").await.unwrap();
        engine.execute_tql("INSERT INTO venues (id, city, active) VALUES ('v2', 'Paris', true)
            REPRESENTATION identity VECTOR [0.4, 0.5, 0.6];").await.unwrap();

        let result = engine.execute_tql("FETCH * FROM venues WHERE city = 'London';").await.unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].values.get("city"), Some(&Value::String("London".into())));
    }

    #[tokio::test]
    async fn sparse_search_returns_ranked() {
        let (engine, _dir) = test_engine().await;
        engine.execute_tql("CREATE COLLECTION docs (
            REPRESENTATION keywords METRIC INNER_PRODUCT SPARSE true
        );").await.unwrap();

        engine.execute_tql("INSERT INTO docs (id, title) VALUES ('d1', 'Rust')
            REPRESENTATION keywords SPARSE [1:0.9, 2:0.1];").await.unwrap();
        engine.execute_tql("INSERT INTO docs (id, title) VALUES ('d2', 'Python')
            REPRESENTATION keywords SPARSE [1:0.1, 3:0.8];").await.unwrap();
        engine.execute_tql("INSERT INTO docs (id, title) VALUES ('d3', 'Go')
            REPRESENTATION keywords SPARSE [2:0.5, 3:0.5];").await.unwrap();

        let result = engine.execute_tql("SEARCH docs NEAR SPARSE [1:1.0] LIMIT 2;").await.unwrap();
        assert_eq!(result.rows.len(), 2);
        // d1 should rank higher (weight 0.9 on dim 1)
        assert_eq!(result.rows[0].values.get("id"), Some(&Value::String("d1".into())));
    }

    #[tokio::test]
    async fn hybrid_search_merges_dense_sparse() {
        let (engine, _dir) = test_engine().await;
        engine.execute_tql("CREATE COLLECTION docs (
            REPRESENTATION dense DIMENSIONS 3 METRIC COSINE,
            REPRESENTATION sparse METRIC INNER_PRODUCT SPARSE true
        );").await.unwrap();

        engine.execute_tql("INSERT INTO docs (id) VALUES ('d1')
            REPRESENTATION dense VECTOR [1.0, 0.0, 0.0]
            REPRESENTATION sparse SPARSE [1:0.9];").await.unwrap();
        engine.execute_tql("INSERT INTO docs (id) VALUES ('d2')
            REPRESENTATION dense VECTOR [0.0, 1.0, 0.0]
            REPRESENTATION sparse SPARSE [1:0.1];").await.unwrap();

        // Hybrid search: both dense and sparse signals point to d1
        let result = engine.execute_tql(
            "SEARCH docs NEAR VECTOR [1.0, 0.0, 0.0] NEAR SPARSE [1:1.0] LIMIT 5;"
        ).await.unwrap();
        assert!(!result.rows.is_empty());
        // d1 should rank first (strong signal in both)
        assert_eq!(result.rows[0].values.get("id"), Some(&Value::String("d1".into())));
    }

    #[tokio::test]
    async fn scalar_prefilter_narrows_search() {
        let (engine, _dir) = test_engine().await;
        engine.execute_tql("CREATE COLLECTION venues (
            REPRESENTATION identity DIMENSIONS 3 METRIC COSINE,
            FIELD city TEXT,
            INDEX idx_city ON (city)
        );").await.unwrap();

        engine.execute_tql("INSERT INTO venues (id, city) VALUES ('v1', 'London')
            REPRESENTATION identity VECTOR [1.0, 0.0, 0.0];").await.unwrap();
        engine.execute_tql("INSERT INTO venues (id, city) VALUES ('v2', 'Paris')
            REPRESENTATION identity VECTOR [0.9, 0.1, 0.0];").await.unwrap();
        engine.execute_tql("INSERT INTO venues (id, city) VALUES ('v3', 'London')
            REPRESENTATION identity VECTOR [0.8, 0.2, 0.0];").await.unwrap();

        // SEARCH with WHERE narrows to London only
        let result = engine.execute_tql(
            "SEARCH venues WHERE city = 'London' NEAR VECTOR [1.0, 0.0, 0.0] LIMIT 10;"
        ).await.unwrap();
        // Should only return London entities
        for row in &result.rows {
            assert_eq!(row.values.get("city"), Some(&Value::String("London".into())));
        }
        assert_eq!(result.rows.len(), 2);
    }

    #[tokio::test]
    async fn error_duplicate_representation() {
        let (engine, _dir) = test_engine().await;
        let result = engine.execute_tql("CREATE COLLECTION test (
            REPRESENTATION dup DIMENSIONS 3 METRIC COSINE,
            REPRESENTATION dup DIMENSIONS 3 METRIC COSINE
        );").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("duplicate representation"), "error should mention duplicate: {err_msg}");
    }

    #[tokio::test]
    async fn error_duplicate_field() {
        let (engine, _dir) = test_engine().await;
        let result = engine.execute_tql("CREATE COLLECTION test (
            REPRESENTATION r DIMENSIONS 3 METRIC COSINE,
            FIELD status TEXT,
            FIELD status TEXT
        );").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("duplicate field"), "error should mention duplicate: {err_msg}");
    }

    #[tokio::test]
    async fn error_duplicate_index() {
        let (engine, _dir) = test_engine().await;
        let result = engine.execute_tql("CREATE COLLECTION test (
            REPRESENTATION r DIMENSIONS 3 METRIC COSINE,
            FIELD status TEXT,
            INDEX idx ON (status),
            INDEX idx ON (status)
        );").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("duplicate index"), "error should mention duplicate: {err_msg}");
    }

    #[tokio::test]
    async fn error_sparse_vector_on_dense_only_collection() {
        let (engine, _dir) = test_engine().await;
        engine.execute_tql("CREATE COLLECTION venues (
            REPRESENTATION identity DIMENSIONS 3 METRIC COSINE
        );").await.unwrap();
        engine.execute_tql("INSERT INTO venues (id) VALUES ('v1')
            REPRESENTATION identity VECTOR [0.1, 0.2, 0.3];").await.unwrap();

        // SEARCH with SPARSE on a collection that has no sparse representation
        let result = engine.execute_tql("SEARCH venues NEAR SPARSE [1:1.0] LIMIT 5;").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("sparse"), "error should mention sparse: {err_msg}");
    }

    #[tokio::test]
    async fn error_field_not_indexed() {
        let (engine, _dir) = test_engine().await;
        engine.execute_tql("CREATE COLLECTION venues (
            REPRESENTATION identity DIMENSIONS 3 METRIC COSINE
        );").await.unwrap();
        engine.execute_tql("INSERT INTO venues (id, city) VALUES ('v1', 'London')
            REPRESENTATION identity VECTOR [0.1, 0.2, 0.3];").await.unwrap();

        // SEARCH WHERE on a field that has no index
        let result = engine.execute_tql(
            "SEARCH venues WHERE city = 'London' NEAR VECTOR [0.1, 0.2, 0.3] LIMIT 5;"
        ).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not indexed") || err_msg.contains("FieldNotIndexed"),
            "error should mention not indexed: {err_msg}");
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
            hnsw_snapshot_interval_secs: 0,
        };

        // Insert data
        {
            let (engine, _) = Engine::open(config.clone()).await.unwrap();
            create_simple_collection(&engine, "venues", 3).await;
            engine
                .execute_tql(
                    "INSERT INTO venues (id, name) VALUES ('v1', 'Test') REPRESENTATION default VECTOR [0.1, 0.2, 0.3];",
                )
                .await
                .unwrap();

            // Manually write snapshot
            let snap_bytes = engine.location().snapshot(engine.wal_head_lsn()).unwrap();
            std::fs::write(data_dir.join("location_table.snap"), &snap_bytes).unwrap();
        }

        // Reopen — should restore from snapshot
        {
            let (engine, _) = Engine::open(config).await.unwrap();
            let key = crate::location::ReprKey {
                entity_id: crate::types::LogicalId::from_string("v1"),
                repr_index: 0,
            };
            let desc = engine.location().get(&key).expect("should have location entry after restore");
            assert_eq!(desc.tier, crate::location::Tier::Fjall);
        }
    }

    #[tokio::test]
    async fn update_entity_changes_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("store"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
            hnsw_snapshot_interval_secs: 0,
        };
        let (engine, _) = Engine::open(config).await.unwrap();

        engine.execute_tql("CREATE COLLECTION venues (
                REPRESENTATION default DIMENSIONS 3 METRIC COSINE,
                FIELD name TEXT,
                INDEX idx_name ON (name)
            );").await.unwrap();

        engine.execute_tql(
            "INSERT INTO venues (id, name) VALUES ('v1', 'Old Name') \
             REPRESENTATION default VECTOR [1.0, 0.0, 0.0];"
        ).await.unwrap();

        // UPDATE
        engine.execute_tql("UPDATE 'v1' IN venues SET name = 'New Name';").await.unwrap();

        // FETCH to verify
        let result = engine.execute_tql("FETCH * FROM venues WHERE name = 'New Name';").await.unwrap();
        assert_eq!(result.rows.len(), 1, "should find entity by new name");

        // Old name should return nothing
        let result = engine.execute_tql("FETCH * FROM venues WHERE name = 'Old Name';").await.unwrap();
        assert_eq!(result.rows.len(), 0, "old name should not match");
    }

    #[tokio::test]
    async fn update_nonexistent_entity_errors() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("store"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
            hnsw_snapshot_interval_secs: 0,
        };
        let (engine, _) = Engine::open(config).await.unwrap();

        engine.execute_tql(
            "CREATE COLLECTION venues (\
                REPRESENTATION default DIMENSIONS 3 METRIC COSINE\
            );"
        ).await.unwrap();

        let result = engine.execute_tql("UPDATE 'nonexistent' IN venues SET name = 'X';").await;
        assert!(result.is_err(), "updating nonexistent entity should error");
    }

    #[tokio::test]
    async fn wal_replay_entity_delete() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("store"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
            hnsw_snapshot_interval_secs: 0,
        };

        // Insert then delete
        {
            let (engine, _) = Engine::open(config.clone()).await.unwrap();
            engine.execute_tql(
                "CREATE COLLECTION venues (\
                    REPRESENTATION default DIMENSIONS 3 METRIC COSINE\
                );"
            ).await.unwrap();
            engine.execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'Test') \
                 REPRESENTATION default VECTOR [1.0, 0.0, 0.0];"
            ).await.unwrap();
            engine.execute_tql("DELETE 'v1' FROM venues;").await.unwrap();
        }

        // Reopen — WAL replay should process the EntityDelete
        let (engine, _) = Engine::open(config).await.unwrap();
        let result = engine.execute_tql("FETCH * FROM venues;").await.unwrap();
        assert_eq!(result.rows.len(), 0, "entity should not exist after WAL replay of delete");
    }

    #[tokio::test]
    async fn hnsw_snapshot_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("store"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
            hnsw_snapshot_interval_secs: 0,
        };

        // Insert entities and create snapshot
        {
            let (engine, _) = Engine::open(config.clone()).await.unwrap();
            engine.execute_tql(
                "CREATE COLLECTION venues (\
                    REPRESENTATION default DIMENSIONS 3 METRIC COSINE\
                );"
            ).await.unwrap();
            engine.execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'A') \
                 REPRESENTATION default VECTOR [1.0, 0.0, 0.0];"
            ).await.unwrap();
            engine.execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v2', 'B') \
                 REPRESENTATION default VECTOR [0.0, 1.0, 0.0];"
            ).await.unwrap();

            // Trigger manual snapshot
            engine.save_hnsw_snapshots().unwrap();
        }

        // Reopen — should load from snapshot instead of full rebuild
        let (engine, _) = Engine::open(config).await.unwrap();
        let result = engine.execute_tql(
            "SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] LIMIT 2;"
        ).await.unwrap();
        assert!(result.rows.len() >= 1, "SEARCH should find entities after snapshot restore");
    }

    #[tokio::test]
    async fn phase8_integration_update_then_restart_with_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("store"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
            hnsw_snapshot_interval_secs: 0,
        };

        // Session 1: create, insert, update, snapshot
        {
            let (engine, _) = Engine::open(config.clone()).await.unwrap();
            engine.execute_tql("CREATE COLLECTION venues (
                REPRESENTATION default DIMENSIONS 3 METRIC COSINE,
                FIELD name TEXT,
                INDEX idx_name ON (name)
            );").await.unwrap();

            engine.execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'Old') \
                 REPRESENTATION default VECTOR [1.0, 0.0, 0.0];"
            ).await.unwrap();
            engine.execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v2', 'Keep') \
                 REPRESENTATION default VECTOR [0.0, 1.0, 0.0];"
            ).await.unwrap();

            // UPDATE v1's name
            engine.execute_tql("UPDATE 'v1' IN venues SET name = 'New';").await.unwrap();

            // DELETE v2
            engine.execute_tql("DELETE 'v2' FROM venues;").await.unwrap();

            // Save HNSW snapshot
            engine.save_hnsw_snapshots().unwrap();
        }

        // Session 2: reopen and verify all state survived
        {
            let (engine, _) = Engine::open(config).await.unwrap();

            // v1 should exist with updated name
            let result = engine.execute_tql("FETCH * FROM venues WHERE name = 'New';").await.unwrap();
            assert_eq!(result.rows.len(), 1, "v1 should be findable by updated name");

            // Old name should not match
            let result = engine.execute_tql("FETCH * FROM venues WHERE name = 'Old';").await.unwrap();
            assert_eq!(result.rows.len(), 0, "old name should not match");

            // v2 should be gone
            let result = engine.execute_tql("FETCH * FROM venues;").await.unwrap();
            assert_eq!(result.rows.len(), 1, "only v1 should remain");

            // SEARCH should still work (HNSW loaded from snapshot)
            let result = engine.execute_tql(
                "SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] LIMIT 5;"
            ).await.unwrap();
            assert!(!result.rows.is_empty(), "SEARCH should return results after snapshot restore");
        }
    }

    #[tokio::test]
    async fn hnsw_snapshot_periodic_creates_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("store"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 1, // 1 second for testing
            hnsw_snapshot_interval_secs: 1, // 1 second for testing
        };

        let (engine, _) = Engine::open(config.clone()).await.unwrap();
        engine.execute_tql(
            "CREATE COLLECTION venues (\
                REPRESENTATION default DIMENSIONS 3 METRIC COSINE\
            );"
        ).await.unwrap();
        engine.execute_tql(
            "INSERT INTO venues (id, name) VALUES ('v1', 'Test') \
             REPRESENTATION default VECTOR [1.0, 0.0, 0.0];"
        ).await.unwrap();

        // Wait for periodic snapshot to fire
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let snap_dir = config.data_dir.join("hnsw_snapshots");
        assert!(snap_dir.exists(), "hnsw_snapshots directory should exist");
        assert!(
            snap_dir.join("venues_default.hnsw.meta").exists(),
            "snapshot meta should exist after periodic snapshot"
        );
    }

    #[tokio::test]
    async fn hnsw_snapshot_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("store"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0, // no periodic — rely on Drop
            hnsw_snapshot_interval_secs: 0,
        };

        {
            let (engine, _) = Engine::open(config.clone()).await.unwrap();
            engine.execute_tql(
                "CREATE COLLECTION venues (\
                    REPRESENTATION default DIMENSIONS 3 METRIC COSINE\
                );"
            ).await.unwrap();
            engine.execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'Test') \
                 REPRESENTATION default VECTOR [1.0, 0.0, 0.0];"
            ).await.unwrap();
            // Engine dropped here — Drop should save HNSW snapshots
        }

        let snap_dir = config.data_dir.join("hnsw_snapshots");
        assert!(snap_dir.exists(), "hnsw_snapshots directory should exist after drop");
        assert!(
            snap_dir.join("venues_default.hnsw.meta").exists(),
            "snapshot meta should exist after drop"
        );
    }
}
