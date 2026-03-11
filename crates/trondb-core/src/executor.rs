use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;

use crate::edge::{AdjacencyIndex, Edge, EdgeType, DecayConfig};
use crate::error::EngineError;
use crate::field_index::FieldIndex;
use crate::index::HnswIndex;
use crate::location::{
    Encoding, LocState, LocationDescriptor, LocationTable, NodeAddress, ReprKey, Tier,
};
use crate::planner::{Plan, SearchStrategy};
use crate::result::{QueryMode, QueryResult, QueryStats, Row};
use crate::sparse_index::SparseIndex;
use crate::store::FjallStore;
use crate::types::{
    CollectionSchema, Entity, FieldType, LogicalId, Metric, ReprState, ReprType, Representation,
    StoredField, StoredIndex, StoredRepresentation, Value, VectorData,
};
use trondb_tql::{FieldList, Literal, VectorLiteral, WhereClause};
use trondb_wal::{RecordType, WalRecord, WalWriter};

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

pub struct Executor {
    store: FjallStore,
    wal: WalWriter,
    location: Arc<LocationTable>,
    indexes: DashMap<String, HnswIndex>,
    sparse_indexes: DashMap<String, SparseIndex>,
    field_indexes: DashMap<String, FieldIndex>,
    schemas: DashMap<String, CollectionSchema>,
    adjacency: AdjacencyIndex,
    edge_types: DashMap<String, EdgeType>,
}

impl Executor {
    pub fn new(store: FjallStore, wal: WalWriter, location: Arc<LocationTable>) -> Self {
        Self {
            store,
            wal,
            location,
            indexes: DashMap::new(),
            sparse_indexes: DashMap::new(),
            field_indexes: DashMap::new(),
            schemas: DashMap::new(),
            adjacency: AdjacencyIndex::new(),
            edge_types: DashMap::new(),
        }
    }

    /// Replay committed WAL records into the Fjall store.
    /// Called during engine startup to close the durability gap.
    pub fn replay_wal_records(&self, records: &[WalRecord]) -> Result<usize, EngineError> {
        let mut replayed = 0;

        for record in records {
            match record.record_type {
                RecordType::SchemaCreateColl => {
                    // Try to deserialize as CollectionSchema first (new format)
                    if let Ok(schema) = rmp_serde::from_slice::<CollectionSchema>(&record.payload) {
                        if !self.store.has_collection(&schema.name) {
                            self.store.create_collection_schema(&schema)?;
                            self.schemas.insert(schema.name.clone(), schema);
                            replayed += 1;
                        }
                    } else {
                        // Legacy format — try old {name, dimensions} struct
                        #[derive(serde::Deserialize)]
                        struct LegacyPayload {
                            name: String,
                            dimensions: usize,
                        }
                        if let Ok(payload) = rmp_serde::from_slice::<LegacyPayload>(&record.payload) {
                            if !self.store.has_collection(&payload.name) {
                                let schema = CollectionSchema {
                                    name: payload.name.clone(),
                                    representations: vec![StoredRepresentation {
                                        name: "default".into(),
                                        model: None,
                                        dimensions: Some(payload.dimensions),
                                        metric: Metric::Cosine,
                                        sparse: false,
                                    }],
                                    fields: vec![],
                                    indexes: vec![],
                                };
                                self.store.create_collection_schema(&schema)?;
                                self.schemas.insert(schema.name.clone(), schema);
                                replayed += 1;
                            }
                        }
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
                RecordType::SchemaCreateEdgeType => {
                    let edge_type: crate::edge::EdgeType = rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                    if !self.store.has_edge_type(&edge_type.name) {
                        self.store.create_edge_type(&edge_type)?;
                        replayed += 1;
                    }
                    self.edge_types.insert(edge_type.name.clone(), edge_type);
                }
                RecordType::EdgeWrite => {
                    let edge: crate::edge::Edge = rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                    if self.store.has_edge_type(&edge.edge_type) {
                        self.store.insert_edge(&edge)?;
                        replayed += 1;
                    }
                }
                RecordType::EdgeDelete => {
                    #[derive(serde::Deserialize)]
                    struct EdgeDeletePayload {
                        edge_type: String,
                        from_id: String,
                        to_id: String,
                    }
                    let payload: EdgeDeletePayload = rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                    self.store.delete_edge(&payload.edge_type, &payload.from_id, &payload.to_id)?;
                    replayed += 1;
                }
                _ => {
                    // Other record types (ReprWrite, etc.) are future phases
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
                // Build CollectionSchema from plan
                let schema = build_collection_schema(p);

                // WAL: TX_BEGIN -> SCHEMA_CREATE_COLL -> commit
                let tx_id = self.wal.next_tx_id();
                self.wal.append(RecordType::TxBegin, &p.name, tx_id, 1, vec![]);

                let payload = rmp_serde::to_vec_named(&schema)
                    .map_err(|e| EngineError::Storage(e.to_string()))?;

                self.wal.append(RecordType::SchemaCreateColl, &p.name, tx_id, 1, payload);
                self.wal.commit(tx_id).await?;

                // Apply to Fjall
                self.store.create_collection_schema(&schema)?;

                // Register in schemas map
                self.schemas.insert(schema.name.clone(), schema);

                Ok(QueryResult {
                    columns: vec!["result".into()],
                    rows: vec![Row {
                        values: HashMap::from([(
                            "result".into(),
                            Value::String(format!(
                                "Collection '{}' created ({} representations, {} fields, {} indexes)",
                                p.name,
                                p.representations.len(),
                                p.fields.len(),
                                p.indexes.len(),
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

                // Handle named representation vectors
                for (repr_name, vec_lit) in &p.vectors {
                    let vector_data = match vec_lit {
                        VectorLiteral::Dense(v) => {
                            let vec_f32: Vec<f32> = v.iter().map(|x| *x as f32).collect();
                            VectorData::Dense(vec_f32)
                        }
                        VectorLiteral::Sparse(v) => {
                            VectorData::Sparse(v.clone())
                        }
                    };

                    let repr = Representation {
                        name: repr_name.clone(),
                        repr_type: ReprType::Atomic,
                        fields: p.fields.clone(),
                        vector: vector_data,
                        recipe_hash: [0u8; 32],
                        state: ReprState::Clean,
                    };
                    entity.representations.push(repr);
                }

                // WAL: TX_BEGIN -> ENTITY_WRITE -> LOCATION_UPDATE(s) -> commit
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

                // Apply to HNSW index (only dense vectors)
                for repr in &entity.representations {
                    if let VectorData::Dense(ref vec_f32) = repr.vector {
                        let dims = vec_f32.len();
                        let index = self.indexes
                            .entry(p.collection.clone())
                            .or_insert_with(|| HnswIndex::new(dims));
                        index.insert(&id, vec_f32);
                    }
                    // NOTE: sparse index updates are Task 9 work
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

                match &p.strategy {
                    SearchStrategy::Hnsw => {
                        let query_f64 = p.dense_vector.as_ref()
                            .ok_or_else(|| EngineError::InvalidQuery("HNSW requires dense vector".into()))?;
                        let query_f32: Vec<f32> = query_f64.iter().map(|v| *v as f32).collect();

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

                            if let Ok(entities) = self.store.scan(&p.collection) {
                                if let Some(entity) = entities.iter().find(|e| e.id == id) {
                                    let mut row = entity_to_row(entity, &p.fields);
                                    row.score = Some(similarity);
                                    rows.push(row);
                                }
                            }
                        }

                        Ok(QueryResult {
                            columns: build_columns(&rows, &p.fields),
                            rows,
                            stats: QueryStats {
                                elapsed: start.elapsed(),
                                entities_scanned: scanned,
                                mode: QueryMode::Probabilistic,
                                tier: "Ram".into(),
                            },
                        })
                    }
                    SearchStrategy::Sparse => {
                        Err(EngineError::UnsupportedOperation(
                            "Sparse-only SEARCH not yet implemented (Task 10)".into(),
                        ))
                    }
                    SearchStrategy::Hybrid => {
                        Err(EngineError::UnsupportedOperation(
                            "Hybrid SEARCH not yet implemented (Task 10)".into(),
                        ))
                    }
                }
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

            Plan::CreateEdgeType(p) => {
                // Validate from/to collections exist
                if !self.store.has_collection(&p.from_collection) {
                    return Err(EngineError::CollectionNotFound(p.from_collection.clone()));
                }
                if !self.store.has_collection(&p.to_collection) {
                    return Err(EngineError::CollectionNotFound(p.to_collection.clone()));
                }

                let edge_type = EdgeType {
                    name: p.name.clone(),
                    from_collection: p.from_collection.clone(),
                    to_collection: p.to_collection.clone(),
                    decay_config: DecayConfig::default(),
                };

                // WAL: TxBegin -> SchemaCreateEdgeType -> commit
                let tx_id = self.wal.next_tx_id();
                self.wal.append(RecordType::TxBegin, &p.name, tx_id, 1, vec![]);
                let payload = rmp_serde::to_vec_named(&edge_type)
                    .map_err(|e| EngineError::Storage(e.to_string()))?;
                self.wal.append(RecordType::SchemaCreateEdgeType, &p.name, tx_id, 1, payload);
                self.wal.commit(tx_id).await?;

                // Apply to Fjall
                self.store.create_edge_type(&edge_type)?;

                // Register in memory
                self.edge_types.insert(p.name.clone(), edge_type);

                Ok(QueryResult {
                    columns: vec!["result".into()],
                    rows: vec![Row {
                        values: HashMap::from([(
                            "result".into(),
                            Value::String(format!(
                                "Edge type '{}' created ({} -> {})",
                                p.name, p.from_collection, p.to_collection
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

            Plan::InsertEdge(p) => {
                // Validate edge type exists
                let edge_type = self.store.get_edge_type(&p.edge_type)?;

                // Validate from/to entities exist
                let from_id = LogicalId::from_string(&p.from_id);
                let to_id = LogicalId::from_string(&p.to_id);
                self.store.get(&edge_type.from_collection, &from_id)?;
                self.store.get(&edge_type.to_collection, &to_id)?;

                // Build metadata
                let mut metadata = HashMap::new();
                for (key, lit) in &p.metadata {
                    metadata.insert(key.clone(), literal_to_value(lit));
                }

                let edge = Edge {
                    from_id: from_id.clone(),
                    to_id: to_id.clone(),
                    edge_type: p.edge_type.clone(),
                    confidence: 1.0,
                    metadata,
                };

                // WAL: TxBegin -> EdgeWrite -> commit
                let tx_id = self.wal.next_tx_id();
                self.wal.append(RecordType::TxBegin, &p.edge_type, tx_id, 1, vec![]);
                let payload = rmp_serde::to_vec_named(&edge)
                    .map_err(|e| EngineError::Storage(e.to_string()))?;
                self.wal.append(RecordType::EdgeWrite, &p.edge_type, tx_id, 1, payload);
                self.wal.commit(tx_id).await?;

                // Apply to Fjall
                self.store.insert_edge(&edge)?;
                self.store.persist()?;

                // Apply to AdjacencyIndex
                self.adjacency.insert(&from_id, &p.edge_type, &to_id, 1.0);

                Ok(QueryResult {
                    columns: vec!["result".into()],
                    rows: vec![Row {
                        values: HashMap::from([(
                            "result".into(),
                            Value::String(format!(
                                "Edge '{}' created: {} -> {}",
                                p.edge_type, p.from_id, p.to_id
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

            Plan::DeleteEdge(p) => {
                let from_id = LogicalId::from_string(&p.from_id);
                let to_id = LogicalId::from_string(&p.to_id);

                // WAL: TxBegin -> EdgeDelete -> commit
                let tx_id = self.wal.next_tx_id();
                self.wal.append(RecordType::TxBegin, &p.edge_type, tx_id, 1, vec![]);
                let payload = rmp_serde::to_vec_named(&serde_json::json!({
                    "edge_type": p.edge_type,
                    "from_id": p.from_id,
                    "to_id": p.to_id,
                }))
                .map_err(|e| EngineError::Storage(e.to_string()))?;
                self.wal.append(RecordType::EdgeDelete, &p.edge_type, tx_id, 1, payload);
                self.wal.commit(tx_id).await?;

                // Apply to Fjall
                self.store.delete_edge(&p.edge_type, &p.from_id, &p.to_id)?;
                self.store.persist()?;

                // Apply to AdjacencyIndex
                self.adjacency.remove(&from_id, &p.edge_type, &to_id);

                Ok(QueryResult {
                    columns: vec!["result".into()],
                    rows: vec![Row {
                        values: HashMap::from([(
                            "result".into(),
                            Value::String(format!(
                                "Edge '{}' deleted: {} -> {}",
                                p.edge_type, p.from_id, p.to_id
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

            Plan::Traverse(p) => {
                // Validate edge type exists
                let edge_type = self.store.get_edge_type(&p.edge_type)?;

                // Gate multi-hop
                if p.depth > 1 {
                    return Err(EngineError::UnsupportedOperation(
                        "TRAVERSE DEPTH > 1 requires Phase 6+".into(),
                    ));
                }

                let from_id = LogicalId::from_string(&p.from_id);
                let entries = self.adjacency.get(&from_id, &p.edge_type);

                let mut rows = Vec::new();
                for entry in &entries {
                    if let Ok(entity) = self.store.get(&edge_type.to_collection, &entry.to_id) {
                        rows.push(entity_to_row(&entity, &FieldList::All));
                    }
                }

                if let Some(limit) = p.limit {
                    rows.truncate(limit);
                }

                let scanned = rows.len();

                Ok(QueryResult {
                    columns: build_columns(&rows, &FieldList::All),
                    rows,
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: scanned,
                        mode: QueryMode::Deterministic,
                        tier: "Ram".into(),
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

    pub fn sparse_indexes(&self) -> &DashMap<String, SparseIndex> {
        &self.sparse_indexes
    }

    pub fn field_indexes(&self) -> &DashMap<String, FieldIndex> {
        &self.field_indexes
    }

    pub fn schemas(&self) -> &DashMap<String, CollectionSchema> {
        &self.schemas
    }

    pub fn scan_collection(&self, name: &str) -> Result<Vec<crate::types::Entity>, EngineError> {
        self.store.scan(name)
    }

    pub fn store(&self) -> &FjallStore {
        &self.store
    }

    pub fn adjacency(&self) -> &AdjacencyIndex {
        &self.adjacency
    }

    pub fn edge_types(&self) -> &DashMap<String, EdgeType> {
        &self.edge_types
    }

    pub fn list_edge_types(&self) -> Vec<EdgeType> {
        self.store.list_edge_types()
    }

    pub fn scan_edges(&self, edge_type: &str) -> Result<Vec<Edge>, EngineError> {
        self.store.scan_edges(edge_type)
    }
}

// ---------------------------------------------------------------------------
// Helper: build CollectionSchema from CreateCollectionPlan
// ---------------------------------------------------------------------------

fn build_collection_schema(p: &crate::planner::CreateCollectionPlan) -> CollectionSchema {
    let representations: Vec<StoredRepresentation> = p.representations.iter().map(|r| {
        StoredRepresentation {
            name: r.name.clone(),
            model: r.model.clone(),
            dimensions: r.dimensions,
            metric: convert_metric(&r.metric),
            sparse: r.sparse,
        }
    }).collect();

    let fields: Vec<StoredField> = p.fields.iter().map(|f| {
        StoredField {
            name: f.name.clone(),
            field_type: convert_field_type(&f.field_type),
        }
    }).collect();

    let indexes: Vec<StoredIndex> = p.indexes.iter().map(|i| {
        StoredIndex {
            name: i.name.clone(),
            fields: i.fields.clone(),
            partial_condition: i.partial_condition.as_ref().map(|c| format!("{c:?}")),
        }
    }).collect();

    CollectionSchema {
        name: p.name.clone(),
        representations,
        fields,
        indexes,
    }
}

/// Convert AST FieldType to core FieldType.
fn convert_field_type(ft: &trondb_tql::FieldType) -> FieldType {
    match ft {
        trondb_tql::FieldType::Text => FieldType::Text,
        trondb_tql::FieldType::DateTime => FieldType::DateTime,
        trondb_tql::FieldType::Bool => FieldType::Bool,
        trondb_tql::FieldType::Int => FieldType::Int,
        trondb_tql::FieldType::Float => FieldType::Float,
        trondb_tql::FieldType::EntityRef(s) => FieldType::EntityRef(s.clone()),
    }
}

/// Convert AST Metric to core Metric.
fn convert_metric(m: &trondb_tql::Metric) -> Metric {
    match m {
        trondb_tql::Metric::Cosine => Metric::Cosine,
        trondb_tql::Metric::InnerProduct => Metric::InnerProduct,
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
            props.push(("strategy", format!("{:?}", p.strategy)));
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
            let strategy_name = match &p.strategy {
                SearchStrategy::Hnsw => "HNSW",
                SearchStrategy::Sparse => "Sparse",
                SearchStrategy::Hybrid => "Hybrid",
            };
            props.push(("strategy", strategy_name.into()));
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
            props.push(("representations", p.representations.len().to_string()));
            props.push(("fields", p.fields.len().to_string()));
            props.push(("indexes", p.indexes.len().to_string()));
        }
        Plan::Explain(_) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "EXPLAIN".into()));
        }
        Plan::CreateEdgeType(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "CREATE EDGE".into()));
            props.push(("edge_type", p.name.clone()));
            props.push(("from_collection", p.from_collection.clone()));
            props.push(("to_collection", p.to_collection.clone()));
        }
        Plan::InsertEdge(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "INSERT EDGE".into()));
            props.push(("edge_type", p.edge_type.clone()));
            props.push(("tier", "Fjall".into()));
        }
        Plan::DeleteEdge(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "DELETE EDGE".into()));
            props.push(("edge_type", p.edge_type.clone()));
            props.push(("tier", "Fjall".into()));
        }
        Plan::Traverse(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "TRAVERSE".into()));
            props.push(("edge_type", p.edge_type.clone()));
            props.push(("tier", "Ram".into()));
            props.push(("strategy", "AdjacencyIndex".into()));
            props.push(("depth", p.depth.to_string()));
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
    use crate::planner::{CreateCollectionPlan, FetchPlan, FetchStrategy, InsertPlan};
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
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(dims),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
            }],
            fields: vec![],
            indexes: vec![],
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
        let vectors = match vector {
            Some(v) => vec![("default".to_string(), VectorLiteral::Dense(v))],
            None => vec![],
        };
        exec.execute(&Plan::Insert(InsertPlan {
            collection: collection.into(),
            fields,
            values,
            vectors,
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
                strategy: FetchStrategy::FullScan,
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
                strategy: FetchStrategy::FullScan,
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
            strategy: FetchStrategy::FullScan,
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
