use std::collections::{HashMap, HashSet};
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
use crate::planner::{FetchStrategy, Plan, SearchStrategy};
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
                // Validate no duplicate representation names
                let mut repr_names = HashSet::new();
                for r in &p.representations {
                    if !repr_names.insert(&r.name) {
                        return Err(EngineError::DuplicateRepresentation(r.name.clone()));
                    }
                }

                // Validate no duplicate field names
                let mut field_names = HashSet::new();
                for f in &p.fields {
                    if !field_names.insert(&f.name) {
                        return Err(EngineError::DuplicateField(f.name.clone()));
                    }
                }

                // Validate no duplicate index names
                let mut index_names = HashSet::new();
                for i in &p.indexes {
                    if !index_names.insert(&i.name) {
                        return Err(EngineError::DuplicateIndex(i.name.clone()));
                    }
                }

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
                self.schemas.insert(schema.name.clone(), schema.clone());

                // Instantiate HNSW indexes for dense representations
                for repr in &schema.representations {
                    if !repr.sparse {
                        if let Some(dims) = repr.dimensions {
                            let hnsw_key = format!("{}:{}", schema.name, repr.name);
                            self.indexes.insert(hnsw_key, HnswIndex::new(dims));
                        }
                    }
                }

                // Instantiate SparseIndex for sparse representations
                for repr in &schema.representations {
                    if repr.sparse {
                        let sparse_key = format!("{}:{}", schema.name, repr.name);
                        self.sparse_indexes.insert(sparse_key, SparseIndex::new());
                    }
                }

                // Instantiate FieldIndex for each declared index
                for idx in &schema.indexes {
                    let partition = self.store.open_field_index_partition(&schema.name, &idx.name)?;
                    let field_types: Vec<(String, FieldType)> = idx.fields.iter()
                        .filter_map(|f| {
                            schema.fields.iter()
                                .find(|sf| sf.name == *f)
                                .map(|sf| (sf.name.clone(), sf.field_type.clone()))
                        })
                        .collect();
                    let fidx_key = format!("{}:{}", schema.name, idx.name);
                    self.field_indexes.insert(fidx_key, FieldIndex::new(partition, field_types));
                }

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
                // Look up schema and validate collection exists
                let schema = self.schemas.get(&p.collection)
                    .ok_or_else(|| EngineError::CollectionNotFound(p.collection.clone()))?
                    .clone();

                // Validate vectors against schema representations and dimensions
                for (repr_name, vec_lit) in &p.vectors {
                    let schema_repr = schema.representations.iter()
                        .find(|r| r.name == *repr_name)
                        .ok_or_else(|| EngineError::InvalidQuery(
                            format!("no representation '{}' in collection '{}'", repr_name, p.collection)
                        ))?;
                    if let VectorLiteral::Dense(v) = vec_lit {
                        if let Some(expected_dims) = schema_repr.dimensions {
                            if v.len() != expected_dims {
                                return Err(EngineError::InvalidQuery(format!(
                                    "representation '{}' expects {} dimensions, got {}",
                                    repr_name, expected_dims, v.len()
                                )));
                            }
                        }
                    }
                }

                // Find or generate LogicalId
                let entity_id = find_id_in_fields(&p.fields, &p.values)
                    .unwrap_or_default();

                // Handle entity updates — if entity exists, remove old index entries
                if let Ok(old_entity) = self.store.get(&p.collection, &entity_id) {
                    // Remove old sparse index entries
                    for repr in &old_entity.representations {
                        if let VectorData::Sparse(ref sv) = repr.vector {
                            let sparse_key = format!("{}:{}", p.collection, repr.name);
                            if let Some(sidx) = self.sparse_indexes.get(&sparse_key) {
                                sidx.remove(&entity_id, sv);
                            }
                        }
                    }
                    // Remove old field index entries
                    for entry in self.field_indexes.iter() {
                        if entry.key().starts_with(&format!("{}:", p.collection)) {
                            let old_values: Vec<Value> = entry.field_types().iter()
                                .filter_map(|(fname, _)| old_entity.metadata.get(fname).cloned())
                                .collect();
                            if old_values.len() == entry.field_types().len() {
                                let _ = entry.remove(&entity_id, &old_values);
                            }
                        }
                    }
                }

                // Build entity with metadata
                let mut entity = Entity::new(entity_id.clone());
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

                // LocationUpdate for each representation with appropriate encoding
                for (idx, repr) in entity.representations.iter().enumerate() {
                    let loc_key = ReprKey {
                        entity_id: entity_id.clone(),
                        repr_index: idx as u32,
                    };
                    let encoding = match &repr.vector {
                        VectorData::Sparse(_) => Encoding::Sparse,
                        VectorData::Dense(_) => Encoding::Float32,
                    };
                    let loc_desc = LocationDescriptor {
                        tier: Tier::Fjall,
                        node_address: NodeAddress::localhost(),
                        state: LocState::Clean,
                        version: 1,
                        encoding,
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
                for (idx, repr) in entity.representations.iter().enumerate() {
                    let encoding = match &repr.vector {
                        VectorData::Sparse(_) => Encoding::Sparse,
                        VectorData::Dense(_) => Encoding::Float32,
                    };
                    self.location.register(
                        ReprKey {
                            entity_id: entity_id.clone(),
                            repr_index: idx as u32,
                        },
                        LocationDescriptor {
                            tier: Tier::Fjall,
                            node_address: NodeAddress::localhost(),
                            state: LocState::Clean,
                            version: 1,
                            encoding,
                        },
                    );
                }

                // Apply to HNSW index (only dense vectors, repr-scoped keys)
                for repr in &entity.representations {
                    if let VectorData::Dense(ref vec_f32) = repr.vector {
                        let hnsw_key = format!("{}:{}", p.collection, repr.name);
                        if let Some(hnsw) = self.indexes.get(&hnsw_key) {
                            hnsw.insert(&entity_id, vec_f32);
                        }
                    }
                }

                // Apply to SparseIndex
                for repr in &entity.representations {
                    if let VectorData::Sparse(ref sv) = repr.vector {
                        let sparse_key = format!("{}:{}", p.collection, repr.name);
                        if let Some(sidx) = self.sparse_indexes.get(&sparse_key) {
                            sidx.insert(&entity_id, sv);
                        }
                    }
                }

                // Apply to FieldIndexes
                for entry in self.field_indexes.iter() {
                    if entry.key().starts_with(&format!("{}:", p.collection)) {
                        let values: Vec<Value> = entry.field_types().iter()
                            .filter_map(|(fname, _)| entity.metadata.get(fname).cloned())
                            .collect();
                        if values.len() == entry.field_types().len() {
                            entry.insert(&entity_id, &values)?;
                        }
                    }
                }

                Ok(QueryResult {
                    columns: vec!["result".into()],
                    rows: vec![Row {
                        values: HashMap::from([(
                            "result".into(),
                            Value::String(format!("Inserted entity '{}'", entity_id)),
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
                match &p.strategy {
                    FetchStrategy::FieldIndexLookup(index_name) => {
                        // Look up via FieldIndex
                        let fidx_key = format!("{}:{}", p.collection, index_name);
                        let fidx = self.field_indexes.get(&fidx_key)
                            .ok_or_else(|| EngineError::InvalidQuery(
                                format!("field index '{}' not found", index_name),
                            ))?;

                        let entity_ids = if let Some(WhereClause::Eq(_, lit)) = &p.filter {
                            let value = literal_to_value(lit);
                            fidx.lookup_eq(&[value])?
                        } else {
                            return Err(EngineError::InvalidQuery(
                                "FieldIndexLookup requires Eq filter".into(),
                            ));
                        };

                        let mut rows = Vec::new();
                        for eid in &entity_ids {
                            if let Ok(entity) = self.store.get(&p.collection, eid) {
                                rows.push(entity_to_row(&entity, &p.fields));
                            }
                        }
                        if let Some(limit) = p.limit {
                            rows.truncate(limit);
                        }
                        let columns = build_columns(&rows, &p.fields);
                        Ok(QueryResult {
                            columns,
                            rows,
                            stats: QueryStats {
                                elapsed: start.elapsed(),
                                entities_scanned: entity_ids.len(),
                                mode: QueryMode::Deterministic,
                                tier: "FieldIndex".into(),
                            },
                        })
                    }
                    FetchStrategy::FullScan => {
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
                }
            }

            Plan::Search(p) => {
                // Validate collection exists
                if !self.store.has_collection(&p.collection) {
                    return Err(EngineError::CollectionNotFound(p.collection.clone()));
                }

                // Step 1: Pre-filter — resolve candidate IDs from FieldIndex if present
                let pre_filter_ids: Option<HashSet<LogicalId>> = if let Some(pf) = &p.pre_filter {
                    let fidx_key = format!("{}:{}", p.collection, pf.index_name);
                    let fidx = self.field_indexes.get(&fidx_key)
                        .ok_or_else(|| EngineError::InvalidQuery(
                            format!("pre-filter index '{}' not found", pf.index_name),
                        ))?;
                    let value = match &pf.clause {
                        WhereClause::Eq(_, lit) => literal_to_value(lit),
                        _ => return Err(EngineError::InvalidQuery(
                            "pre-filter only supports Eq".into(),
                        )),
                    };
                    let ids = fidx.lookup_eq(&[value])?;
                    Some(ids.into_iter().collect())
                } else {
                    None
                };

                // Step 2: Over-fetch when pre-filtering
                let fetch_k = if pre_filter_ids.is_some() { p.k * 4 } else { p.k };

                // Step 3: Execute strategy
                let results: Vec<(LogicalId, f32)> = match &p.strategy {
                    SearchStrategy::Hnsw => {
                        let query = p.dense_vector.as_ref()
                            .ok_or_else(|| EngineError::InvalidQuery(
                                "HNSW requires dense vector".into(),
                            ))?;
                        let prefix = format!("{}:", p.collection);
                        let hnsw_key = self.indexes.iter()
                            .find(|e| e.key().starts_with(&prefix))
                            .map(|e| e.key().clone())
                            .ok_or_else(|| EngineError::InvalidQuery(
                                "no HNSW index for collection".into(),
                            ))?;
                        let hnsw = self.indexes.get(&hnsw_key).unwrap();
                        let query_f32: Vec<f32> = query.iter().map(|x| *x as f32).collect();
                        let mut raw = hnsw.search(&query_f32, fetch_k);
                        // Confidence threshold only for HNSW (cosine similarity is meaningful)
                        if p.confidence_threshold > 0.0 {
                            raw.retain(|(_, score)| (*score as f64) >= p.confidence_threshold);
                        }
                        raw
                    }
                    SearchStrategy::Sparse => {
                        let query = p.sparse_vector.as_ref()
                            .ok_or_else(|| EngineError::InvalidQuery(
                                "Sparse requires sparse vector".into(),
                            ))?;
                        let prefix = format!("{}:", p.collection);
                        let sparse_key = self.sparse_indexes.iter()
                            .find(|e| e.key().starts_with(&prefix))
                            .map(|e| e.key().clone())
                            .ok_or_else(|| EngineError::InvalidQuery(
                                "no SparseIndex for collection".into(),
                            ))?;
                        let sidx = self.sparse_indexes.get(&sparse_key).unwrap();
                        // No confidence threshold for sparse (dot product scores)
                        sidx.search(query, fetch_k)
                    }
                    SearchStrategy::Hybrid => {
                        let dense_query = p.dense_vector.as_ref()
                            .ok_or_else(|| EngineError::InvalidQuery(
                                "Hybrid requires dense vector".into(),
                            ))?;
                        let sparse_query = p.sparse_vector.as_ref()
                            .ok_or_else(|| EngineError::InvalidQuery(
                                "Hybrid requires sparse vector".into(),
                            ))?;

                        let prefix = format!("{}:", p.collection);
                        let hnsw_key = self.indexes.iter()
                            .find(|e| e.key().starts_with(&prefix))
                            .map(|e| e.key().clone())
                            .ok_or_else(|| EngineError::InvalidQuery(
                                "no HNSW index for collection".into(),
                            ))?;
                        let hnsw = self.indexes.get(&hnsw_key).unwrap();
                        let query_f32: Vec<f32> = dense_query.iter().map(|x| *x as f32).collect();
                        let dense_results = hnsw.search(&query_f32, fetch_k);

                        let sparse_key = self.sparse_indexes.iter()
                            .find(|e| e.key().starts_with(&prefix))
                            .map(|e| e.key().clone())
                            .ok_or_else(|| EngineError::InvalidQuery(
                                "no SparseIndex for collection".into(),
                            ))?;
                        let sidx = self.sparse_indexes.get(&sparse_key).unwrap();
                        let sparse_results = sidx.search(sparse_query, fetch_k);

                        // RRF merge — no confidence threshold (RRF scores are relative ranking)
                        crate::hybrid::merge_rrf(&dense_results, &sparse_results, crate::hybrid::default_rrf_k())
                    }
                };

                // Step 4: Post-filter on pre-filter candidates
                let filtered = if let Some(ref allowed) = pre_filter_ids {
                    results.into_iter().filter(|(id, _)| allowed.contains(id)).collect::<Vec<_>>()
                } else {
                    results
                };

                // Step 5: Trim to k
                let final_results: Vec<(LogicalId, f32)> = filtered.into_iter().take(p.k).collect();

                // Step 6: Resolve entities using store.get() (not scan!)
                let mut rows = Vec::new();
                for (id, score) in &final_results {
                    if let Ok(entity) = self.store.get(&p.collection, id) {
                        let mut row = entity_to_row(&entity, &p.fields);
                        row.score = Some(*score);
                        rows.push(row);
                    }
                }

                let columns = build_columns(&rows, &p.fields);
                Ok(QueryResult {
                    columns,
                    rows,
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: final_results.len(),
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
            match &p.strategy {
                FetchStrategy::FullScan => {
                    props.push(("strategy", "FullScan".into()));
                    props.push(("tier", "Fjall".into()));
                }
                FetchStrategy::FieldIndexLookup(index_name) => {
                    props.push(("strategy", format!("FieldIndexLookup ({})", index_name)));
                    props.push(("tier", "FieldIndex".into()));
                }
            }
            if let Some(limit) = p.limit {
                props.push(("limit", limit.to_string()));
            }
        }
        Plan::Search(p) => {
            props.push(("mode", "Probabilistic".into()));
            props.push(("verb", "SEARCH".into()));
            props.push(("collection", p.collection.clone()));
            props.push(("tier", "Ram".into()));
            let strategy_str = match &p.strategy {
                SearchStrategy::Hnsw => "HnswSearch",
                SearchStrategy::Sparse => "SparseSearch",
                SearchStrategy::Hybrid => "HybridSearch",
            };
            props.push(("strategy", strategy_str.into()));
            props.push(("k", p.k.to_string()));
            props.push(("confidence_threshold", p.confidence_threshold.to_string()));
            if let Some(pf) = &p.pre_filter {
                props.push(("pre_filter", format!("ScalarPreFilter ({})", pf.index_name)));
            }
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
    use crate::planner::{CreateCollectionPlan, FetchPlan, FetchStrategy, InsertPlan, PreFilter, SearchPlan};
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

    // -----------------------------------------------------------------------
    // Task 9 tests: CREATE COLLECTION validation + INSERT enhancements
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_collection_validates_duplicate_repr() {
        let (exec, _dir) = setup_executor().await;

        let result = exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "test".into(),
            representations: vec![
                trondb_tql::RepresentationDecl {
                    name: "identity".into(),
                    model: None,
                    dimensions: Some(3),
                    metric: trondb_tql::Metric::Cosine,
                    sparse: false,
                },
                trondb_tql::RepresentationDecl {
                    name: "identity".into(),
                    model: None,
                    dimensions: Some(3),
                    metric: trondb_tql::Metric::Cosine,
                    sparse: false,
                },
            ],
            fields: vec![],
            indexes: vec![],
        })).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            EngineError::DuplicateRepresentation(name) => assert_eq!(name, "identity"),
            other => panic!("expected DuplicateRepresentation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_collection_validates_duplicate_field() {
        let (exec, _dir) = setup_executor().await;

        let result = exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "test".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
            }],
            fields: vec![
                trondb_tql::FieldDecl {
                    name: "status".into(),
                    field_type: trondb_tql::FieldType::Text,
                },
                trondb_tql::FieldDecl {
                    name: "status".into(),
                    field_type: trondb_tql::FieldType::Text,
                },
            ],
            indexes: vec![],
        })).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            EngineError::DuplicateField(name) => assert_eq!(name, "status"),
            other => panic!("expected DuplicateField, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_collection_validates_duplicate_index() {
        let (exec, _dir) = setup_executor().await;

        let result = exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "test".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
            }],
            fields: vec![trondb_tql::FieldDecl {
                name: "status".into(),
                field_type: trondb_tql::FieldType::Text,
            }],
            indexes: vec![
                trondb_tql::IndexDecl {
                    name: "idx_status".into(),
                    fields: vec!["status".into()],
                    partial_condition: None,
                },
                trondb_tql::IndexDecl {
                    name: "idx_status".into(),
                    fields: vec!["status".into()],
                    partial_condition: None,
                },
            ],
        })).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            EngineError::DuplicateIndex(name) => assert_eq!(name, "idx_status"),
            other => panic!("expected DuplicateIndex, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_collection_registers_indexes() {
        let (exec, _dir) = setup_executor().await;

        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "docs".into(),
            representations: vec![
                trondb_tql::RepresentationDecl {
                    name: "dense_repr".into(),
                    model: None,
                    dimensions: Some(4),
                    metric: trondb_tql::Metric::Cosine,
                    sparse: false,
                },
                trondb_tql::RepresentationDecl {
                    name: "sparse_repr".into(),
                    model: None,
                    dimensions: None,
                    metric: trondb_tql::Metric::Cosine,
                    sparse: true,
                },
            ],
            fields: vec![trondb_tql::FieldDecl {
                name: "status".into(),
                field_type: trondb_tql::FieldType::Text,
            }],
            indexes: vec![trondb_tql::IndexDecl {
                name: "idx_status".into(),
                fields: vec!["status".into()],
                partial_condition: None,
            }],
        })).await.unwrap();

        // Verify HNSW index registered with repr-scoped key
        assert!(exec.indexes().contains_key("docs:dense_repr"));
        assert_eq!(exec.indexes().get("docs:dense_repr").unwrap().dimensions(), 4);

        // Verify SparseIndex registered
        assert!(exec.sparse_indexes().contains_key("docs:sparse_repr"));

        // Verify FieldIndex registered
        assert!(exec.field_indexes().contains_key("docs:idx_status"));
    }

    #[tokio::test]
    async fn insert_validates_dimensions() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;

        let result = exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into()],
            values: vec![Literal::String("v1".into())],
            vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![1.0, 2.0]))],
        })).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            EngineError::InvalidQuery(msg) => {
                assert!(msg.contains("3 dimensions"), "error should mention expected dims: {msg}");
                assert!(msg.contains("got 2"), "error should mention actual dims: {msg}");
            }
            other => panic!("expected InvalidQuery, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn insert_updates_sparse_index() {
        let (exec, _dir) = setup_executor().await;

        // Create collection with sparse representation
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "docs".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "keywords".into(),
                model: None,
                dimensions: None,
                metric: trondb_tql::Metric::Cosine,
                sparse: true,
            }],
            fields: vec![],
            indexes: vec![],
        })).await.unwrap();

        // Insert with sparse vector
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "docs".into(),
            fields: vec!["id".into()],
            values: vec![Literal::String("d1".into())],
            vectors: vec![("keywords".to_string(), VectorLiteral::Sparse(vec![(1, 0.8), (42, 0.5)]))],
        })).await.unwrap();

        // Verify SparseIndex contains the entry
        let sidx = exec.sparse_indexes().get("docs:keywords").expect("sparse index should exist");
        let results = sidx.search(&[(1, 1.0)], 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, LogicalId::from_string("d1"));
    }

    #[tokio::test]
    async fn insert_updates_field_index() {
        let (exec, _dir) = setup_executor().await;

        // Create collection with a field and index
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "venues".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
            }],
            fields: vec![trondb_tql::FieldDecl {
                name: "city".into(),
                field_type: trondb_tql::FieldType::Text,
            }],
            indexes: vec![trondb_tql::IndexDecl {
                name: "idx_city".into(),
                fields: vec!["city".into()],
                partial_condition: None,
            }],
        })).await.unwrap();

        // Insert with field values
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![Literal::String("v1".into()), Literal::String("London".into())],
            vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.1, 0.2, 0.3]))],
        })).await.unwrap();

        // Verify FieldIndex contains the entry
        let fidx = exec.field_indexes().get("venues:idx_city").expect("field index should exist");
        let results = fidx.lookup_eq(&[Value::String("London".into())]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], LogicalId::from_string("v1"));
    }

    // -----------------------------------------------------------------------
    // Task 10 tests: FETCH FieldIndex, SEARCH sparse/hybrid/prefilter, EXPLAIN
    // -----------------------------------------------------------------------

    /// Helper: create collection with field+index+dense repr for Task 10 tests.
    async fn create_collection_with_field_index(exec: &Executor, name: &str, dims: usize) {
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: name.into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(dims),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
            }],
            fields: vec![trondb_tql::FieldDecl {
                name: "city".into(),
                field_type: trondb_tql::FieldType::Text,
            }],
            indexes: vec![trondb_tql::IndexDecl {
                name: "idx_city".into(),
                fields: vec!["city".into()],
                partial_condition: None,
            }],
        })).await.unwrap();
    }

    #[tokio::test]
    async fn fetch_via_field_index() {
        let (exec, _dir) = setup_executor().await;
        create_collection_with_field_index(&exec, "venues", 3).await;

        // Insert two entities with different cities
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![Literal::String("v1".into()), Literal::String("London".into())],
            vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.1, 0.2, 0.3]))],
        })).await.unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![Literal::String("v2".into()), Literal::String("Paris".into())],
            vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.4, 0.5, 0.6]))],
        })).await.unwrap();

        // FETCH with FieldIndexLookup strategy
        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            limit: None,
            strategy: FetchStrategy::FieldIndexLookup("idx_city".into()),
        })).await.unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].values.get("city"), Some(&Value::String("London".into())));
        assert_eq!(result.rows[0].values.get("id"), Some(&Value::String("v1".into())));
        assert_eq!(result.stats.tier, "FieldIndex");
    }

    #[tokio::test]
    async fn search_sparse_returns_ranked() {
        let (exec, _dir) = setup_executor().await;

        // Create collection with sparse representation
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "docs".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "keywords".into(),
                model: None,
                dimensions: None,
                metric: trondb_tql::Metric::Cosine,
                sparse: true,
            }],
            fields: vec![],
            indexes: vec![],
        })).await.unwrap();

        // Insert entities with sparse vectors
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "docs".into(),
            fields: vec!["id".into()],
            values: vec![Literal::String("d1".into())],
            vectors: vec![("keywords".to_string(), VectorLiteral::Sparse(vec![(1, 0.8), (42, 0.5)]))],
        })).await.unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "docs".into(),
            fields: vec!["id".into()],
            values: vec![Literal::String("d2".into())],
            vectors: vec![("keywords".to_string(), VectorLiteral::Sparse(vec![(1, 0.3), (99, 0.9)]))],
        })).await.unwrap();

        // SEARCH Sparse
        let result = exec.execute(&Plan::Search(SearchPlan {
            collection: "docs".into(),
            fields: FieldList::All,
            dense_vector: None,
            sparse_vector: Some(vec![(1, 1.0)]),
            filter: None,
            pre_filter: None,
            k: 10,
            confidence_threshold: 0.0,
            strategy: SearchStrategy::Sparse,
        })).await.unwrap();

        assert_eq!(result.rows.len(), 2);
        // d1 should rank higher (dimension 1 weight 0.8 > d2's 0.3)
        assert_eq!(result.rows[0].values.get("id"), Some(&Value::String("d1".into())));
        assert!(result.rows[0].score.unwrap() > result.rows[1].score.unwrap());
        assert_eq!(result.stats.mode, QueryMode::Probabilistic);
    }

    #[tokio::test]
    async fn search_hybrid_merges_dense_sparse() {
        let (exec, _dir) = setup_executor().await;

        // Create collection with both dense + sparse representations
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "docs".into(),
            representations: vec![
                trondb_tql::RepresentationDecl {
                    name: "embed".into(),
                    model: None,
                    dimensions: Some(3),
                    metric: trondb_tql::Metric::Cosine,
                    sparse: false,
                },
                trondb_tql::RepresentationDecl {
                    name: "keywords".into(),
                    model: None,
                    dimensions: None,
                    metric: trondb_tql::Metric::Cosine,
                    sparse: true,
                },
            ],
            fields: vec![],
            indexes: vec![],
        })).await.unwrap();

        // Insert entities with both dense and sparse vectors
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "docs".into(),
            fields: vec!["id".into()],
            values: vec![Literal::String("d1".into())],
            vectors: vec![
                ("embed".to_string(), VectorLiteral::Dense(vec![1.0, 0.0, 0.0])),
                ("keywords".to_string(), VectorLiteral::Sparse(vec![(1, 0.9)])),
            ],
        })).await.unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "docs".into(),
            fields: vec!["id".into()],
            values: vec![Literal::String("d2".into())],
            vectors: vec![
                ("embed".to_string(), VectorLiteral::Dense(vec![0.0, 1.0, 0.0])),
                ("keywords".to_string(), VectorLiteral::Sparse(vec![(2, 0.9)])),
            ],
        })).await.unwrap();

        // SEARCH Hybrid
        let result = exec.execute(&Plan::Search(SearchPlan {
            collection: "docs".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: Some(vec![(1, 1.0)]),
            filter: None,
            pre_filter: None,
            k: 10,
            confidence_threshold: 0.0,
            strategy: SearchStrategy::Hybrid,
        })).await.unwrap();

        // Both entities should appear, d1 should rank higher (matches both dense and sparse)
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0].values.get("id"), Some(&Value::String("d1".into())));
        assert!(result.rows[0].score.is_some());
    }

    #[tokio::test]
    async fn search_with_prefilter_narrows_results() {
        let (exec, _dir) = setup_executor().await;

        // Create collection with dense repr + field index
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "venues".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
            }],
            fields: vec![trondb_tql::FieldDecl {
                name: "city".into(),
                field_type: trondb_tql::FieldType::Text,
            }],
            indexes: vec![trondb_tql::IndexDecl {
                name: "idx_city".into(),
                fields: vec!["city".into()],
                partial_condition: None,
            }],
        })).await.unwrap();

        // Insert entities in different cities
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![Literal::String("v1".into()), Literal::String("London".into())],
            vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
        })).await.unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![Literal::String("v2".into()), Literal::String("Paris".into())],
            vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.9, 0.1, 0.0]))],
        })).await.unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![Literal::String("v3".into()), Literal::String("London".into())],
            vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.8, 0.2, 0.0]))],
        })).await.unwrap();

        // SEARCH with pre-filter on city=London
        let result = exec.execute(&Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            pre_filter: Some(PreFilter {
                index_name: "idx_city".into(),
                clause: WhereClause::Eq("city".into(), Literal::String("London".into())),
            }),
            k: 10,
            confidence_threshold: 0.0,
            strategy: SearchStrategy::Hnsw,
        })).await.unwrap();

        // Only London entities should be returned
        assert_eq!(result.rows.len(), 2);
        for row in &result.rows {
            assert_eq!(row.values.get("city"), Some(&Value::String("London".into())));
        }
    }

    #[tokio::test]
    async fn explain_shows_search_strategy() {
        let (exec, _dir) = setup_executor().await;

        let search_plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: None,
            pre_filter: None,
            k: 5,
            confidence_threshold: 0.8,
            strategy: SearchStrategy::Hnsw,
        });

        let result = exec
            .execute(&Plan::Explain(Box::new(search_plan)))
            .await
            .unwrap();

        // Find strategy property
        let strategy_row = result.rows.iter()
            .find(|r| r.values.get("property") == Some(&Value::String("strategy".into())))
            .expect("should have 'strategy' property");
        assert_eq!(strategy_row.values.get("value"), Some(&Value::String("HnswSearch".into())));

        // Verify mode
        let mode_row = result.rows.iter()
            .find(|r| r.values.get("property") == Some(&Value::String("mode".into())))
            .expect("should have 'mode' property");
        assert_eq!(mode_row.values.get("value"), Some(&Value::String("Probabilistic".into())));

        // Verify k
        let k_row = result.rows.iter()
            .find(|r| r.values.get("property") == Some(&Value::String("k".into())))
            .expect("should have 'k' property");
        assert_eq!(k_row.values.get("value"), Some(&Value::String("5".into())));
    }

    #[tokio::test]
    async fn explain_shows_fetch_field_index_strategy() {
        let (exec, _dir) = setup_executor().await;

        let fetch_plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            limit: None,
            strategy: FetchStrategy::FieldIndexLookup("idx_city".into()),
        });

        let result = exec
            .execute(&Plan::Explain(Box::new(fetch_plan)))
            .await
            .unwrap();

        let strategy_row = result.rows.iter()
            .find(|r| r.values.get("property") == Some(&Value::String("strategy".into())))
            .expect("should have 'strategy' property");
        assert_eq!(
            strategy_row.values.get("value"),
            Some(&Value::String("FieldIndexLookup (idx_city)".into()))
        );

        let tier_row = result.rows.iter()
            .find(|r| r.values.get("property") == Some(&Value::String("tier".into())))
            .expect("should have 'tier' property");
        assert_eq!(tier_row.values.get("value"), Some(&Value::String("FieldIndex".into())));
    }

    #[tokio::test]
    async fn explain_shows_prefilter() {
        let (exec, _dir) = setup_executor().await;

        let search_plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            pre_filter: Some(PreFilter {
                index_name: "idx_city".into(),
                clause: WhereClause::Eq("city".into(), Literal::String("London".into())),
            }),
            k: 5,
            confidence_threshold: 0.0,
            strategy: SearchStrategy::Hnsw,
        });

        let result = exec
            .execute(&Plan::Explain(Box::new(search_plan)))
            .await
            .unwrap();

        let pf_row = result.rows.iter()
            .find(|r| r.values.get("property") == Some(&Value::String("pre_filter".into())))
            .expect("should have 'pre_filter' property");
        assert_eq!(
            pf_row.values.get("value"),
            Some(&Value::String("ScalarPreFilter (idx_city)".into()))
        );
    }
}
