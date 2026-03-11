use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;

use crate::error::EngineError;
use crate::index::HnswIndex;
use crate::location::{
    Encoding, LocState, LocationDescriptor, LocationTable, NodeAddress, ReprKey, Tier,
};
use crate::planner::Plan;
use crate::result::{QueryMode, QueryResult, QueryStats, Row};
use crate::store::FjallStore;
use crate::types::{Entity, LogicalId, ReprState, ReprType, Representation, Value};
use trondb_tql::{FieldList, Literal, WhereClause};
use trondb_wal::{RecordType, WalRecord, WalWriter};

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

pub struct Executor {
    store: FjallStore,
    wal: WalWriter,
    location: Arc<LocationTable>,
    indexes: DashMap<String, HnswIndex>,
}

impl Executor {
    pub fn new(store: FjallStore, wal: WalWriter, location: Arc<LocationTable>) -> Self {
        Self {
            store,
            wal,
            location,
            indexes: DashMap::new(),
        }
    }

    /// Replay committed WAL records into the Fjall store.
    /// Called during engine startup to close the durability gap.
    pub fn replay_wal_records(&self, records: &[WalRecord]) -> Result<usize, EngineError> {
        let mut replayed = 0;

        for record in records {
            match record.record_type {
                RecordType::SchemaCreateColl => {
                    #[derive(serde::Deserialize)]
                    struct CreateCollPayload {
                        name: String,
                        dimensions: usize,
                    }
                    let payload: CreateCollPayload = rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;

                    // Idempotent: skip if collection already exists
                    if !self.store.has_collection(&payload.name) {
                        self.store.create_collection(&payload.name, payload.dimensions)?;
                        replayed += 1;
                    }
                }
                RecordType::EntityWrite => {
                    let entity: Entity = rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;

                    // Idempotent: overwrite with WAL version (WAL is authoritative)
                    if self.store.has_collection(&record.collection) {
                        self.store.insert(&record.collection, entity)?;
                        replayed += 1;
                    }
                }
                RecordType::EntityDelete => {
                    // Phase 2 doesn't support DELETE yet, but handle for completeness
                }
                RecordType::LocationUpdate => {
                    let (key, desc): (ReprKey, LocationDescriptor) =
                        rmp_serde::from_slice(&record.payload)
                            .map_err(|e| EngineError::Storage(e.to_string()))?;
                    self.location.register(key, desc);
                    replayed += 1;
                }
                _ => {
                    // Other record types (ReprWrite, EdgeWrite, etc.) are future phases
                }
            }
        }

        if replayed > 0 {
            self.store.persist()?;
        }

        Ok(replayed)
    }

    pub async fn execute(&self, plan: &Plan) -> Result<QueryResult, EngineError> {
        let start = Instant::now();

        match plan {
            Plan::CreateCollection(p) => {
                // WAL: TX_BEGIN → SCHEMA_CREATE_COLL → commit
                let tx_id = self.wal.next_tx_id();
                self.wal.append(RecordType::TxBegin, &p.name, tx_id, 1, vec![]);

                let payload = rmp_serde::to_vec_named(&serde_json::json!({
                    "name": p.name,
                    "dimensions": p.dimensions,
                }))
                .map_err(|e| EngineError::Storage(e.to_string()))?;

                self.wal.append(RecordType::SchemaCreateColl, &p.name, tx_id, 1, payload);
                self.wal.commit(tx_id).await?;

                // Apply to Fjall
                self.store.create_collection(&p.name, p.dimensions)?;

                Ok(QueryResult {
                    columns: vec!["result".into()],
                    rows: vec![Row {
                        values: HashMap::from([(
                            "result".into(),
                            Value::String(format!(
                                "Collection '{}' created ({} dimensions)",
                                p.name, p.dimensions
                            )),
                        )]),
                        score: None,
                    }],
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: 0,
                        mode: QueryMode::Deterministic,
                        tier: "Fjall".into(),
                    },
                })
            }

            Plan::Insert(p) => {
                // Find or generate LogicalId
                let id = find_id_in_fields(&p.fields, &p.values)
                    .unwrap_or_default();

                // Build entity with metadata
                let mut entity = Entity::new(id.clone());
                for (field, value) in p.fields.iter().zip(p.values.iter()) {
                    if field == "id" {
                        continue;
                    }
                    entity.metadata.insert(field.clone(), literal_to_value(value));
                }

                // Handle vector
                if let Some(ref vec_f64) = p.vector {
                    let dims = self.store.get_dimensions(&p.collection)?;
                    if vec_f64.len() != dims {
                        return Err(EngineError::DimensionMismatch {
                            expected: dims,
                            got: vec_f64.len(),
                        });
                    }

                    let vec_f32: Vec<f32> = vec_f64.iter().map(|v| *v as f32).collect();

                    let repr = Representation {
                        repr_type: ReprType::Atomic,
                        fields: p.fields.clone(),
                        vector: vec_f32,
                        recipe_hash: [0u8; 32],
                        state: ReprState::Clean,
                    };
                    entity.representations.push(repr);
                }

                // WAL: TX_BEGIN → ENTITY_WRITE → LOCATION_UPDATE(s) → commit
                let tx_id = self.wal.next_tx_id();
                self.wal.append(RecordType::TxBegin, &p.collection, tx_id, 1, vec![]);

                let entity_payload = rmp_serde::to_vec_named(&entity)
                    .map_err(|e| EngineError::Storage(e.to_string()))?;
                self.wal.append(RecordType::EntityWrite, &p.collection, tx_id, 1, entity_payload);

                // LocationUpdate for each representation
                for (idx, _repr) in entity.representations.iter().enumerate() {
                    let loc_key = ReprKey {
                        entity_id: id.clone(),
                        repr_index: idx as u32,
                    };
                    let loc_desc = LocationDescriptor {
                        tier: Tier::Fjall,
                        node_address: NodeAddress::localhost(),
                        state: LocState::Clean,
                        version: 1,
                        encoding: Encoding::Float32,
                    };
                    let loc_payload = rmp_serde::to_vec_named(&(&loc_key, &loc_desc))
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                    self.wal.append(RecordType::LocationUpdate, &p.collection, tx_id, 1, loc_payload);
                }

                self.wal.commit(tx_id).await?;

                // Apply to Fjall
                self.store.insert(&p.collection, entity.clone())?;
                self.store.persist()?;

                // Apply to Location Table
                for (idx, _repr) in entity.representations.iter().enumerate() {
                    self.location.register(
                        ReprKey {
                            entity_id: id.clone(),
                            repr_index: idx as u32,
                        },
                        LocationDescriptor {
                            tier: Tier::Fjall,
                            node_address: NodeAddress::localhost(),
                            state: LocState::Clean,
                            version: 1,
                            encoding: Encoding::Float32,
                        },
                    );
                }

                // Apply to HNSW index
                for repr in &entity.representations {
                    let dims = self.store.get_dimensions(&p.collection)?;
                    let index = self.indexes
                        .entry(p.collection.clone())
                        .or_insert_with(|| HnswIndex::new(dims));
                    index.insert(&id, &repr.vector);
                }

                Ok(QueryResult {
                    columns: vec!["result".into()],
                    rows: vec![Row {
                        values: HashMap::from([(
                            "result".into(),
                            Value::String(format!("Inserted entity '{}'", id)),
                        )]),
                        score: None,
                    }],
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: 0,
                        mode: QueryMode::Deterministic,
                        tier: "Fjall".into(),
                    },
                })
            }

            Plan::Fetch(p) => {
                // Read directly from Fjall — no WAL involvement
                let entities = self.store.scan(&p.collection)?;
                let scanned = entities.len();

                let filtered: Vec<&Entity> = entities
                    .iter()
                    .filter(|e| match &p.filter {
                        Some(clause) => entity_matches(e, clause),
                        None => true,
                    })
                    .collect();

                let mut rows: Vec<Row> = filtered
                    .iter()
                    .map(|e| entity_to_row(e, &p.fields))
                    .collect();

                if let Some(limit) = p.limit {
                    rows.truncate(limit);
                }

                let columns = build_columns(&rows, &p.fields);

                Ok(QueryResult {
                    columns,
                    rows,
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: scanned,
                        mode: QueryMode::Deterministic,
                        tier: "Fjall".into(),
                    },
                })
            }

            Plan::Search(p) => {
                // Validate collection exists
                if !self.store.has_collection(&p.collection) {
                    return Err(EngineError::CollectionNotFound(p.collection.clone()));
                }

                // Validate dimensions
                let dims = self.store.get_dimensions(&p.collection)?;
                if p.query_vector.len() != dims {
                    return Err(EngineError::DimensionMismatch {
                        expected: dims,
                        got: p.query_vector.len(),
                    });
                }

                // Cast query vector f64 → f32
                let query_f32: Vec<f32> = p.query_vector.iter().map(|v| *v as f32).collect();

                // Search HNSW index
                let candidates = if let Some(index) = self.indexes.get(&p.collection) {
                    index.search(&query_f32, p.k)
                } else {
                    Vec::new()
                };

                // Filter by confidence threshold and resolve entities
                let mut rows = Vec::new();
                let mut scanned = 0;
                for (id, similarity) in candidates {
                    if (similarity as f64) < p.confidence_threshold {
                        continue;
                    }
                    scanned += 1;

                    // Resolve entity from Fjall
                    if let Ok(entities) = self.store.scan(&p.collection) {
                        if let Some(entity) = entities.iter().find(|e| e.id == id) {
                            let mut row = entity_to_row(entity, &FieldList::All);
                            row.score = Some(similarity);
                            rows.push(row);
                        }
                    }
                }

                Ok(QueryResult {
                    columns: build_columns(&rows, &FieldList::All),
                    rows,
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: scanned,
                        mode: QueryMode::Probabilistic,
                        tier: "Ram".into(),
                    },
                })
            }

            Plan::Explain(inner) => {
                let rows = explain_plan(inner);
                Ok(QueryResult {
                    columns: vec!["property".into(), "value".into()],
                    rows,
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: 0,
                        mode: QueryMode::Deterministic,
                        tier: "Fjall".into(),
                    },
                })
            }
        }
    }

    pub fn collections(&self) -> Vec<String> {
        self.store.list_collections()
    }

    pub fn location(&self) -> &LocationTable {
        &self.location
    }

    pub fn location_arc(&self) -> Arc<LocationTable> {
        Arc::clone(&self.location)
    }

    pub fn wal_head_lsn(&self) -> u64 {
        self.wal.head_lsn()
    }

    pub fn indexes(&self) -> &DashMap<String, HnswIndex> {
        &self.indexes
    }

    pub fn scan_collection(&self, name: &str) -> Result<Vec<crate::types::Entity>, EngineError> {
        self.store.scan(name)
    }

    pub fn get_collection_dimensions(&self, name: &str) -> Result<usize, EngineError> {
        self.store.get_dimensions(name)
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

fn find_id_in_fields(fields: &[String], values: &[Literal]) -> Option<LogicalId> {
    for (field, value) in fields.iter().zip(values.iter()) {
        if field == "id" {
            if let Literal::String(s) = value {
                return Some(LogicalId::from_string(s));
            }
        }
    }
    None
}

fn literal_to_value(lit: &Literal) -> Value {
    match lit {
        Literal::String(s) => Value::String(s.clone()),
        Literal::Int(n) => Value::Int(*n),
        Literal::Float(f) => Value::Float(*f),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
    }
}

fn entity_to_row(entity: &Entity, fields: &FieldList) -> Row {
    let mut values = HashMap::new();

    // Always include id
    values.insert("id".into(), Value::String(entity.id.to_string()));

    match fields {
        FieldList::All => {
            for (k, v) in &entity.metadata {
                values.insert(k.clone(), v.clone());
            }
        }
        FieldList::Named(names) => {
            for name in names {
                if name == "id" {
                    continue; // already included
                }
                if let Some(v) = entity.metadata.get(name) {
                    values.insert(name.clone(), v.clone());
                } else {
                    values.insert(name.clone(), Value::Null);
                }
            }
        }
    }

    Row {
        values,
        score: None,
    }
}

fn entity_matches(entity: &Entity, clause: &WhereClause) -> bool {
    match clause {
        WhereClause::Eq(field, lit) => {
            let expected = literal_to_value(lit);
            if field == "id" {
                return Value::String(entity.id.to_string()) == expected;
            }
            entity
                .metadata
                .get(field)
                .map(|v| *v == expected)
                .unwrap_or(false)
        }
        WhereClause::Gt(field, lit) => {
            let threshold = literal_to_value(lit);
            entity
                .metadata
                .get(field)
                .map(|v| value_gt(v, &threshold))
                .unwrap_or(false)
        }
        WhereClause::Lt(field, lit) => {
            let threshold = literal_to_value(lit);
            entity
                .metadata
                .get(field)
                .map(|v| value_lt(v, &threshold))
                .unwrap_or(false)
        }
        WhereClause::And(a, b) => entity_matches(entity, a) && entity_matches(entity, b),
        WhereClause::Or(a, b) => entity_matches(entity, a) || entity_matches(entity, b),
    }
}

fn value_gt(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x > y,
        (Value::Float(x), Value::Float(y)) => x > y,
        (Value::Int(x), Value::Float(y)) => (*x as f64) > *y,
        (Value::Float(x), Value::Int(y)) => *x > (*y as f64),
        (Value::String(x), Value::String(y)) => x > y,
        _ => false,
    }
}

fn value_lt(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x < y,
        (Value::Float(x), Value::Float(y)) => x < y,
        (Value::Int(x), Value::Float(y)) => (*x as f64) < *y,
        (Value::Float(x), Value::Int(y)) => *x < (*y as f64),
        (Value::String(x), Value::String(y)) => x < y,
        _ => false,
    }
}

fn build_columns(rows: &[Row], fields: &FieldList) -> Vec<String> {
    match fields {
        FieldList::Named(names) => {
            let mut cols = vec!["id".to_string()];
            for n in names {
                if n != "id" && !cols.contains(n) {
                    cols.push(n.clone());
                }
            }
            cols
        }
        FieldList::All => {
            let mut cols = vec!["id".to_string()];
            for row in rows {
                for key in row.values.keys() {
                    if key != "id" && !cols.contains(key) {
                        cols.push(key.clone());
                    }
                }
            }
            cols.sort();
            // Keep id first, sort the rest
            cols.retain(|c| c != "id");
            cols.insert(0, "id".to_string());
            cols
        }
    }
}

fn explain_plan(plan: &Plan) -> Vec<Row> {
    let mut props: Vec<(&str, String)> = Vec::new();

    match plan {
        Plan::Fetch(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "FETCH".into()));
            props.push(("collection", p.collection.clone()));
            props.push(("tier", "Fjall".into()));
            props.push(("strategy", "FullScan".into()));
            if let Some(limit) = p.limit {
                props.push(("limit", limit.to_string()));
            }
        }
        Plan::Search(p) => {
            props.push(("mode", "Probabilistic".into()));
            props.push(("verb", "SEARCH".into()));
            props.push(("collection", p.collection.clone()));
            props.push(("tier", "Ram".into()));
            props.push(("encoding", "Float32".into()));
            props.push(("strategy", "HNSW".into()));
            props.push(("k", p.k.to_string()));
            props.push(("confidence_threshold", p.confidence_threshold.to_string()));
        }
        Plan::Insert(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "INSERT".into()));
            props.push(("collection", p.collection.clone()));
            props.push(("tier", "Fjall".into()));
        }
        Plan::CreateCollection(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "CREATE COLLECTION".into()));
            props.push(("collection", p.name.clone()));
            props.push(("dimensions", p.dimensions.to_string()));
        }
        Plan::Explain(_) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "EXPLAIN".into()));
        }
    }

    props
        .into_iter()
        .map(|(k, v)| Row {
            values: HashMap::from([
                ("property".into(), Value::String(k.into())),
                ("value".into(), Value::String(v)),
            ]),
            score: None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::location::LocationTable;
    use crate::planner::{CreateCollectionPlan, FetchPlan, InsertPlan};
    use trondb_tql::Literal;
    use trondb_wal::WalConfig;

    async fn setup_executor() -> (Executor, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = FjallStore::open(&dir.path().join("store")).unwrap();
        let wal_config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };
        let wal = WalWriter::open(wal_config).await.unwrap();
        (Executor::new(store, wal, Arc::new(LocationTable::new())), dir)
    }

    async fn create_collection(exec: &Executor, name: &str, dims: usize) {
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: name.into(),
            dimensions: dims,
        }))
        .await
        .unwrap();
    }

    async fn insert_entity(
        exec: &Executor,
        collection: &str,
        id: &str,
        metadata: Vec<(&str, Literal)>,
        vector: Option<Vec<f64>>,
    ) {
        let mut fields = vec!["id".to_string()];
        let mut values = vec![Literal::String(id.into())];
        for (k, v) in metadata {
            fields.push(k.into());
            values.push(v);
        }
        exec.execute(&Plan::Insert(InsertPlan {
            collection: collection.into(),
            fields,
            values,
            vector,
        }))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn execute_fetch_all() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;

        insert_entity(
            &exec,
            "venues",
            "v1",
            vec![("name", Literal::String("Venue A".into()))],
            None,
        )
        .await;
        insert_entity(
            &exec,
            "venues",
            "v2",
            vec![("name", Literal::String("Venue B".into()))],
            None,
        )
        .await;

        let result = exec
            .execute(&Plan::Fetch(FetchPlan {
                collection: "venues".into(),
                fields: FieldList::All,
                filter: None,
                limit: None,
            }))
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 2);
    }

    #[tokio::test]
    async fn execute_fetch_with_filter() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;

        insert_entity(
            &exec,
            "venues",
            "v1",
            vec![
                ("name", Literal::String("The Shard".into())),
                ("city", Literal::String("London".into())),
            ],
            None,
        )
        .await;
        insert_entity(
            &exec,
            "venues",
            "v2",
            vec![
                ("name", Literal::String("Old Trafford".into())),
                ("city", Literal::String("Manchester".into())),
            ],
            None,
        )
        .await;

        let result = exec
            .execute(&Plan::Fetch(FetchPlan {
                collection: "venues".into(),
                fields: FieldList::All,
                filter: Some(WhereClause::Eq(
                    "city".into(),
                    Literal::String("London".into()),
                )),
                limit: None,
            }))
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("city"),
            Some(&Value::String("London".into()))
        );
    }

    #[tokio::test]
    async fn execute_explain() {
        let (exec, _dir) = setup_executor().await;

        let fetch_plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            limit: None,
        });

        let result = exec
            .execute(&Plan::Explain(Box::new(fetch_plan)))
            .await
            .unwrap();

        // Should have property rows
        assert!(!result.rows.is_empty());

        // Find the "mode" property
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
}
