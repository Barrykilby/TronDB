use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;

use crate::edge::{AdjacencyIndex, Edge, EdgeType};
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
use crate::vectoriser::{FieldSet, VectoriserRegistry};
use trondb_tql::{FieldList, Literal, VectorLiteral, WhereClause};
use trondb_wal::{RecordType, WalRecord, WalWriter};

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

pub struct Executor {
    store: FjallStore,
    wal: Arc<WalWriter>,
    location: Arc<LocationTable>,
    indexes: Arc<DashMap<String, HnswIndex>>,
    sparse_indexes: DashMap<String, SparseIndex>,
    field_indexes: DashMap<String, FieldIndex>,
    schemas: DashMap<String, CollectionSchema>,
    adjacency: AdjacencyIndex,
    edge_types: DashMap<String, EdgeType>,
    vectoriser_registry: Arc<VectoriserRegistry>,
    /// Reverse map: entity_id → collection name (for recompute_dirty)
    entity_collections: DashMap<LogicalId, String>,
}

impl Executor {
    pub fn new(
        store: FjallStore,
        wal: WalWriter,
        location: Arc<LocationTable>,
        vectoriser_registry: Arc<VectoriserRegistry>,
    ) -> Self {
        Self {
            store,
            wal: Arc::new(wal),
            location,
            indexes: Arc::new(DashMap::new()),
            sparse_indexes: DashMap::new(),
            field_indexes: DashMap::new(),
            schemas: DashMap::new(),
            adjacency: AdjacencyIndex::new(),
            edge_types: DashMap::new(),
            vectoriser_registry,
            entity_collections: DashMap::new(),
        }
    }

    /// Replay committed WAL records into the Fjall store.
    /// Called during engine startup to close the durability gap.
    pub fn replay_wal_records(&self, records: &[WalRecord]) -> Result<(usize, Vec<WalRecord>), EngineError> {
        let mut replayed = 0;
        let mut unhandled = Vec::new();

        for record in records {
            match record.record_type {
                RecordType::SchemaCreateColl => {
                    let schema: CollectionSchema = rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Storage(
                            format!("failed to deserialize CollectionSchema from WAL: {}. \
                                     If upgrading from pre-Phase 5a, delete trondb_data/wal/ and restart.", e)
                        ))?;
                    if !self.store.has_collection(&schema.name) {
                        self.store.create_collection_schema(&schema)?;
                        replayed += 1;
                    }
                    self.schemas.insert(schema.name.clone(), schema);
                }
                RecordType::EntityWrite => {
                    let entity: Entity = rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;

                    // Track entity → collection mapping (for recompute_dirty)
                    self.entity_collections.insert(entity.id.clone(), record.collection.clone());

                    // Idempotent: overwrite with WAL version (WAL is authoritative)
                    if self.store.has_collection(&record.collection) {
                        self.store.insert(&record.collection, entity)?;
                        replayed += 1;
                    }
                }
                RecordType::EntityDelete => {
                    #[derive(serde::Deserialize)]
                    struct EntityDeletePayload {
                        entity_id: String,
                        collection: String,
                    }
                    let payload: EntityDeletePayload = rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                    let entity_id = LogicalId::from_string(&payload.entity_id);

                    // Delete from main Fjall partition
                    if self.store.has_collection(&payload.collection) {
                        let _ = self.store.delete_entity(&payload.collection, &entity_id);
                    }
                    // Clean up tiered storage partitions
                    let _ = self.store.delete_from_tier(&payload.collection, &entity_id, Tier::NVMe);
                    let _ = self.store.delete_from_tier(&payload.collection, &entity_id, Tier::Archive);
                    // Location table cleanup
                    self.location.remove_entity(&entity_id);
                    replayed += 1;
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
                    unhandled.push(record.clone());
                }
            }
        }

        if replayed > 0 {
            self.store.persist()?;
        }

        Ok((replayed, unhandled))
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

                    // Look up schema representation to get its declared FIELDS list
                    let stored_repr = schema.representations.iter()
                        .find(|r| r.name == *repr_name);

                    let recipe_hash = match stored_repr {
                        Some(sr) if !sr.fields.is_empty() => {
                            let model_id = schema.vectoriser_config.as_ref()
                                .and_then(|vc| vc.model.as_deref())
                                .unwrap_or("unknown");
                            crate::vectoriser::compute_recipe_hash(model_id, &sr.fields)
                        }
                        _ => [0u8; 32], // passthrough — no staleness tracking
                    };

                    let repr = Representation {
                        name: repr_name.clone(),
                        repr_type: ReprType::Atomic,
                        fields: p.fields.clone(),
                        vector: vector_data,
                        recipe_hash,
                        state: ReprState::Clean,
                    };
                    entity.representations.push(repr);
                }

                // Auto-vectorise managed representations that weren't explicitly provided
                let explicit_repr_names: HashSet<&str> = p.vectors.iter()
                    .map(|(name, _)| name.as_str())
                    .collect();

                for stored_repr in &schema.representations {
                    if stored_repr.fields.is_empty() {
                        continue; // passthrough — no auto-vectorisation
                    }
                    if explicit_repr_names.contains(stored_repr.name.as_str()) {
                        continue; // explicit vector was provided
                    }

                    // Build FieldSet from entity metadata
                    let field_set: FieldSet = stored_repr.fields.iter()
                        .filter_map(|f| {
                            entity.metadata.get(f).map(|v| (f.clone(), v.to_string()))
                        })
                        .collect();

                    if field_set.is_empty() {
                        continue; // no matching fields in entity
                    }

                    // Look up vectoriser
                    let vectoriser = self.vectoriser_registry
                        .get(&p.collection, &stored_repr.name)
                        .ok_or_else(|| EngineError::InvalidQuery(format!(
                            "representation '{}' has FIELDS but no vectoriser registered for collection '{}'",
                            stored_repr.name, p.collection
                        )))?;

                    let vector_data = vectoriser.encode(&field_set).await
                        .map_err(|e| EngineError::Storage(format!("vectoriser encode failed: {e}")))?;

                    let model_id = vectoriser.model_id();
                    let recipe_hash = crate::vectoriser::compute_recipe_hash(model_id, &stored_repr.fields);

                    let repr = Representation {
                        name: stored_repr.name.clone(),
                        repr_type: if stored_repr.fields.len() > 1 { ReprType::Composite } else { ReprType::Atomic },
                        fields: stored_repr.fields.clone(),
                        vector: vector_data,
                        recipe_hash,
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
                        last_accessed: 0,
                    };
                    let loc_payload = rmp_serde::to_vec_named(&(&loc_key, &loc_desc))
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                    self.wal.append(RecordType::LocationUpdate, &p.collection, tx_id, 1, loc_payload);
                }

                self.wal.commit(tx_id).await?;

                // Apply to Fjall
                self.store.insert(&p.collection, entity.clone())?;
                self.store.persist()?;

                // Track entity → collection mapping (for recompute_dirty)
                self.entity_collections.insert(entity_id.clone(), p.collection.clone());

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
                            last_accessed: 0,
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
                    FetchStrategy::FieldIndexRange(index_name) => {
                        let fidx_key = format!("{}:{}", p.collection, index_name);
                        let fidx = self.field_indexes.get(&fidx_key)
                            .ok_or_else(|| EngineError::InvalidQuery(
                                format!("field index '{}' not found", index_name),
                            ))?;

                        // Compute range bounds from the filter
                        let entity_ids = match &p.filter {
                            Some(clause) => {
                                let (lower, upper) = range_bounds_from_clause(clause);
                                fidx.lookup_range(&lower, &upper)?
                            }
                            None => {
                                return Err(EngineError::InvalidQuery(
                                    "FieldIndexRange requires a range filter".into(),
                                ));
                            }
                        };

                        let mut rows = Vec::new();
                        for eid in &entity_ids {
                            if let Ok(entity) = self.store.get(&p.collection, eid) {
                                // Post-filter: entity_matches handles strict Gt/Lt boundary exclusion
                                if p.filter.as_ref().map(|c| entity_matches(&entity, c)).unwrap_or(true) {
                                    rows.push(entity_to_row(&entity, &p.fields));
                                }
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

                let decay_config = p.decay_config.as_ref().map(|dc| {
                    crate::edge::DecayConfig {
                        decay_fn: dc.decay_fn.as_ref().map(|f| match f {
                            trondb_tql::DecayFnDecl::Exponential => crate::edge::DecayFn::Exponential,
                            trondb_tql::DecayFnDecl::Linear => crate::edge::DecayFn::Linear,
                            trondb_tql::DecayFnDecl::Step => crate::edge::DecayFn::Step,
                        }),
                        decay_rate: dc.decay_rate,
                        floor: dc.floor,
                        promote_threshold: dc.promote_threshold,
                        prune_threshold: dc.prune_threshold,
                    }
                }).unwrap_or_default();

                let edge_type = EdgeType {
                    name: p.name.clone(),
                    from_collection: p.from_collection.clone(),
                    to_collection: p.to_collection.clone(),
                    decay_config,
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

                let now_millis = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;

                let edge = Edge {
                    from_id: from_id.clone(),
                    to_id: to_id.clone(),
                    edge_type: p.edge_type.clone(),
                    confidence: 1.0,
                    metadata,
                    created_at: now_millis,
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
                self.adjacency.insert(&from_id, &p.edge_type, &to_id, 1.0, now_millis);

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
                #[derive(serde::Serialize)]
                struct EdgeDeletePayload<'a> {
                    edge_type: &'a str,
                    from_id: &'a str,
                    to_id: &'a str,
                }
                let payload = rmp_serde::to_vec_named(&EdgeDeletePayload {
                    edge_type: &p.edge_type,
                    from_id: &p.from_id,
                    to_id: &p.to_id,
                })
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

                // Cap depth at 10
                let max_depth = p.depth.min(10);

                let from_id = LogicalId::from_string(&p.from_id);
                let mut visited: HashSet<LogicalId> = HashSet::new();
                visited.insert(from_id.clone());

                let mut frontier = vec![from_id];
                let mut rows = Vec::new();
                let limit = p.limit.unwrap_or(usize::MAX);

                for _hop in 0..max_depth {
                    if frontier.is_empty() || rows.len() >= limit {
                        break;
                    }

                    let mut next_frontier = Vec::new();

                    for node_id in &frontier {
                        let entries = self.adjacency.get(node_id, &p.edge_type);
                        for entry in &entries {
                            if visited.contains(&entry.to_id) {
                                continue;
                            }

                            // Apply edge decay if edge type has decay config
                            let effective_conf = if entry.created_at > 0 {
                                let now_ms = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap()
                                    .as_millis() as u64;
                                let elapsed = now_ms.saturating_sub(entry.created_at);
                                crate::edge::effective_confidence(
                                    entry.confidence,
                                    elapsed,
                                    &edge_type.decay_config,
                                )
                            } else {
                                entry.confidence
                            };

                            // Skip edges below prune threshold
                            if let Some(prune) = edge_type.decay_config.prune_threshold {
                                if effective_conf < prune as f32 {
                                    continue;
                                }
                            }

                            visited.insert(entry.to_id.clone());

                            if let Ok(entity) = self.store.get(&edge_type.to_collection, &entry.to_id) {
                                rows.push(entity_to_row(&entity, &FieldList::All));
                                if rows.len() >= limit {
                                    break;
                                }
                            }
                            next_frontier.push(entry.to_id.clone());
                        }
                        if rows.len() >= limit {
                            break;
                        }
                    }

                    frontier = next_frontier;
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

            Plan::CreateAffinityGroup(_) | Plan::AlterEntityDropAffinity(_) => {
                // These are handled by the routing layer, not the core executor.
                // Return a simple acknowledgment.
                Ok(QueryResult {
                    columns: vec!["status".into()],
                    rows: vec![Row {
                        values: HashMap::from([
                            ("status".to_owned(), crate::types::Value::String("OK".into())),
                        ]),
                        score: None,
                    }],
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: 0,
                        mode: QueryMode::Deterministic,
                        tier: "Routing".into(),
                    },
                })
            }

            Plan::Demote(_) | Plan::Promote(_) => {
                // Handled by the routing layer (TierMigrator / SemanticRouter)
                let start = std::time::Instant::now();
                Ok(QueryResult {
                    columns: vec!["status".to_string()],
                    rows: vec![Row {
                        values: HashMap::from([("status".into(), Value::String("OK".into()))]),
                        score: None,
                    }],
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: 0,
                        mode: QueryMode::Deterministic,
                        tier: "Routing".into(),
                    },
                })
            }

            Plan::DeleteEntity(p) => {
                let entity_id = LogicalId::from_string(&p.entity_id);

                // Step 1: Read entity before deletion (needed for index cleanup)
                let entity = self.store.get(&p.collection, &entity_id)?;

                // Step 2: WAL log
                let tx_id = self.wal.next_tx_id();
                self.wal.append(RecordType::TxBegin, &p.collection, tx_id, 1, vec![]);
                #[derive(serde::Serialize)]
                struct EntityDeletePayload<'a> {
                    entity_id: &'a str,
                    collection: &'a str,
                }
                let payload = rmp_serde::to_vec_named(&EntityDeletePayload {
                    entity_id: &p.entity_id,
                    collection: &p.collection,
                })
                .map_err(|e| EngineError::Storage(e.to_string()))?;
                self.wal.append(RecordType::EntityDelete, &p.collection, tx_id, 1, payload);
                self.wal.commit(tx_id).await?;

                // Step 3: HNSW tombstone — indexes keyed as "{collection}:{repr_name}"
                let hnsw_prefix = format!("{}:", p.collection);
                for entry in self.indexes.iter() {
                    if entry.key().starts_with(&hnsw_prefix) {
                        entry.value().remove(&entity_id);
                    }
                }

                // Step 4: Field index removal
                let schema = self.schemas.get(&p.collection);
                if let Some(schema) = &schema {
                    for idx_def in &schema.indexes {
                        let fidx_key = format!("{}:{}", p.collection, idx_def.name);
                        if let Some(fidx) = self.field_indexes.get(&fidx_key) {
                            let values: Vec<Value> = idx_def.fields.iter()
                                .map(|f| entity.metadata.get(f).cloned().unwrap_or(Value::Null))
                                .collect();
                            let _ = fidx.remove(&entity_id, &values);
                        }
                    }
                }

                // Step 5: Sparse index removal — keyed as "{collection}:{repr_name}"
                for repr in &entity.representations {
                    if let VectorData::Sparse(ref sv) = repr.vector {
                        let sparse_key = format!("{}:{}", p.collection, repr.name);
                        if let Some(sparse_idx) = self.sparse_indexes.get(&sparse_key) {
                            sparse_idx.remove(&entity_id, sv);
                        }
                    }
                }

                // Step 6: Location table removal (remove_entity removes all reprs)
                self.location.remove_entity(&entity_id);

                // Step 7: Fjall delete
                self.store.delete_entity(&p.collection, &entity_id)?;

                // Step 8: Edge cleanup — find and delete all edges involving this entity
                let (forward_edges, backward_edges) = self.adjacency.edges_involving(&entity_id);
                for (edge_type, to_id) in &forward_edges {
                    self.store.delete_edge(edge_type, entity_id.as_str(), to_id.as_str())?;
                    self.adjacency.remove(&entity_id, edge_type, to_id);
                }
                for (edge_type, from_id) in &backward_edges {
                    self.store.delete_edge(edge_type, from_id.as_str(), entity_id.as_str())?;
                    self.adjacency.remove(from_id, edge_type, &entity_id);
                }

                // Step 9: Tiered storage cleanup
                let _ = self.store.delete_from_tier(&p.collection, &entity_id, Tier::NVMe);
                let _ = self.store.delete_from_tier(&p.collection, &entity_id, Tier::Archive);

                self.store.persist()?;

                Ok(QueryResult {
                    columns: vec!["result".into()],
                    rows: vec![Row {
                        values: HashMap::from([(
                            "result".into(),
                            Value::String(format!("Entity '{}' deleted from '{}'", p.entity_id, p.collection)),
                        )]),
                        score: None,
                    }],
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: 1,
                        mode: QueryMode::Deterministic,
                        tier: "Fjall".into(),
                    },
                })
            }

            Plan::ExplainTiers(ref p) => {
                let start = std::time::Instant::now();
                let hot = self.store.tier_entity_count(&p.collection, Tier::Fjall).unwrap_or(0);
                let warm = self.store.tier_entity_count(&p.collection, Tier::NVMe).unwrap_or(0);
                let archive = self.store.tier_entity_count(&p.collection, Tier::Archive).unwrap_or(0);

                let mut rows = Vec::new();
                for (tier_name, count) in [("Hot", hot), ("Warm", warm), ("Archive", archive)] {
                    rows.push(Row {
                        values: HashMap::from([
                            ("tier".into(), Value::String(tier_name.into())),
                            ("entity_count".into(), Value::Int(count as i64)),
                        ]),
                        score: None,
                    });
                }

                Ok(QueryResult {
                    columns: vec!["tier".to_string(), "entity_count".to_string()],
                    rows,
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: 0,
                        mode: QueryMode::Deterministic,
                        tier: "Fjall".into(),
                    },
                })
            }
            Plan::UpdateEntity(p) => {
                let entity_id = LogicalId::from_string(&p.entity_id);

                // Step 1: Read current entity (errors if not found)
                let mut entity = self.store.get(&p.collection, &entity_id)?;

                // Step 2: Capture old field values for indexed fields (before mutation)
                let schema = self.schemas.get(&p.collection);
                let old_field_values: Vec<(String, Vec<Value>)> = if let Some(ref s) = schema {
                    s.indexes.iter().map(|idx_def| {
                        let fidx_key = format!("{}:{}", p.collection, idx_def.name);
                        let values: Vec<Value> = idx_def.fields.iter()
                            .map(|f| entity.metadata.get(f).cloned().unwrap_or(Value::Null))
                            .collect();
                        (fidx_key, values)
                    }).collect()
                } else {
                    Vec::new()
                };

                // Step 3: Apply assignments
                for (field, literal) in &p.assignments {
                    let value = literal_to_value(literal);
                    entity.metadata.insert(field.clone(), value);
                }

                // Step 4: Dirty detection — which representations are affected by changed fields?
                let changed_fields: HashSet<&str> = p.assignments.iter()
                    .map(|(f, _)| f.as_str())
                    .collect();

                let dirty_repr_indices: Vec<u32> = if let Some(ref s) = schema {
                    s.representations.iter().enumerate()
                        .filter(|(_, stored_repr)| {
                            // passthrough representations (no FIELDS) are never dirty from field changes
                            !stored_repr.fields.is_empty()
                                && stored_repr.fields.iter().any(|f| changed_fields.contains(f.as_str()))
                        })
                        .map(|(idx, _)| idx as u32)
                        .collect()
                } else {
                    Vec::new()
                };

                // Step 5: WAL append — EntityWrite + ReprDirty records in same transaction
                let tx_id = self.wal.next_tx_id();
                self.wal.append(RecordType::TxBegin, &p.collection, tx_id, 1, vec![]);
                let payload = rmp_serde::to_vec_named(&entity)
                    .map_err(|e| EngineError::Storage(e.to_string()))?;
                self.wal.append(RecordType::EntityWrite, &p.collection, tx_id, 1, payload);

                // Append ReprDirty WAL records for affected representations
                for &repr_idx in &dirty_repr_indices {
                    let repr_key = ReprKey {
                        entity_id: entity_id.clone(),
                        repr_index: repr_idx,
                    };
                    let dirty_payload = rmp_serde::to_vec_named(&repr_key)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                    self.wal.append(RecordType::ReprDirty, &p.collection, tx_id, 1, dirty_payload);
                }

                self.wal.commit(tx_id).await?;

                // Step 6: Write to Fjall
                self.store.insert(&p.collection, entity.clone())?;
                self.store.persist()?;

                // Step 7: Transition Location Table entries to Dirty
                for &repr_idx in &dirty_repr_indices {
                    let loc_key = ReprKey {
                        entity_id: entity_id.clone(),
                        repr_index: repr_idx,
                    };
                    // Best-effort: location entry may not exist (e.g. entity had no vector)
                    let _ = self.location.transition(&loc_key, LocState::Dirty);
                }

                // Step 8: Update field indexes — remove old, insert new
                if let Some(ref s) = schema {
                    for (idx_def, (fidx_key, old_values)) in s.indexes.iter().zip(old_field_values.iter()) {
                        if let Some(fidx) = self.field_indexes.get(fidx_key) {
                            // Remove old entry (best-effort — old entry may not exist)
                            let _ = fidx.remove(&entity_id, old_values);
                            // Insert new entry
                            let new_values: Vec<Value> = idx_def.fields.iter()
                                .map(|f| entity.metadata.get(f).cloned().unwrap_or(Value::Null))
                                .collect();
                            if new_values.len() == fidx.field_types().len() {
                                fidx.insert(&entity_id, &new_values)?;
                            }
                        }
                    }
                }

                Ok(QueryResult {
                    columns: vec!["result".into()],
                    rows: vec![Row {
                        values: HashMap::from([(
                            "result".into(),
                            Value::String(format!("Entity '{}' updated in '{}'", p.entity_id, p.collection)),
                        )]),
                        score: None,
                    }],
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: 1,
                        mode: QueryMode::Deterministic,
                        tier: "Fjall".into(),
                    },
                })
            }
        }
    }

    pub fn entity_count(&self) -> usize {
        self.store.entity_count()
    }

    pub fn collection_count(&self) -> usize {
        self.schemas.len()
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

    pub fn wal_writer(&self) -> Arc<WalWriter> {
        Arc::clone(&self.wal)
    }

    pub fn indexes(&self) -> &Arc<DashMap<String, HnswIndex>> {
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

    pub fn vectoriser_registry(&self) -> &Arc<VectoriserRegistry> {
        &self.vectoriser_registry
    }

    pub fn list_edge_types(&self) -> Vec<EdgeType> {
        self.store.list_edge_types()
    }

    pub fn scan_edges(&self, edge_type: &str) -> Result<Vec<Edge>, EngineError> {
        self.store.scan_edges(edge_type)
    }

    pub fn entity_collections(&self) -> &DashMap<LogicalId, String> {
        &self.entity_collections
    }

    /// Look up which collection an entity belongs to (via the reverse map).
    fn find_collection_for_entity(&self, entity_id: &LogicalId) -> Result<String, EngineError> {
        self.entity_collections
            .get(entity_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| EngineError::EntityNotFound(entity_id.to_string()))
    }

    /// Scan for Dirty representations, re-encode via the vectoriser,
    /// write the new vector, and transition back to Clean.
    pub async fn recompute_dirty(&self) -> Result<usize, EngineError> {
        let dirty_keys = self.location.iter_dirty();
        let mut recomputed = 0;

        for (repr_key, _loc_desc) in &dirty_keys {
            let collection = match self.find_collection_for_entity(&repr_key.entity_id) {
                Ok(c) => c,
                Err(_) => continue, // entity might have been deleted
            };

            let entity = match self.store.get(&collection, &repr_key.entity_id) {
                Ok(e) => e,
                Err(_) => continue,
            };

            let schema = match self.schemas.get(&collection) {
                Some(s) => s.clone(),
                None => continue,
            };

            let repr_idx = repr_key.repr_index as usize;
            if repr_idx >= schema.representations.len() {
                continue;
            }

            let stored_repr = &schema.representations[repr_idx];
            if stored_repr.fields.is_empty() {
                continue; // passthrough — no auto-recomputation
            }

            // Build FieldSet from current metadata
            let field_set: FieldSet = stored_repr
                .fields
                .iter()
                .filter_map(|f| entity.metadata.get(f).map(|v| (f.clone(), v.to_string())))
                .collect();

            let vectoriser = match self.vectoriser_registry.get(&collection, &stored_repr.name) {
                Some(v) => v,
                None => continue,
            };

            // Transition to Recomputing
            self.location.transition(repr_key, LocState::Recomputing)?;

            // Encode new vector
            let vector_data = vectoriser
                .encode(&field_set)
                .await
                .map_err(|e| EngineError::Storage(format!("recompute failed: {e}")))?;

            let model_id = vectoriser.model_id();
            let recipe_hash =
                crate::vectoriser::compute_recipe_hash(model_id, &stored_repr.fields);

            // Update entity with new vector + Clean state
            let mut updated_entity = entity.clone();
            if repr_idx < updated_entity.representations.len() {
                updated_entity.representations[repr_idx].vector = vector_data;
                updated_entity.representations[repr_idx].state = ReprState::Clean;
                updated_entity.representations[repr_idx].recipe_hash = recipe_hash;
            }

            // WAL: ReprWrite
            let tx_id = self.wal.next_tx_id();
            self.wal
                .append(RecordType::TxBegin, &collection, tx_id, 1, vec![]);
            let payload = rmp_serde::to_vec_named(&updated_entity)
                .map_err(|e| EngineError::Storage(e.to_string()))?;
            self.wal
                .append(RecordType::ReprWrite, &collection, tx_id, 1, payload);
            self.wal.commit(tx_id).await?;

            // Fjall persist
            self.store.insert(&collection, updated_entity.clone())?;
            self.store.persist()?;

            // Update HNSW index
            if let Some(repr) = updated_entity.representations.get(repr_idx) {
                if let VectorData::Dense(ref vec_f32) = repr.vector {
                    let hnsw_key = format!("{}:{}", collection, repr.name);
                    if let Some(hnsw) = self.indexes.get(&hnsw_key) {
                        hnsw.insert(&repr_key.entity_id, vec_f32);
                    }
                }
            }

            // Transition to Clean
            self.location.transition(repr_key, LocState::Clean)?;
            recomputed += 1;
        }

        Ok(recomputed)
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
            fields: r.fields.clone(),
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

    let vectoriser_config = p.vectoriser_config.as_ref().map(|vc| {
        crate::types::VectoriserConfig {
            model: vc.model.clone(),
            model_path: vc.model_path.clone(),
            device: vc.device.clone(),
            vectoriser_type: vc.vectoriser_type.clone(),
            endpoint: vc.endpoint.clone(),
            auth: vc.auth.clone(),
        }
    });

    CollectionSchema {
        name: p.name.clone(),
        representations,
        fields,
        indexes,
        vectoriser_config,
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
        WhereClause::Gte(field, lit) => {
            let threshold = literal_to_value(lit);
            entity
                .metadata
                .get(field)
                .map(|v| value_gt(v, &threshold) || v == &threshold)
                .unwrap_or(false)
        }
        WhereClause::Lte(field, lit) => {
            let threshold = literal_to_value(lit);
            entity
                .metadata
                .get(field)
                .map(|v| value_lt(v, &threshold) || v == &threshold)
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

/// Extract approximate lower/upper bound `Value` vectors from a `WhereClause`
/// for use with `FieldIndex::lookup_range()`. The bounds are intentionally
/// inclusive — strict Gt/Lt exclusion is handled by the post-filter
/// (`entity_matches`).
fn range_bounds_from_clause(clause: &WhereClause) -> (Vec<Value>, Vec<Value>) {
    match clause {
        WhereClause::Gt(_, lit) | WhereClause::Gte(_, lit) => {
            (vec![literal_to_value(lit)], vec![Value::Int(i64::MAX)])
        }
        WhereClause::Lt(_, lit) | WhereClause::Lte(_, lit) => {
            (vec![Value::Int(i64::MIN)], vec![literal_to_value(lit)])
        }
        WhereClause::And(left, right) => {
            let (l_lower, l_upper) = range_bounds_from_clause(left);
            let (r_lower, r_upper) = range_bounds_from_clause(right);
            // Take the tighter (more restrictive) bound from each side
            let lower = pick_tighter_lower(&l_lower, &r_lower);
            let upper = pick_tighter_upper(&l_upper, &r_upper);
            (lower, upper)
        }
        _ => (vec![Value::Int(i64::MIN)], vec![Value::Int(i64::MAX)]),
    }
}

/// Pick the larger (more restrictive) lower bound.
fn pick_tighter_lower(a: &[Value], b: &[Value]) -> Vec<Value> {
    if a.len() == 1 && b.len() == 1 {
        if value_gt(&a[0], &b[0]) { a.to_vec() } else { b.to_vec() }
    } else {
        a.to_vec()
    }
}

/// Pick the smaller (more restrictive) upper bound.
fn pick_tighter_upper(a: &[Value], b: &[Value]) -> Vec<Value> {
    if a.len() == 1 && b.len() == 1 {
        if value_lt(&a[0], &b[0]) { a.to_vec() } else { b.to_vec() }
    } else {
        a.to_vec()
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
                FetchStrategy::FieldIndexRange(index_name) => {
                    props.push(("strategy", format!("FieldIndexRange ({})", index_name)));
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
        Plan::CreateAffinityGroup(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "CREATE AFFINITY GROUP".into()));
            props.push(("name", p.name.clone()));
            props.push(("tier", "Routing".into()));
        }
        Plan::AlterEntityDropAffinity(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "ALTER ENTITY DROP AFFINITY".into()));
            props.push(("entity_id", p.entity_id.clone()));
            props.push(("tier", "Routing".into()));
        }
        Plan::Demote(_) => {
            props.push(("operation", "Demote".into()));
            props.push(("tier", "Routing".into()));
        }
        Plan::Promote(_) => {
            props.push(("operation", "Promote".into()));
            props.push(("tier", "Routing".into()));
        }
        Plan::DeleteEntity(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "DELETE".into()));
            props.push(("collection", p.collection.clone()));
            props.push(("tier", "Fjall".into()));
        }
        Plan::ExplainTiers(_) => {
            props.push(("operation", "ExplainTiers".into()));
        }
        Plan::UpdateEntity(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "UPDATE".into()));
            props.push(("collection", p.collection.clone()));
            props.push(("tier", "Fjall".into()));
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
    use crate::planner::{CreateCollectionPlan, CreateEdgeTypePlan, DeleteEntityPlan, FetchPlan, FetchStrategy, InsertEdgePlan, InsertPlan, PreFilter, SearchPlan, TraversePlan};
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
        (Executor::new(store, wal, Arc::new(LocationTable::new()), Arc::new(VectoriserRegistry::new())), dir)
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
                fields: vec![],
            }],
            fields: vec![],
            indexes: vec![],
            vectoriser_config: None,
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
            collocate_with: None,
            affinity_group: None,
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
                fields: vec![],
                },
                trondb_tql::RepresentationDecl {
                    name: "identity".into(),
                    model: None,
                    dimensions: Some(3),
                    metric: trondb_tql::Metric::Cosine,
                    sparse: false,
                fields: vec![],
                },
            ],
            fields: vec![],
            indexes: vec![],
            vectoriser_config: None,
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
                fields: vec![],
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
            vectoriser_config: None,
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
                fields: vec![],
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
            vectoriser_config: None,
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
                fields: vec![],
                },
                trondb_tql::RepresentationDecl {
                    name: "sparse_repr".into(),
                    model: None,
                    dimensions: None,
                    metric: trondb_tql::Metric::Cosine,
                    sparse: true,
                fields: vec![],
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
            vectoriser_config: None,
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
            collocate_with: None,
            affinity_group: None,
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
                fields: vec![],
            }],
            fields: vec![],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

        // Insert with sparse vector
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "docs".into(),
            fields: vec!["id".into()],
            values: vec![Literal::String("d1".into())],
            vectors: vec![("keywords".to_string(), VectorLiteral::Sparse(vec![(1, 0.8), (42, 0.5)]))],
            collocate_with: None,
            affinity_group: None,
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
                fields: vec![],
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
            vectoriser_config: None,
        })).await.unwrap();

        // Insert with field values
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![Literal::String("v1".into()), Literal::String("London".into())],
            vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.1, 0.2, 0.3]))],
            collocate_with: None,
            affinity_group: None,
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
                fields: vec![],
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
            vectoriser_config: None,
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
            collocate_with: None,
            affinity_group: None,
        })).await.unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![Literal::String("v2".into()), Literal::String("Paris".into())],
            vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.4, 0.5, 0.6]))],
            collocate_with: None,
            affinity_group: None,
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
    async fn fetch_via_field_index_range() {
        let (exec, _dir) = setup_executor().await;

        // Create collection with INT score field + index
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "items".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
                fields: vec![],
            }],
            fields: vec![trondb_tql::FieldDecl {
                name: "score".into(),
                field_type: trondb_tql::FieldType::Int,
            }],
            indexes: vec![trondb_tql::IndexDecl {
                name: "idx_score".into(),
                fields: vec!["score".into()],
                partial_condition: None,
            }],
            vectoriser_config: None,
        })).await.unwrap();

        // Insert entities with scores 10, 20, 30, 40, 50
        for i in 1..=5 {
            let score = i * 10;
            exec.execute(&Plan::Insert(InsertPlan {
                collection: "items".into(),
                fields: vec!["id".into(), "score".into()],
                values: vec![
                    Literal::String(format!("item{}", i)),
                    Literal::Int(score),
                ],
                vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.1, 0.2, 0.3]))],
                collocate_with: None,
                affinity_group: None,
            })).await.unwrap();
        }

        // FETCH WHERE score >= 30 using FieldIndexRange strategy
        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "items".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Gte("score".into(), Literal::Int(30))),
            limit: None,
            strategy: FetchStrategy::FieldIndexRange("idx_score".into()),
        })).await.unwrap();

        assert_eq!(result.rows.len(), 3); // scores 30, 40, 50
        assert_eq!(result.stats.tier, "FieldIndex");
        assert_eq!(result.stats.mode, QueryMode::Deterministic);

        // Verify all returned scores are >= 30
        for row in &result.rows {
            let score = row.values.get("score").unwrap();
            match score {
                Value::Int(v) => assert!(*v >= 30, "score {} should be >= 30", v),
                _ => panic!("expected Int score"),
            }
        }
    }

    #[tokio::test]
    async fn fetch_via_field_index_range_strict_gt() {
        let (exec, _dir) = setup_executor().await;

        // Create collection with INT score field + index
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "items".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
                fields: vec![],
            }],
            fields: vec![trondb_tql::FieldDecl {
                name: "score".into(),
                field_type: trondb_tql::FieldType::Int,
            }],
            indexes: vec![trondb_tql::IndexDecl {
                name: "idx_score".into(),
                fields: vec!["score".into()],
                partial_condition: None,
            }],
            vectoriser_config: None,
        })).await.unwrap();

        // Insert entities with scores 10, 20, 30, 40, 50
        for i in 1..=5 {
            let score = i * 10;
            exec.execute(&Plan::Insert(InsertPlan {
                collection: "items".into(),
                fields: vec!["id".into(), "score".into()],
                values: vec![
                    Literal::String(format!("item{}", i)),
                    Literal::Int(score),
                ],
                vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.1, 0.2, 0.3]))],
                collocate_with: None,
                affinity_group: None,
            })).await.unwrap();
        }

        // FETCH WHERE score > 30 (strict) — should exclude score=30
        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "items".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Gt("score".into(), Literal::Int(30))),
            limit: None,
            strategy: FetchStrategy::FieldIndexRange("idx_score".into()),
        })).await.unwrap();

        assert_eq!(result.rows.len(), 2); // scores 40, 50 only
        for row in &result.rows {
            let score = row.values.get("score").unwrap();
            match score {
                Value::Int(v) => assert!(*v > 30, "score {} should be > 30", v),
                _ => panic!("expected Int score"),
            }
        }
    }

    #[tokio::test]
    async fn fetch_via_field_index_range_and_bounds() {
        let (exec, _dir) = setup_executor().await;

        // Create collection with INT score field + index
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "items".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
                fields: vec![],
            }],
            fields: vec![trondb_tql::FieldDecl {
                name: "score".into(),
                field_type: trondb_tql::FieldType::Int,
            }],
            indexes: vec![trondb_tql::IndexDecl {
                name: "idx_score".into(),
                fields: vec!["score".into()],
                partial_condition: None,
            }],
            vectoriser_config: None,
        })).await.unwrap();

        // Insert entities with scores 10, 20, 30, 40, 50
        for i in 1..=5 {
            let score = i * 10;
            exec.execute(&Plan::Insert(InsertPlan {
                collection: "items".into(),
                fields: vec!["id".into(), "score".into()],
                values: vec![
                    Literal::String(format!("item{}", i)),
                    Literal::Int(score),
                ],
                vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.1, 0.2, 0.3]))],
                collocate_with: None,
                affinity_group: None,
            })).await.unwrap();
        }

        // FETCH WHERE score >= 20 AND score <= 40
        let filter = WhereClause::And(
            Box::new(WhereClause::Gte("score".into(), Literal::Int(20))),
            Box::new(WhereClause::Lte("score".into(), Literal::Int(40))),
        );
        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "items".into(),
            fields: FieldList::All,
            filter: Some(filter),
            limit: None,
            strategy: FetchStrategy::FieldIndexRange("idx_score".into()),
        })).await.unwrap();

        assert_eq!(result.rows.len(), 3); // scores 20, 30, 40
        for row in &result.rows {
            let score = row.values.get("score").unwrap();
            match score {
                Value::Int(v) => assert!(*v >= 20 && *v <= 40, "score {} should be in [20,40]", v),
                _ => panic!("expected Int score"),
            }
        }
    }

    #[tokio::test]
    async fn fetch_lt_excludes_boundary() {
        let (exec, _dir) = setup_executor().await;

        // Create collection with INT score field + index
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "items".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
                fields: vec![],
            }],
            fields: vec![trondb_tql::FieldDecl {
                name: "score".into(),
                field_type: trondb_tql::FieldType::Int,
            }],
            indexes: vec![trondb_tql::IndexDecl {
                name: "idx_score".into(),
                fields: vec!["score".into()],
                partial_condition: None,
            }],
            vectoriser_config: None,
        })).await.unwrap();

        // Insert entities with scores 10, 20, 30, 40, 50
        for i in 1..=5 {
            let score = i * 10;
            exec.execute(&Plan::Insert(InsertPlan {
                collection: "items".into(),
                fields: vec!["id".into(), "score".into()],
                values: vec![
                    Literal::String(format!("item{}", i)),
                    Literal::Int(score),
                ],
                vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.1, 0.2, 0.3]))],
                collocate_with: None,
                affinity_group: None,
            })).await.unwrap();
        }

        // FETCH WHERE score < 30 (strict) — should exclude score=30
        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "items".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Lt("score".into(), Literal::Int(30))),
            limit: None,
            strategy: FetchStrategy::FieldIndexRange("idx_score".into()),
        })).await.unwrap();

        assert_eq!(result.rows.len(), 2); // scores 10, 20 only
        for row in &result.rows {
            let score = row.values.get("score").unwrap();
            match score {
                Value::Int(v) => assert!(*v < 30, "score {} should be < 30", v),
                _ => panic!("expected Int score"),
            }
        }
    }

    #[tokio::test]
    async fn fetch_lte_includes_boundary() {
        let (exec, _dir) = setup_executor().await;

        // Create collection with INT score field + index
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "items".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
                fields: vec![],
            }],
            fields: vec![trondb_tql::FieldDecl {
                name: "score".into(),
                field_type: trondb_tql::FieldType::Int,
            }],
            indexes: vec![trondb_tql::IndexDecl {
                name: "idx_score".into(),
                fields: vec!["score".into()],
                partial_condition: None,
            }],
            vectoriser_config: None,
        })).await.unwrap();

        // Insert entities with scores 10, 20, 30, 40, 50
        for i in 1..=5 {
            let score = i * 10;
            exec.execute(&Plan::Insert(InsertPlan {
                collection: "items".into(),
                fields: vec!["id".into(), "score".into()],
                values: vec![
                    Literal::String(format!("item{}", i)),
                    Literal::Int(score),
                ],
                vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.1, 0.2, 0.3]))],
                collocate_with: None,
                affinity_group: None,
            })).await.unwrap();
        }

        // FETCH WHERE score <= 30 — should include score=30
        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "items".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Lte("score".into(), Literal::Int(30))),
            limit: None,
            strategy: FetchStrategy::FieldIndexRange("idx_score".into()),
        })).await.unwrap();

        assert_eq!(result.rows.len(), 3); // scores 10, 20, 30
        for row in &result.rows {
            let score = row.values.get("score").unwrap();
            match score {
                Value::Int(v) => assert!(*v <= 30, "score {} should be <= 30", v),
                _ => panic!("expected Int score"),
            }
        }
    }

    #[tokio::test]
    async fn fetch_gte_lte_fullscan_fallback() {
        let (exec, _dir) = setup_executor().await;

        // Create collection WITHOUT an index on score
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "scores".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
                fields: vec![],
            }],
            fields: vec![trondb_tql::FieldDecl {
                name: "score".into(),
                field_type: trondb_tql::FieldType::Int,
            }],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

        // Insert entities with scores 10, 20, 30, 40, 50
        for i in 1..=5 {
            let score = i * 10;
            exec.execute(&Plan::Insert(InsertPlan {
                collection: "scores".into(),
                fields: vec!["id".into(), "score".into()],
                values: vec![
                    Literal::String(format!("item{}", i)),
                    Literal::Int(score),
                ],
                vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.1, 0.2, 0.3]))],
                collocate_with: None,
                affinity_group: None,
            })).await.unwrap();
        }

        // FETCH WHERE score >= 30 via FullScan (no index available)
        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "scores".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Gte("score".into(), Literal::Int(30))),
            limit: None,
            strategy: FetchStrategy::FullScan,
        })).await.unwrap();

        assert_eq!(result.rows.len(), 3); // scores 30, 40, 50
        assert_eq!(result.stats.tier, "Fjall"); // FullScan reads from Fjall tier
        for row in &result.rows {
            let score = row.values.get("score").unwrap();
            match score {
                Value::Int(v) => assert!(*v >= 30, "score {} should be >= 30", v),
                _ => panic!("expected Int score"),
            }
        }
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
                fields: vec![],
            }],
            fields: vec![],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

        // Insert entities with sparse vectors
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "docs".into(),
            fields: vec!["id".into()],
            values: vec![Literal::String("d1".into())],
            vectors: vec![("keywords".to_string(), VectorLiteral::Sparse(vec![(1, 0.8), (42, 0.5)]))],
            collocate_with: None,
            affinity_group: None,
        })).await.unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "docs".into(),
            fields: vec!["id".into()],
            values: vec![Literal::String("d2".into())],
            vectors: vec![("keywords".to_string(), VectorLiteral::Sparse(vec![(1, 0.3), (99, 0.9)]))],
            collocate_with: None,
            affinity_group: None,
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
                fields: vec![],
                },
                trondb_tql::RepresentationDecl {
                    name: "keywords".into(),
                    model: None,
                    dimensions: None,
                    metric: trondb_tql::Metric::Cosine,
                    sparse: true,
                fields: vec![],
                },
            ],
            fields: vec![],
            indexes: vec![],
            vectoriser_config: None,
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
            collocate_with: None,
            affinity_group: None,
        })).await.unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "docs".into(),
            fields: vec!["id".into()],
            values: vec![Literal::String("d2".into())],
            vectors: vec![
                ("embed".to_string(), VectorLiteral::Dense(vec![0.0, 1.0, 0.0])),
                ("keywords".to_string(), VectorLiteral::Sparse(vec![(2, 0.9)])),
            ],
            collocate_with: None,
            affinity_group: None,
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
                fields: vec![],
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
            vectoriser_config: None,
        })).await.unwrap();

        // Insert entities in different cities
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![Literal::String("v1".into()), Literal::String("London".into())],
            vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
            collocate_with: None,
            affinity_group: None,
        })).await.unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![Literal::String("v2".into()), Literal::String("Paris".into())],
            vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.9, 0.1, 0.0]))],
            collocate_with: None,
            affinity_group: None,
        })).await.unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![Literal::String("v3".into()), Literal::String("London".into())],
            vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.8, 0.2, 0.0]))],
            collocate_with: None,
            affinity_group: None,
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

    #[tokio::test]
    async fn traverse_multi_hop_depth_3() {
        let (exec, _dir) = setup_executor().await;

        // Create collection with a field
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "people".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
                fields: vec![],
            }],
            fields: vec![trondb_tql::FieldDecl {
                name: "name".into(),
                field_type: trondb_tql::FieldType::Text,
            }],
            indexes: vec![],
            vectoriser_config: None,
        }))
        .await
        .unwrap();

        // Insert 4 people: a, b, c, d
        for (id, name) in [("a", "Alice"), ("b", "Bob"), ("c", "Carol"), ("d", "Dave")] {
            insert_entity(
                &exec,
                "people",
                id,
                vec![("name", Literal::String(name.into()))],
                Some(vec![1.0, 0.0, 0.0]),
            )
            .await;
        }

        // Create edge type: knows (people → people)
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
        }))
        .await
        .unwrap();

        // Insert chain: a→b→c→d
        for (from, to) in [("a", "b"), ("b", "c"), ("c", "d")] {
            exec.execute(&Plan::InsertEdge(InsertEdgePlan {
                edge_type: "knows".into(),
                from_id: from.into(),
                to_id: to.into(),
                metadata: vec![],
            }))
            .await
            .unwrap();
        }

        // Traverse depth 3 from "a" — should reach b, c, d
        let result = exec
            .execute(&Plan::Traverse(TraversePlan {
                edge_type: "knows".into(),
                from_id: "a".into(),
                depth: 3,
                limit: None,
            }))
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 3); // b, c, d
    }

    #[tokio::test]
    async fn traverse_cycle_detection() {
        let (exec, _dir) = setup_executor().await;

        // Create collection with a "name" TEXT field
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "people".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
                fields: vec![],
            }],
            fields: vec![trondb_tql::FieldDecl {
                name: "name".into(),
                field_type: trondb_tql::FieldType::Text,
            }],
            indexes: vec![],
            vectoriser_config: None,
        }))
        .await
        .unwrap();

        // Insert 3 people: a, b, c
        for (id, name) in [("a", "Alice"), ("b", "Bob"), ("c", "Carol")] {
            insert_entity(
                &exec,
                "people",
                id,
                vec![("name", Literal::String(name.into()))],
                Some(vec![1.0, 0.0, 0.0]),
            )
            .await;
        }

        // Create edge type
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
        }))
        .await
        .unwrap();

        // Build cycle: a→b→c→a
        for (from, to) in [("a", "b"), ("b", "c"), ("c", "a")] {
            exec.execute(&Plan::InsertEdge(InsertEdgePlan {
                edge_type: "knows".into(),
                from_id: from.into(),
                to_id: to.into(),
                metadata: vec![],
            }))
            .await
            .unwrap();
        }

        // Traverse from a with DEPTH 10 — should not loop, start node excluded
        let result = exec
            .execute(&Plan::Traverse(TraversePlan {
                edge_type: "knows".into(),
                from_id: "a".into(),
                depth: 10,
                limit: None,
            }))
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 2); // b, c — no infinite loop, a excluded
    }

    #[tokio::test]
    async fn traverse_depth_1_single_hop() {
        let (exec, _dir) = setup_executor().await;

        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "people".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
                fields: vec![],
            }],
            fields: vec![trondb_tql::FieldDecl {
                name: "name".into(),
                field_type: trondb_tql::FieldType::Text,
            }],
            indexes: vec![],
            vectoriser_config: None,
        }))
        .await
        .unwrap();

        // Insert chain: a→b→c
        for (id, name) in [("a", "Alice"), ("b", "Bob"), ("c", "Carol")] {
            insert_entity(
                &exec,
                "people",
                id,
                vec![("name", Literal::String(name.into()))],
                Some(vec![1.0, 0.0, 0.0]),
            )
            .await;
        }

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
        }))
        .await
        .unwrap();

        for (from, to) in [("a", "b"), ("b", "c")] {
            exec.execute(&Plan::InsertEdge(InsertEdgePlan {
                edge_type: "knows".into(),
                from_id: from.into(),
                to_id: to.into(),
                metadata: vec![],
            }))
            .await
            .unwrap();
        }

        // Traverse from a with DEPTH 1 — should return only b
        let result = exec
            .execute(&Plan::Traverse(TraversePlan {
                edge_type: "knows".into(),
                from_id: "a".into(),
                depth: 1,
                limit: None,
            }))
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 1); // b only
    }

    #[tokio::test]
    async fn traverse_with_limit() {
        let (exec, _dir) = setup_executor().await;

        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "people".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
                fields: vec![],
            }],
            fields: vec![trondb_tql::FieldDecl {
                name: "name".into(),
                field_type: trondb_tql::FieldType::Text,
            }],
            indexes: vec![],
            vectoriser_config: None,
        }))
        .await
        .unwrap();

        // Fan-out: a→b, a→c, a→d
        for (id, name) in [("a", "Alice"), ("b", "Bob"), ("c", "Carol"), ("d", "Dave")] {
            insert_entity(
                &exec,
                "people",
                id,
                vec![("name", Literal::String(name.into()))],
                Some(vec![1.0, 0.0, 0.0]),
            )
            .await;
        }

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
        }))
        .await
        .unwrap();

        for to in ["b", "c", "d"] {
            exec.execute(&Plan::InsertEdge(InsertEdgePlan {
                edge_type: "knows".into(),
                from_id: "a".into(),
                to_id: to.into(),
                metadata: vec![],
            }))
            .await
            .unwrap();
        }

        // Traverse from a with DEPTH 1 LIMIT 2 — should return exactly 2
        let result = exec
            .execute(&Plan::Traverse(TraversePlan {
                edge_type: "knows".into(),
                from_id: "a".into(),
                depth: 1,
                limit: Some(2),
            }))
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 2);
    }

    #[tokio::test]
    async fn traverse_depth_exceeds_graph() {
        let (exec, _dir) = setup_executor().await;

        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "people".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
                fields: vec![],
            }],
            fields: vec![trondb_tql::FieldDecl {
                name: "name".into(),
                field_type: trondb_tql::FieldType::Text,
            }],
            indexes: vec![],
            vectoriser_config: None,
        }))
        .await
        .unwrap();

        // Only a→b, no further edges
        for (id, name) in [("a", "Alice"), ("b", "Bob")] {
            insert_entity(
                &exec,
                "people",
                id,
                vec![("name", Literal::String(name.into()))],
                Some(vec![1.0, 0.0, 0.0]),
            )
            .await;
        }

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
        }))
        .await
        .unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(),
            from_id: "a".into(),
            to_id: "b".into(),
            metadata: vec![],
        }))
        .await
        .unwrap();

        // Traverse from a with DEPTH 10 — should return just b, no error
        let result = exec
            .execute(&Plan::Traverse(TraversePlan {
                edge_type: "knows".into(),
                from_id: "a".into(),
                depth: 10,
                limit: None,
            }))
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 1); // just b
    }

    #[tokio::test]
    async fn delete_entity_cascading() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;

        // Insert two entities
        insert_entity(
            &exec,
            "venues",
            "v1",
            vec![("name", Literal::String("Venue A".into()))],
            Some(vec![1.0, 0.0, 0.0]),
        )
        .await;
        insert_entity(
            &exec,
            "venues",
            "v2",
            vec![("name", Literal::String("Venue B".into()))],
            Some(vec![0.0, 1.0, 0.0]),
        )
        .await;

        // Verify both exist
        let fetch = exec
            .execute(&Plan::Fetch(FetchPlan {
                collection: "venues".into(),
                fields: FieldList::All,
                filter: None,
                limit: None,
                strategy: FetchStrategy::FullScan,
            }))
            .await
            .unwrap();
        assert_eq!(fetch.rows.len(), 2);

        // Delete v1
        let result = exec
            .execute(&Plan::DeleteEntity(DeleteEntityPlan {
                entity_id: "v1".into(),
                collection: "venues".into(),
            }))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        match &result.rows[0].values["result"] {
            Value::String(s) => assert!(s.contains("deleted"), "expected 'deleted' in: {s}"),
            other => panic!("expected Value::String, got: {other:?}"),
        }

        // Verify only v2 remains
        let fetch = exec
            .execute(&Plan::Fetch(FetchPlan {
                collection: "venues".into(),
                fields: FieldList::All,
                filter: None,
                limit: None,
                strategy: FetchStrategy::FullScan,
            }))
            .await
            .unwrap();
        assert_eq!(fetch.rows.len(), 1);
        assert_eq!(
            fetch.rows[0].values["id"],
            Value::String("v2".into())
        );
    }

    #[tokio::test]
    async fn delete_entity_removes_edges() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "people", 3).await;

        insert_entity(&exec, "people", "p1", vec![], Some(vec![1.0, 0.0, 0.0])).await;
        insert_entity(&exec, "people", "p2", vec![], Some(vec![0.0, 1.0, 0.0])).await;
        insert_entity(&exec, "people", "p3", vec![], Some(vec![0.0, 0.0, 1.0])).await;

        // Create edge type and edges: p1 -> p2, p3 -> p1
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
        }))
        .await
        .unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(),
            from_id: "p1".into(),
            to_id: "p2".into(),
            metadata: vec![],
        }))
        .await
        .unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(),
            from_id: "p3".into(),
            to_id: "p1".into(),
            metadata: vec![],
        }))
        .await
        .unwrap();

        // Delete p1 — should cascade-remove both edges
        exec.execute(&Plan::DeleteEntity(DeleteEntityPlan {
            entity_id: "p1".into(),
            collection: "people".into(),
        }))
        .await
        .unwrap();

        // Traverse from p3 should find no outgoing edges (p3 -> p1 was removed)
        let result = exec
            .execute(&Plan::Traverse(TraversePlan {
                edge_type: "knows".into(),
                from_id: "p3".into(),
                depth: 1,
                limit: None,
            }))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 0);
    }

    #[tokio::test]
    async fn delete_nonexistent_entity_errors() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;

        let result = exec
            .execute(&Plan::DeleteEntity(DeleteEntityPlan {
                entity_id: "nonexistent".into(),
                collection: "venues".into(),
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn delete_entity_removes_from_hnsw() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;

        // Insert two entities with dense vectors
        insert_entity(
            &exec,
            "venues",
            "v1",
            vec![("name", Literal::String("Alpha".into()))],
            Some(vec![1.0, 0.0, 0.0]),
        )
        .await;
        insert_entity(
            &exec,
            "venues",
            "v2",
            vec![("name", Literal::String("Beta".into()))],
            Some(vec![0.9, 0.1, 0.0]),
        )
        .await;

        // SEARCH should return both (query near v1)
        let result = exec
            .execute(&Plan::Search(SearchPlan {
                collection: "venues".into(),
                fields: FieldList::All,
                dense_vector: Some(vec![1.0, 0.0, 0.0]),
                sparse_vector: None,
                filter: None,
                pre_filter: None,
                k: 10,
                confidence_threshold: 0.0,
                strategy: SearchStrategy::Hnsw,
            }))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 2);

        // Delete v1
        exec.execute(&Plan::DeleteEntity(DeleteEntityPlan {
            entity_id: "v1".into(),
            collection: "venues".into(),
        }))
        .await
        .unwrap();

        // SEARCH should now return only v2
        let result = exec
            .execute(&Plan::Search(SearchPlan {
                collection: "venues".into(),
                fields: FieldList::All,
                dense_vector: Some(vec![1.0, 0.0, 0.0]),
                sparse_vector: None,
                filter: None,
                pre_filter: None,
                k: 10,
                confidence_threshold: 0.0,
                strategy: SearchStrategy::Hnsw,
            }))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("id"),
            Some(&Value::String("v2".into()))
        );
    }

    #[tokio::test]
    async fn delete_entity_removes_from_field_index() {
        let (exec, _dir) = setup_executor().await;
        create_collection_with_field_index(&exec, "venues", 3).await;

        // Insert two entities with city field
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![
                Literal::String("v1".into()),
                Literal::String("London".into()),
            ],
            vectors: vec![(
                "default".to_string(),
                VectorLiteral::Dense(vec![1.0, 0.0, 0.0]),
            )],
            collocate_with: None,
            affinity_group: None,
        }))
        .await
        .unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![
                Literal::String("v2".into()),
                Literal::String("London".into()),
            ],
            vectors: vec![(
                "default".to_string(),
                VectorLiteral::Dense(vec![0.0, 1.0, 0.0]),
            )],
            collocate_with: None,
            affinity_group: None,
        }))
        .await
        .unwrap();

        // FETCH via field index should find both London entities
        let result = exec
            .execute(&Plan::Fetch(FetchPlan {
                collection: "venues".into(),
                fields: FieldList::All,
                filter: Some(WhereClause::Eq(
                    "city".into(),
                    Literal::String("London".into()),
                )),
                limit: None,
                strategy: FetchStrategy::FieldIndexLookup("idx_city".into()),
            }))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 2);

        // Delete v1
        exec.execute(&Plan::DeleteEntity(DeleteEntityPlan {
            entity_id: "v1".into(),
            collection: "venues".into(),
        }))
        .await
        .unwrap();

        // FETCH via field index should now find only v2
        let result = exec
            .execute(&Plan::Fetch(FetchPlan {
                collection: "venues".into(),
                fields: FieldList::All,
                filter: Some(WhereClause::Eq(
                    "city".into(),
                    Literal::String("London".into()),
                )),
                limit: None,
                strategy: FetchStrategy::FieldIndexLookup("idx_city".into()),
            }))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("id"),
            Some(&Value::String("v2".into()))
        );
    }
}
