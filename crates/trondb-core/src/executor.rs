use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use dashmap::{DashMap, DashSet};

use crate::edge::{AdjacencyIndex, Edge, EdgeSource, EdgeType};
use crate::error::EngineError;
use crate::field_index::FieldIndex;
use crate::index::HnswIndex;
use crate::inference::{InferenceAuditBuffer, InferenceAuditEntry, InferenceCandidate, InferenceTrigger};
use crate::location::{
    Encoding, LocState, LocationDescriptor, LocationTable, NodeAddress, ReprKey, Tier,
};
use crate::cost::{ConstantCostProvider, CostProvider};
use crate::planner::{FetchStrategy, Plan, SearchStrategy};
use crate::result::{QueryMode, QueryResult, QueryStats, Row};
use crate::sparse_index::SparseIndex;
use crate::store::FjallStore;
use crate::types::{
    CollectionSchema, Entity, FieldType, LogicalId, Metric, ReprState, ReprType, Representation,
    StoredField, StoredIndex, StoredRepresentation, Value, VectorData,
};
use crate::vectoriser::{FieldSet, VectoriserRegistry};
use trondb_tql::{FieldList, JoinFieldList, JoinType, Literal, VectorLiteral, WhereClause};
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
    /// Ring buffer of inference audit entries for EXPLAIN HISTORY
    inference_audit: Arc<InferenceAuditBuffer>,
    /// RAM-only work queue of entity IDs pending background inference
    inference_queue: Arc<DashSet<LogicalId>>,
    /// Cost provider for ACU estimation
    cost_provider: Arc<dyn CostProvider>,
}

impl Executor {
    pub fn new(
        store: FjallStore,
        wal: WalWriter,
        location: Arc<LocationTable>,
        vectoriser_registry: Arc<VectoriserRegistry>,
        inference_audit: Arc<InferenceAuditBuffer>,
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
            inference_audit,
            inference_queue: Arc::new(DashSet::new()),
            cost_provider: Arc::new(ConstantCostProvider),
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
                RecordType::EdgeConfirm | RecordType::EdgeConfidenceUpdate | RecordType::EdgeInferred => {
                    // EdgeConfirm, EdgeConfidenceUpdate, and EdgeInferred all store a full Edge payload
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
                RecordType::SchemaDropColl => {
                    let coll_name: String = rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                    if self.store.has_collection(&coll_name) {
                        self.store.drop_collection(&coll_name)?;
                        replayed += 1;
                    }
                    self.schemas.remove(&coll_name);
                    // Clean up HNSW indexes, sparse indexes, field indexes
                    let prefix = format!("{coll_name}:");
                    self.indexes.retain(|k, _| !k.starts_with(&prefix));
                    self.sparse_indexes.retain(|k, _| !k.starts_with(&prefix));
                    self.field_indexes.retain(|k, _| !k.starts_with(&prefix));
                    // Clean up entity_collections + location entries
                    self.entity_collections.retain(|entity_id, coll| {
                        if coll.as_str() == coll_name {
                            self.location.remove_entity(entity_id);
                            false
                        } else {
                            true
                        }
                    });
                }
                RecordType::SchemaDropEdgeType => {
                    let et_name: String = rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                    if self.store.has_edge_type(&et_name) {
                        self.store.drop_edge_type(&et_name)?;
                        replayed += 1;
                    }
                    self.edge_types.remove(&et_name);
                    self.adjacency.remove_edge_type(&et_name);
                }
                RecordType::ReprDirty => {
                    // Payload is a serialized ReprKey (entity_id + repr_index)
                    let repr_key: ReprKey = rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                    if self.location.get(&repr_key).is_some() {
                        // Best-effort: transition may fail if already Dirty
                        let _ = self.location.transition(&repr_key, LocState::Dirty);
                    }
                    replayed += 1;
                }
                RecordType::ReprWrite => {
                    // Payload is a full serialized Entity (same format as EntityWrite)
                    let entity: Entity = rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;

                    // Track entity → collection mapping (for recompute_dirty)
                    self.entity_collections.insert(entity.id.clone(), record.collection.clone());

                    // Write entity to Fjall (idempotent: WAL is authoritative)
                    if self.store.has_collection(&record.collection) {
                        self.store.insert(&record.collection, entity.clone())?;
                    }

                    // Transition affected repr location entries to Clean
                    for (idx, _repr) in entity.representations.iter().enumerate() {
                        let loc_key = ReprKey {
                            entity_id: entity.id.clone(),
                            repr_index: idx as u32,
                        };
                        if let Some(loc) = self.location.get(&loc_key) {
                            if loc.state == LocState::Dirty {
                                // Dirty → Recomputing → Clean
                                let _ = self.location.transition(&loc_key, LocState::Recomputing);
                                let _ = self.location.transition(&loc_key, LocState::Clean);
                            } else if loc.state == LocState::Recomputing {
                                // Recomputing → Clean
                                let _ = self.location.transition(&loc_key, LocState::Clean);
                            }
                        }
                    }
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

    /// Approximate collection size for cost estimation.
    /// Fast path: check HNSW index len (hot tier, O(1)).
    /// Slow path: Fjall scan count.
    fn collection_size(&self, collection: &str) -> usize {
        let prefix = format!("{collection}:");
        for entry in self.indexes.iter() {
            if entry.key().starts_with(&prefix) {
                return entry.value().len();
            }
        }
        self.store.scan(collection).map(|v| v.len()).unwrap_or(0)
    }

    pub async fn execute(&self, plan: &Plan) -> Result<QueryResult, EngineError> {
        let start = Instant::now();

        // Cost estimation + budget check (skip for EXPLAIN -- it reports cost, doesn't enforce)
        let cost_estimate = match plan {
            Plan::Explain(_) => None,
            _ => {
                let collection_name = plan_collection_name(plan);
                let size = collection_name
                    .map(|c| self.collection_size(c))
                    .unwrap_or(0);
                let est = crate::planner::estimate_plan_cost(plan, self.cost_provider.as_ref(), size);
                crate::planner::check_acu_budget(plan, &est)?;
                Some(est)
            }
        };

        // Run optimisation rules (warnings are collected but don't modify the plan in Phase 13)
        let opt_warnings = match plan {
            Plan::Explain(_) => vec![], // EXPLAIN handles its own
            _ => {
                let collection_name = plan_collection_name(plan);
                let size = collection_name
                    .map(|c| self.collection_size(c))
                    .unwrap_or(0);
                let optimised = crate::optimise::apply_rules(
                    plan.clone(),
                    &crate::optimise::OptimiserConfig::default(),
                    self.cost_provider.as_ref(),
                    size,
                );
                optimised.warnings
            }
        };

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
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
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
                        computed_at: 0,
                        model_version: String::new(),
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
                        computed_at: 0,
                        model_version: String::new(),
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

                // Enqueue for background inference if any edge type has auto inference
                for et in self.edge_types.iter() {
                    if et.value().inference_config.auto && et.value().from_collection == p.collection {
                        self.inference_queue.insert(entity_id.clone());
                        break;
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
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
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
                        sort_rows(&mut rows, &p.order_by);
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
                                cost: cost_estimate.clone(),
                                warnings: opt_warnings.clone(),
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
                        sort_rows(&mut rows, &p.order_by);
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
                                cost: cost_estimate.clone(),
                                warnings: opt_warnings.clone(),
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

                        sort_rows(&mut rows, &p.order_by);
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
                                cost: cost_estimate.clone(),
                                warnings: opt_warnings.clone(),
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
                        let mut raw = if let Some(ref tp) = p.two_pass {
                            // Two-pass: over-fetch in first pass, then rescore.
                            // Phase 13: both passes use the same HNSW index.
                            // Future: first pass uses Int8/Binary index, second pass rescores.
                            let first_pass = hnsw.search(&query_f32, tp.first_pass_k);
                            let mut rescored = first_pass;
                            rescored.truncate(p.k);
                            rescored
                        } else {
                            hnsw.search(&query_f32, fetch_k)
                        };
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
                    SearchStrategy::NaturalLanguage => {
                        let query_text = p.query_text.as_ref()
                            .ok_or_else(|| EngineError::InvalidQuery(
                                "NaturalLanguage strategy requires query_text".into(),
                            ))?;

                        // Determine which representation to use
                        let schema = self.schemas.get(&p.collection)
                            .ok_or_else(|| EngineError::CollectionNotFound(p.collection.clone()))?;

                        let repr_name = p.using_repr.as_deref()
                            .or_else(|| {
                                schema.representations.iter()
                                    .find(|r| !r.fields.is_empty() && !r.sparse)
                                    .map(|r| r.name.as_str())
                            })
                            .ok_or_else(|| EngineError::InvalidQuery(
                                "no managed dense representation found for natural language SEARCH".into(),
                            ))?
                            .to_string();

                        drop(schema); // release DashMap ref before async call

                        // Look up vectoriser
                        let vectoriser = self.vectoriser_registry
                            .get(&p.collection, &repr_name)
                            .ok_or_else(|| EngineError::InvalidQuery(format!(
                                "no vectoriser registered for {}:{}", p.collection, repr_name
                            )))?;

                        // Encode query text
                        let query_vector = vectoriser.encode_query(query_text).await
                            .map_err(|e| EngineError::Storage(format!("encode_query failed: {e}")))?;

                        let query_f32 = match query_vector {
                            VectorData::Dense(v) => v,
                            _ => return Err(EngineError::InvalidQuery(
                                "natural language SEARCH requires a dense vectoriser".into(),
                            )),
                        };

                        let hnsw_key = format!("{}:{}", p.collection, repr_name);
                        let hnsw = self.indexes.get(&hnsw_key)
                            .ok_or_else(|| EngineError::InvalidQuery(format!(
                                "no HNSW index for {}", hnsw_key
                            )))?;

                        let mut raw = if let Some(ref tp) = p.two_pass {
                            let first_pass = hnsw.search(&query_f32, tp.first_pass_k);
                            let mut rescored = first_pass;
                            rescored.truncate(p.k);
                            rescored
                        } else {
                            hnsw.search(&query_f32, fetch_k)
                        };
                        if p.confidence_threshold > 0.0 {
                            raw.retain(|(_, score)| (*score as f64) >= p.confidence_threshold);
                        }
                        raw
                    }
                };

                // Step 3b: Exclude dirty/recomputing representations from results
                let searched_repr_idx: Option<u32> = self.schemas.get(&p.collection)
                    .and_then(|schema| {
                        // Determine which repr_name was searched based on the strategy
                        let prefix = format!("{}:", p.collection);
                        let repr_name = match &p.strategy {
                            SearchStrategy::Hnsw | SearchStrategy::Hybrid | SearchStrategy::NaturalLanguage => {
                                self.indexes.iter()
                                    .find(|e| e.key().starts_with(&prefix))
                                    .map(|e| e.key().strip_prefix(&prefix).unwrap_or("").to_string())
                            }
                            SearchStrategy::Sparse => {
                                self.sparse_indexes.iter()
                                    .find(|e| e.key().starts_with(&prefix))
                                    .map(|e| e.key().strip_prefix(&prefix).unwrap_or("").to_string())
                            }
                        };
                        repr_name.and_then(|name| {
                            schema.representations.iter()
                                .position(|r| r.name == name)
                                .map(|idx| idx as u32)
                        })
                    });

                let results: Vec<(LogicalId, f32)> = if let Some(repr_idx) = searched_repr_idx {
                    results.into_iter().filter(|(entity_id, _score)| {
                        let repr_key = ReprKey {
                            entity_id: entity_id.clone(),
                            repr_index: repr_idx,
                        };
                        if let Some(loc) = self.location.get(&repr_key) {
                            if loc.state == LocState::Dirty || loc.state == LocState::Recomputing {
                                return false;
                            }
                        }
                        true
                    }).collect()
                } else {
                    results
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
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
                    },
                })
            }

            Plan::Explain(inner) => {
                let collection_name = plan_collection_name(inner);
                let size = collection_name
                    .map(|c| self.collection_size(c))
                    .unwrap_or(0);
                let est = crate::planner::estimate_plan_cost(inner, self.cost_provider.as_ref(), size);

                let mut rows = explain_plan(inner);

                // Add cost breakdown rows
                rows.push(Row {
                    values: HashMap::from([
                        ("property".into(), Value::String("estimated_acu".into())),
                        ("value".into(), Value::String(format!("{:.1}", est.total_acu))),
                    ]),
                    score: None,
                });
                let breakdown_str = est.items.iter()
                    .map(|item| format!("{}x {} @ {:.1} = {:.1}", item.count, item.operation, item.unit_cost, item.total))
                    .collect::<Vec<_>>()
                    .join("; ");
                rows.push(Row {
                    values: HashMap::from([
                        ("property".into(), Value::String("cost_breakdown".into())),
                        ("value".into(), Value::String(breakdown_str)),
                    ]),
                    score: None,
                });

                // Run optimisation rules
                let optimised = crate::optimise::apply_rules(
                    (**inner).clone(),
                    &crate::optimise::OptimiserConfig::default(),
                    self.cost_provider.as_ref(),
                    size,
                );

                // Add rules_applied row
                if !optimised.rules_applied.is_empty() {
                    rows.push(Row {
                        values: HashMap::from([
                            ("property".into(), Value::String("rules_applied".into())),
                            ("value".into(), Value::String(optimised.rules_applied.join(", "))),
                        ]),
                        score: None,
                    });
                }

                // Add warnings rows
                if !optimised.warnings.is_empty() {
                    let warnings_str = optimised.warnings.iter()
                        .map(|w| format!("{}", w))
                        .collect::<Vec<_>>()
                        .join("; ");
                    rows.push(Row {
                        values: HashMap::from([
                            ("property".into(), Value::String("warnings".into())),
                            ("value".into(), Value::String(warnings_str)),
                        ]),
                        score: None,
                    });
                }

                Ok(QueryResult {
                    columns: vec!["property".into(), "value".into()],
                    rows,
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: 0,
                        mode: QueryMode::Deterministic,
                        tier: "Fjall".into(),
                        cost: Some(est),
                        warnings: vec![],
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
                    inference_config: p.inference_config.clone().unwrap_or_default(),
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
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
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
                    source: EdgeSource::Structural,
                    valid_from: None,
                    valid_to: None,
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
                self.adjacency.insert(&from_id, &p.edge_type, &to_id, 1.0, now_millis, EdgeSource::Structural, None, None);

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
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
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
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
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
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
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
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
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
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
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
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
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
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
                    },
                })
            }
            // -----------------------------------------------------------------
            // INFER — read-only HNSW similarity search for edge proposals
            // -----------------------------------------------------------------
            Plan::Infer(p) => {
                let source_id = LogicalId::from_string(&p.from_id);

                // Step 1: Find the source entity's collection
                let source_collection = self.entity_collections.get(&source_id)
                    .map(|v| v.clone())
                    .ok_or_else(|| EngineError::InvalidQuery(
                        format!("entity '{}' not found in any collection", p.from_id),
                    ))?;

                // Step 2: Retrieve the source entity to get its vector
                let source_entity = self.store.get(&source_collection, &source_id)?;

                // Extract the first dense vector from the source entity's representations
                let source_vector = source_entity.representations.iter()
                    .find_map(|repr| {
                        if let VectorData::Dense(ref v) = repr.vector {
                            Some(v.clone())
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| EngineError::InvalidQuery(
                        format!("entity '{}' has no dense vector representation", p.from_id),
                    ))?;

                // Step 3: Resolve applicable edge types
                let applicable_edge_types: Vec<EdgeType> = if p.edge_types.is_empty() {
                    // Find all edge types where from_collection matches source entity's collection
                    self.edge_types.iter()
                        .filter(|et| et.from_collection == source_collection)
                        .map(|et| et.clone())
                        .collect()
                } else {
                    // Look up specified edge types
                    let mut types = Vec::new();
                    for et_name in &p.edge_types {
                        let et = self.edge_types.get(et_name)
                            .ok_or_else(|| EngineError::InvalidQuery(
                                format!("edge type '{}' not found", et_name),
                            ))?;
                        if et.from_collection != source_collection {
                            return Err(EngineError::InvalidQuery(
                                format!(
                                    "edge type '{}' from_collection '{}' does not match entity collection '{}'",
                                    et_name, et.from_collection, source_collection,
                                ),
                            ));
                        }
                        types.push(et.clone());
                    }
                    types
                };

                if applicable_edge_types.is_empty() {
                    return Ok(QueryResult {
                        columns: vec![
                            "from_id".into(), "to_id".into(),
                            "edge_type".into(), "confidence".into(),
                        ],
                        rows: vec![],
                        stats: QueryStats {
                            elapsed: start.elapsed(),
                            entities_scanned: 0,
                            mode: QueryMode::Probabilistic,
                            tier: "Ram".into(),
                            cost: cost_estimate.clone(),
                            warnings: opt_warnings.clone(),
                        },
                    });
                }

                // Step 4: For each edge type, HNSW search in target collection
                let default_limit = 10usize;
                let global_limit = p.limit.unwrap_or(default_limit);
                let default_floor = 0.0f32;
                let confidence_floor = p.confidence_floor.unwrap_or(default_floor);

                let mut all_candidates: Vec<(String, LogicalId, f32)> = Vec::new(); // (edge_type, to_id, score)
                let mut total_candidates_evaluated: usize = 0;
                let mut total_candidates_above_threshold: usize = 0;

                for et in &applicable_edge_types {
                    let target_collection = &et.to_collection;

                    // Find an HNSW index for the target collection
                    let prefix = format!("{}:", target_collection);
                    let hnsw_key = self.indexes.iter()
                        .find(|e| e.key().starts_with(&prefix))
                        .map(|e| e.key().clone());

                    let hnsw_key = match hnsw_key {
                        Some(k) => k,
                        None => continue, // No HNSW index for target collection — skip
                    };

                    let hnsw = match self.indexes.get(&hnsw_key) {
                        Some(h) => h,
                        None => continue,
                    };

                    // Search using source entity's vector — over-fetch to account for filtering
                    let search_k = global_limit * 4;
                    let raw_results = hnsw.search(&source_vector, search_k);

                    // Step 5: Filter out entities that already have an edge of this type from the source
                    let existing_edges = self.adjacency.get(&source_id, &et.name);
                    let existing_targets: HashSet<LogicalId> = existing_edges
                        .iter()
                        .map(|e| e.to_id.clone())
                        .collect();

                    for (candidate_id, score) in raw_results {
                        // Skip self-references
                        if candidate_id == source_id {
                            continue;
                        }
                        // Skip entities that already have this edge type from source
                        if existing_targets.contains(&candidate_id) {
                            continue;
                        }
                        total_candidates_evaluated += 1;
                        // Step 6: Apply confidence floor filter
                        if score < confidence_floor {
                            continue;
                        }
                        total_candidates_above_threshold += 1;
                        all_candidates.push((et.name.clone(), candidate_id, score));
                    }
                }

                // Step 7: Sort by score descending and apply limit
                all_candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
                all_candidates.truncate(global_limit);

                // Record inference audit entry
                self.inference_audit.record(InferenceAuditEntry {
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                    source_entity: source_id.clone(),
                    edge_type: "all".to_string(),
                    candidates_evaluated: total_candidates_evaluated,
                    candidates_above_threshold: total_candidates_above_threshold,
                    top_candidates: all_candidates.iter().map(|(_edge_type, to_id, score)| {
                        InferenceCandidate {
                            entity_id: to_id.clone(),
                            similarity_score: *score,
                            accepted: true,
                        }
                    }).collect(),
                    trigger: InferenceTrigger::Explicit,
                });

                // Step 8: Build QueryResult rows
                let mut rows = Vec::new();
                for (edge_type_name, to_id, score) in &all_candidates {
                    let mut values = HashMap::new();
                    values.insert("from_id".into(), Value::String(p.from_id.clone()));
                    values.insert("to_id".into(), Value::String(to_id.to_string()));
                    values.insert("edge_type".into(), Value::String(edge_type_name.clone()));
                    values.insert("confidence".into(), Value::Float(*score as f64));
                    rows.push(Row {
                        values,
                        score: Some(*score),
                    });
                }

                Ok(QueryResult {
                    columns: vec![
                        "from_id".into(), "to_id".into(),
                        "edge_type".into(), "confidence".into(),
                    ],
                    rows,
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: all_candidates.len(),
                        mode: QueryMode::Probabilistic,
                        tier: "Ram".into(),
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
                    },
                })
            }
            Plan::ConfirmEdge(p) => {
                // Step 1: Validate edge type exists
                let _edge_type = self.store.get_edge_type(&p.edge_type)?;

                let from_id = LogicalId::from_string(&p.from_id);
                let to_id = LogicalId::from_string(&p.to_id);

                // Step 2: Look up existing edge in Fjall
                let existing = self.store.get_edge(&p.edge_type, &p.from_id, &p.to_id)?;

                let now_millis = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;

                match existing {
                    // Structural edges cannot be confirmed — they are already permanent at 1.0
                    Some(ref edge) if edge.source == EdgeSource::Structural => {
                        return Err(EngineError::InvalidQuery(
                            format!(
                                "cannot CONFIRM structural edge '{}' {} -> {} (already permanent at confidence 1.0)",
                                p.edge_type, p.from_id, p.to_id,
                            ),
                        ));
                    }

                    // Inferred → Confirmed: delete old, create new with Confirmed source
                    Some(ref _edge) if _edge.source == EdgeSource::Inferred => {
                        let new_edge = Edge {
                            from_id: from_id.clone(),
                            to_id: to_id.clone(),
                            edge_type: p.edge_type.clone(),
                            confidence: p.confidence,
                            metadata: _edge.metadata.clone(),
                            created_at: now_millis,
                            source: EdgeSource::Confirmed,
                            valid_from: None,
                            valid_to: None,
                        };

                        // WAL: TxBegin -> EdgeConfirm -> commit
                        let tx_id = self.wal.next_tx_id();
                        self.wal.append(RecordType::TxBegin, &p.edge_type, tx_id, 1, vec![]);
                        let payload = rmp_serde::to_vec_named(&new_edge)
                            .map_err(|e| EngineError::Storage(e.to_string()))?;
                        self.wal.append(RecordType::EdgeConfirm, &p.edge_type, tx_id, 1, payload);
                        self.wal.commit(tx_id).await?;

                        // Apply to Fjall (insert_edge overwrites the same key)
                        self.store.insert_edge(&new_edge)?;
                        self.store.persist()?;

                        // Update AdjacencyIndex: remove old entry, insert new
                        self.adjacency.remove(&from_id, &p.edge_type, &to_id);
                        self.adjacency.insert(&from_id, &p.edge_type, &to_id, p.confidence, now_millis, EdgeSource::Confirmed, None, None);

                        Ok(QueryResult {
                            columns: vec!["result".into()],
                            rows: vec![Row {
                                values: HashMap::from([(
                                    "result".into(),
                                    Value::String(format!(
                                        "Edge '{}' confirmed: {} -> {} (was Inferred, now Confirmed at {:.2})",
                                        p.edge_type, p.from_id, p.to_id, p.confidence,
                                    )),
                                )]),
                                score: None,
                            }],
                            stats: QueryStats {
                                elapsed: start.elapsed(),
                                entities_scanned: 0,
                                mode: QueryMode::Deterministic,
                                tier: "Fjall".into(),
                                cost: cost_estimate.clone(),
                                warnings: opt_warnings.clone(),
                            },
                        })
                    }

                    // Confirmed → re-confirm: update confidence (idempotent)
                    Some(ref _edge) if _edge.source == EdgeSource::Confirmed => {
                        let updated_edge = Edge {
                            from_id: from_id.clone(),
                            to_id: to_id.clone(),
                            edge_type: p.edge_type.clone(),
                            confidence: p.confidence,
                            metadata: _edge.metadata.clone(),
                            created_at: _edge.created_at,
                            source: EdgeSource::Confirmed,
                            valid_from: None,
                            valid_to: None,
                        };

                        // WAL: TxBegin -> EdgeConfidenceUpdate -> commit
                        let tx_id = self.wal.next_tx_id();
                        self.wal.append(RecordType::TxBegin, &p.edge_type, tx_id, 1, vec![]);
                        let payload = rmp_serde::to_vec_named(&updated_edge)
                            .map_err(|e| EngineError::Storage(e.to_string()))?;
                        self.wal.append(RecordType::EdgeConfidenceUpdate, &p.edge_type, tx_id, 1, payload);
                        self.wal.commit(tx_id).await?;

                        // Apply to Fjall
                        self.store.insert_edge(&updated_edge)?;
                        self.store.persist()?;

                        // Update AdjacencyIndex
                        self.adjacency.remove(&from_id, &p.edge_type, &to_id);
                        self.adjacency.insert(&from_id, &p.edge_type, &to_id, p.confidence, _edge.created_at, EdgeSource::Confirmed, None, None);

                        Ok(QueryResult {
                            columns: vec!["result".into()],
                            rows: vec![Row {
                                values: HashMap::from([(
                                    "result".into(),
                                    Value::String(format!(
                                        "Edge '{}' re-confirmed: {} -> {} (confidence updated to {:.2})",
                                        p.edge_type, p.from_id, p.to_id, p.confidence,
                                    )),
                                )]),
                                score: None,
                            }],
                            stats: QueryStats {
                                elapsed: start.elapsed(),
                                entities_scanned: 0,
                                mode: QueryMode::Deterministic,
                                tier: "Fjall".into(),
                                cost: cost_estimate.clone(),
                                warnings: opt_warnings.clone(),
                            },
                        })
                    }

                    // Some edge with unknown source — treat as error (shouldn't happen)
                    Some(_) => {
                        return Err(EngineError::InvalidQuery(
                            format!("edge '{}' {} -> {} has unexpected source", p.edge_type, p.from_id, p.to_id),
                        ));
                    }

                    // No existing edge → create new Confirmed edge
                    None => {
                        let new_edge = Edge {
                            from_id: from_id.clone(),
                            to_id: to_id.clone(),
                            edge_type: p.edge_type.clone(),
                            confidence: p.confidence,
                            metadata: HashMap::new(),
                            created_at: now_millis,
                            source: EdgeSource::Confirmed,
                            valid_from: None,
                            valid_to: None,
                        };

                        // WAL: TxBegin -> EdgeConfirm -> commit
                        let tx_id = self.wal.next_tx_id();
                        self.wal.append(RecordType::TxBegin, &p.edge_type, tx_id, 1, vec![]);
                        let payload = rmp_serde::to_vec_named(&new_edge)
                            .map_err(|e| EngineError::Storage(e.to_string()))?;
                        self.wal.append(RecordType::EdgeConfirm, &p.edge_type, tx_id, 1, payload);
                        self.wal.commit(tx_id).await?;

                        // Apply to Fjall
                        self.store.insert_edge(&new_edge)?;
                        self.store.persist()?;

                        // Update AdjacencyIndex
                        self.adjacency.insert(&from_id, &p.edge_type, &to_id, p.confidence, now_millis, EdgeSource::Confirmed, None, None);

                        Ok(QueryResult {
                            columns: vec!["result".into()],
                            rows: vec![Row {
                                values: HashMap::from([(
                                    "result".into(),
                                    Value::String(format!(
                                        "Edge '{}' confirmed: {} -> {} (new Confirmed edge at {:.2})",
                                        p.edge_type, p.from_id, p.to_id, p.confidence,
                                    )),
                                )]),
                                score: None,
                            }],
                            stats: QueryStats {
                                elapsed: start.elapsed(),
                                entities_scanned: 0,
                                mode: QueryMode::Deterministic,
                                tier: "Fjall".into(),
                                cost: cost_estimate.clone(),
                                warnings: opt_warnings.clone(),
                            },
                        })
                    }
                }
            }
            Plan::ExplainHistory(p) => {
                let entity_id = LogicalId::from_string(&p.entity_id);
                let entries = self.inference_audit.query(&entity_id, p.limit);

                let rows: Vec<Row> = entries.iter().map(|e| {
                    let mut values = HashMap::new();
                    values.insert("timestamp".to_string(), Value::Int(e.timestamp as i64));
                    values.insert("entity_id".to_string(), Value::String(e.source_entity.to_string()));
                    values.insert("edge_type".to_string(), Value::String(e.edge_type.clone()));
                    values.insert("candidates_evaluated".to_string(), Value::Int(e.candidates_evaluated as i64));
                    values.insert("candidates_above_threshold".to_string(), Value::Int(e.candidates_above_threshold as i64));
                    values.insert("trigger".to_string(), Value::String(format!("{:?}", e.trigger)));
                    Row { values, score: None }
                }).collect();

                Ok(QueryResult {
                    columns: vec![
                        "timestamp".into(), "entity_id".into(), "edge_type".into(),
                        "candidates_evaluated".into(), "candidates_above_threshold".into(),
                        "trigger".into(),
                    ],
                    rows,
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: 0,
                        mode: QueryMode::Deterministic,
                        tier: "Ram".into(),
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
                    },
                })
            }

            Plan::DropCollection(p) => {
                // Step 1: Validate collection exists
                if !self.schemas.contains_key(&p.name) {
                    return Err(EngineError::CollectionNotFound(p.name.clone()));
                }

                // Step 2: WAL log
                let tx_id = self.wal.next_tx_id();
                self.wal.append(RecordType::TxBegin, &p.name, tx_id, 1, vec![]);
                let payload = rmp_serde::to_vec_named(&p.name)
                    .map_err(|e| EngineError::Storage(e.to_string()))?;
                self.wal.append(RecordType::SchemaDropColl, &p.name, tx_id, 1, payload);
                self.wal.commit(tx_id).await?;

                // Step 3: Remove HNSW indexes
                let hnsw_prefix = format!("{}:", p.name);
                self.indexes.retain(|k, _| !k.starts_with(&hnsw_prefix));

                // Step 4: Remove sparse indexes
                self.sparse_indexes.retain(|k, _| !k.starts_with(&hnsw_prefix));

                // Step 5: Remove field indexes
                self.field_indexes.retain(|k, _| !k.starts_with(&hnsw_prefix));

                // Step 6: Remove location table entries for entities in this collection
                self.entity_collections.retain(|entity_id, coll| {
                    if coll.as_str() == p.name {
                        self.location.remove_entity(entity_id);
                        false
                    } else {
                        true
                    }
                });

                // Step 7: Drop from Fjall store
                self.store.drop_collection(&p.name)?;

                // Step 8: Remove from schemas
                self.schemas.remove(&p.name);

                Ok(QueryResult {
                    columns: vec!["result".into()],
                    rows: vec![Row {
                        values: HashMap::from([(
                            "result".into(),
                            Value::String(format!("Collection '{}' dropped", p.name)),
                        )]),
                        score: None,
                    }],
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: 0,
                        mode: QueryMode::Deterministic,
                        tier: "Fjall".into(),
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
                    },
                })
            }

            Plan::DropEdgeType(p) => {
                // Step 1: Validate edge type exists
                if !self.edge_types.contains_key(&p.name) {
                    return Err(EngineError::EdgeTypeNotFound(p.name.clone()));
                }

                // Step 2: WAL log
                let tx_id = self.wal.next_tx_id();
                self.wal.append(RecordType::TxBegin, &p.name, tx_id, 1, vec![]);
                let payload = rmp_serde::to_vec_named(&p.name)
                    .map_err(|e| EngineError::Storage(e.to_string()))?;
                self.wal.append(RecordType::SchemaDropEdgeType, &p.name, tx_id, 1, payload);
                self.wal.commit(tx_id).await?;

                // Step 3: Remove adjacency entries
                self.adjacency.remove_edge_type(&p.name);

                // Step 4: Drop from Fjall store
                self.store.drop_edge_type(&p.name)?;

                // Step 5: Remove from edge_types
                self.edge_types.remove(&p.name);

                Ok(QueryResult {
                    columns: vec!["result".into()],
                    rows: vec![Row {
                        values: HashMap::from([(
                            "result".into(),
                            Value::String(format!("Edge type '{}' dropped", p.name)),
                        )]),
                        score: None,
                    }],
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: 0,
                        mode: QueryMode::Deterministic,
                        tier: "Fjall".into(),
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
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

                // Enqueue for background inference if any edge type has auto inference
                for et in self.edge_types.iter() {
                    if et.value().inference_config.auto && et.value().from_collection == p.collection {
                        self.inference_queue.insert(entity_id.clone());
                        break;
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
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
                    },
                })
            }

            Plan::Join(p) => {
                // Build alias → collection mapping
                let mut alias_to_collection: HashMap<String, String> = HashMap::new();
                alias_to_collection.insert(p.from_alias.clone(), p.from_collection.clone());
                for join in &p.joins {
                    alias_to_collection.insert(join.alias.clone(), join.collection.clone());
                }

                // Fetch all entities from the left (FROM) collection
                let left_entities: Vec<Entity> = self.store
                    .scan(&p.from_collection)?;

                let mut rows = Vec::new();
                let limit = p.limit.unwrap_or(usize::MAX);

                for left_entity in &left_entities {
                    let mut combined_row: HashMap<String, Value> = HashMap::new();

                    // Add left entity fields with alias prefix
                    combined_row.insert(
                        format!("{}.id", p.from_alias),
                        Value::String(left_entity.id.to_string()),
                    );
                    for (k, v) in &left_entity.metadata {
                        combined_row.insert(format!("{}.{}", p.from_alias, k), v.clone());
                    }

                    let mut matched = true;

                    for join in &p.joins {
                        // Get the left-side join field value
                        let left_field_val = if join.on_left.field == "id" {
                            Some(Value::String(left_entity.id.to_string()))
                        } else {
                            left_entity.metadata.get(&join.on_left.field).cloned()
                        };

                        let right_entity = match &left_field_val {
                            Some(Value::String(ref id_str)) => {
                                if join.confidence_threshold.is_some() {
                                    // Probabilistic join: look up via AdjacencyIndex
                                    let source_id = LogicalId::from_string(id_str);
                                    let mut best_match: Option<(Entity, f32)> = None;
                                    let threshold = join.confidence_threshold.unwrap_or(0.0) as f32;

                                    for et_entry in self.edge_types.iter() {
                                        let et = et_entry.value();
                                        if et.from_collection == *alias_to_collection.get(&join.on_left.alias).unwrap_or(&String::new())
                                            && et.to_collection == join.collection
                                        {
                                            let entries = self.adjacency.get(&source_id, &et.name);
                                            for entry in &entries {
                                                if entry.confidence >= threshold {
                                                    if let Ok(right_ent) = self.store.get(&join.collection, &entry.to_id) {
                                                        if best_match.as_ref().map(|(_, c)| entry.confidence > *c).unwrap_or(true) {
                                                            best_match = Some((right_ent, entry.confidence));
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    best_match
                                } else {
                                    // Structural join: use the field value as a lookup key
                                    if join.on_right.field == "id" {
                                        // Direct ID lookup
                                        self.store.get(&join.collection, &LogicalId::from_string(id_str))
                                            .ok().map(|e| (e, 1.0f32))
                                    } else {
                                        // Need to scan right collection for matching field value
                                        let mut found = None;
                                        if let Ok(right_entities) = self.store.scan(&join.collection) {
                                            for right_ent in right_entities {
                                                let right_val = if join.on_right.field == "id" {
                                                    Some(Value::String(right_ent.id.to_string()))
                                                } else {
                                                    right_ent.metadata.get(&join.on_right.field).cloned()
                                                };
                                                if right_val.as_ref() == left_field_val.as_ref() {
                                                    found = Some((right_ent, 1.0f32));
                                                    break;
                                                }
                                            }
                                        }
                                        found
                                    }
                                }
                            }
                            _ => None,
                        };

                        match (right_entity, &join.join_type) {
                            (Some((right_ent, confidence)), _) => {
                                // Add right entity fields with alias prefix
                                combined_row.insert(
                                    format!("{}.id", join.alias),
                                    Value::String(right_ent.id.to_string()),
                                );
                                for (k, v) in &right_ent.metadata {
                                    combined_row.insert(format!("{}.{}", join.alias, k), v.clone());
                                }
                                // Add _edge.confidence if this is a probabilistic join
                                if join.confidence_threshold.is_some() {
                                    combined_row.insert(
                                        "_edge.confidence".into(),
                                        Value::Float(confidence as f64),
                                    );
                                }
                            }
                            (None, JoinType::Inner) => {
                                matched = false;
                                break;
                            }
                            (None, JoinType::Left) | (None, JoinType::Full) => {
                                // Left/Full: include left row with NULLs for right side
                                combined_row.insert(format!("{}.id", join.alias), Value::Null);
                            }
                            (None, JoinType::Right) => {
                                // Right join: skip this left entity
                                matched = false;
                                break;
                            }
                        }
                    }

                    if !matched {
                        continue;
                    }

                    // Apply WHERE filter on combined row
                    if let Some(ref filter) = p.filter {
                        if !join_row_matches(&combined_row, filter) {
                            continue;
                        }
                    }

                    // Project requested fields
                    let projected = project_join_row(&combined_row, &p.fields);
                    rows.push(projected);

                    if rows.len() >= limit {
                        break;
                    }
                }

                // For RIGHT and FULL joins, also scan unmatched right entities
                for join in &p.joins {
                    if matches!(join.join_type, JoinType::Right | JoinType::Full) {
                        if let Ok(right_entities) = self.store.scan(&join.collection) {
                            for right_ent in right_entities {
                                // Check if this right entity was already matched
                                let right_id_key = format!("{}.id", join.alias);
                                let right_id_str = right_ent.id.to_string();
                                let already_matched = rows.iter().any(|r| {
                                    r.values.get(&right_id_key)
                                        .map(|v| matches!(v, Value::String(s) if s == &right_id_str))
                                        .unwrap_or(false)
                                });
                                if already_matched {
                                    continue;
                                }

                                let mut combined_row: HashMap<String, Value> = HashMap::new();
                                // NULLs for left side
                                combined_row.insert(format!("{}.id", p.from_alias), Value::Null);
                                // Right side fields
                                combined_row.insert(
                                    format!("{}.id", join.alias),
                                    Value::String(right_ent.id.to_string()),
                                );
                                for (k, v) in &right_ent.metadata {
                                    combined_row.insert(format!("{}.{}", join.alias, k), v.clone());
                                }

                                if let Some(ref filter) = p.filter {
                                    if !join_row_matches(&combined_row, filter) {
                                        continue;
                                    }
                                }

                                let projected = project_join_row(&combined_row, &p.fields);
                                rows.push(projected);

                                if rows.len() >= limit {
                                    break;
                                }
                            }
                        }
                    }
                }

                let scanned = rows.len();

                Ok(QueryResult {
                    columns: build_join_columns(&rows, &p.fields),
                    rows,
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: scanned,
                        mode: QueryMode::Deterministic,
                        tier: "Fjall".into(),
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
                    },
                })
            }

            Plan::TraverseMatch(p) => {
                let max_depth = p.max_depth.min(10); // cap at 10
                let min_depth = p.min_depth;
                let confidence_threshold = p.confidence_threshold.unwrap_or(0.0) as f32;

                let from_id = LogicalId::from_string(&p.from_id);
                let mut visited: HashSet<LogicalId> = HashSet::new();
                visited.insert(from_id.clone());

                let mut frontier = vec![from_id];
                let mut rows = Vec::new();
                let limit = p.limit.unwrap_or(usize::MAX);

                // Optional edge type filter from pattern
                let edge_type_filter: Option<String> = p.pattern.edge.edge_type.clone();

                for hop in 0..max_depth {
                    if frontier.is_empty() || rows.len() >= limit {
                        break;
                    }

                    let mut next_frontier = Vec::new();
                    let include_at_depth = hop + 1 >= min_depth;

                    for node_id in &frontier {
                        // Collect neighbor entries based on direction
                        // (to_id, confidence, collection)
                        let mut neighbors: Vec<(LogicalId, f32, String)> = Vec::new();

                        // Get edge types to check: filtered or all
                        let edge_types_to_check: Vec<EdgeType> = if let Some(ref et_name) = edge_type_filter {
                            match self.store.get_edge_type(et_name) {
                                Ok(et) => vec![et],
                                Err(_) => vec![],
                            }
                        } else {
                            self.edge_types.iter().map(|e| e.value().clone()).collect()
                        };

                        for et in &edge_types_to_check {
                            match p.pattern.edge.direction {
                                trondb_tql::EdgeDirection::Forward => {
                                    let entries = self.adjacency.get(node_id, &et.name);
                                    for entry in &entries {
                                        let effective_conf = if entry.created_at > 0 {
                                            let now_ms = SystemTime::now()
                                                .duration_since(UNIX_EPOCH)
                                                .unwrap()
                                                .as_millis() as u64;
                                            let elapsed = now_ms.saturating_sub(entry.created_at);
                                            crate::edge::effective_confidence(entry.confidence, elapsed, &et.decay_config)
                                        } else {
                                            entry.confidence
                                        };
                                        if effective_conf >= confidence_threshold {
                                            neighbors.push((entry.to_id.clone(), effective_conf, et.to_collection.clone()));
                                        }
                                    }
                                }
                                trondb_tql::EdgeDirection::Backward => {
                                    let source_ids = self.adjacency.get_backward(node_id, &et.name);
                                    for source_id in &source_ids {
                                        let entries = self.adjacency.get(source_id, &et.name);
                                        for entry in &entries {
                                            if entry.to_id == *node_id {
                                                let effective_conf = if entry.created_at > 0 {
                                                    let now_ms = SystemTime::now()
                                                        .duration_since(UNIX_EPOCH)
                                                        .unwrap()
                                                        .as_millis() as u64;
                                                    let elapsed = now_ms.saturating_sub(entry.created_at);
                                                    crate::edge::effective_confidence(entry.confidence, elapsed, &et.decay_config)
                                                } else {
                                                    entry.confidence
                                                };
                                                if effective_conf >= confidence_threshold {
                                                    neighbors.push((source_id.clone(), effective_conf, et.from_collection.clone()));
                                                }
                                            }
                                        }
                                    }
                                }
                                trondb_tql::EdgeDirection::Undirected => {
                                    // Forward direction
                                    let entries = self.adjacency.get(node_id, &et.name);
                                    for entry in &entries {
                                        let effective_conf = if entry.created_at > 0 {
                                            let now_ms = SystemTime::now()
                                                .duration_since(UNIX_EPOCH)
                                                .unwrap()
                                                .as_millis() as u64;
                                            let elapsed = now_ms.saturating_sub(entry.created_at);
                                            crate::edge::effective_confidence(entry.confidence, elapsed, &et.decay_config)
                                        } else {
                                            entry.confidence
                                        };
                                        if effective_conf >= confidence_threshold {
                                            neighbors.push((entry.to_id.clone(), effective_conf, et.to_collection.clone()));
                                        }
                                    }
                                    // Backward direction
                                    let source_ids = self.adjacency.get_backward(node_id, &et.name);
                                    for source_id in &source_ids {
                                        let entries = self.adjacency.get(source_id, &et.name);
                                        for entry in &entries {
                                            if entry.to_id == *node_id {
                                                let effective_conf = if entry.created_at > 0 {
                                                    let now_ms = SystemTime::now()
                                                        .duration_since(UNIX_EPOCH)
                                                        .unwrap()
                                                        .as_millis() as u64;
                                                    let elapsed = now_ms.saturating_sub(entry.created_at);
                                                    crate::edge::effective_confidence(entry.confidence, elapsed, &et.decay_config)
                                                } else {
                                                    entry.confidence
                                                };
                                                if effective_conf >= confidence_threshold {
                                                    neighbors.push((source_id.clone(), effective_conf, et.from_collection.clone()));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        for (neighbor_id, confidence, collection) in &neighbors {
                            if visited.contains(neighbor_id) {
                                continue;
                            }
                            visited.insert(neighbor_id.clone());

                            if include_at_depth {
                                if let Ok(entity) = self.store.get(collection, neighbor_id) {
                                    let mut row = entity_to_row(&entity, &FieldList::All);
                                    // Add _edge.confidence and _depth metadata
                                    row.values.insert("_edge.confidence".into(), Value::Float(*confidence as f64));
                                    if let Some(ref et_name) = edge_type_filter {
                                        row.values.insert("_edge.type".into(), Value::String(et_name.clone()));
                                    }
                                    row.values.insert("_depth".into(), Value::Int((hop + 1) as i64));
                                    rows.push(row);
                                    if rows.len() >= limit {
                                        break;
                                    }
                                }
                            }
                            next_frontier.push(neighbor_id.clone());
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
                        cost: cost_estimate.clone(),
                        warnings: opt_warnings.clone(),
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

    pub fn inference_audit(&self) -> &InferenceAuditBuffer {
        &self.inference_audit
    }

    pub fn inference_queue(&self) -> &Arc<DashSet<LogicalId>> {
        &self.inference_queue
    }

    /// Drain the inference work queue and run background inference for each entity.
    /// For each queued entity, find edge types with `auto` inference from that entity's
    /// collection, HNSW search in the target collection, and create inferred edges.
    pub async fn drain_inference_queue(&self) -> Result<usize, EngineError> {
        let entity_ids: Vec<LogicalId> = self.inference_queue.iter().map(|r| r.key().clone()).collect();
        let mut processed = 0;

        for entity_id in &entity_ids {
            self.inference_queue.remove(entity_id);

            // Find entity's collection
            let collection = match self.entity_collections.get(entity_id) {
                Some(c) => c.value().clone(),
                None => continue,
            };

            // Find edge types with auto inference for this collection
            let auto_types: Vec<EdgeType> = self.edge_types.iter()
                .filter(|et| et.value().from_collection == collection && et.value().inference_config.auto)
                .map(|et| et.value().clone())
                .collect();

            for et in &auto_types {
                // Get source entity vector (same pattern as Plan::Infer)
                let source_entity = match self.store.get(&collection, entity_id) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let source_vector = match source_entity.representations.iter().find_map(|repr| {
                    if let VectorData::Dense(ref v) = repr.vector {
                        Some(v.clone())
                    } else {
                        None
                    }
                }) {
                    Some(v) => v,
                    None => continue,
                };

                // Find HNSW index for target collection
                let target_collection = &et.to_collection;
                let prefix = format!("{}:", target_collection);
                let hnsw_key = match self.indexes.iter()
                    .find(|e| e.key().starts_with(&prefix))
                    .map(|e| e.key().clone())
                {
                    Some(k) => k,
                    None => continue,
                };

                let hnsw = match self.indexes.get(&hnsw_key) {
                    Some(h) => h,
                    None => continue,
                };

                // Search using source entity's vector
                let search_k = et.inference_config.limit * 4;
                let raw_results = hnsw.search(&source_vector, search_k);

                // Existing edges for this source + edge type
                let existing_edges = self.adjacency.get(entity_id, &et.name);
                let existing_targets: HashSet<LogicalId> = existing_edges
                    .iter()
                    .map(|e| e.to_id.clone())
                    .collect();

                let mut created_count = 0;

                for (target_id, score) in raw_results {
                    if created_count >= et.inference_config.limit {
                        break;
                    }
                    if score < et.inference_config.confidence_floor {
                        continue;
                    }
                    if target_id == *entity_id {
                        continue; // skip self
                    }
                    if existing_targets.contains(&target_id) {
                        continue; // skip if edge already exists
                    }

                    let now_millis = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;

                    let edge = Edge {
                        from_id: entity_id.clone(),
                        to_id: target_id.clone(),
                        edge_type: et.name.clone(),
                        confidence: score,
                        metadata: HashMap::new(),
                        created_at: now_millis,
                        source: EdgeSource::Inferred,
                        valid_from: None,
                        valid_to: None,
                    };

                    // WAL write (EdgeInferred)
                    let tx_id = self.wal.next_tx_id();
                    self.wal.append(RecordType::TxBegin, &et.name, tx_id, 1, vec![]);
                    let payload = rmp_serde::to_vec_named(&edge)
                        .map_err(|e| EngineError::Storage(e.to_string()))?;
                    self.wal.append(RecordType::EdgeInferred, &et.name, tx_id, 1, payload);
                    self.wal.commit(tx_id).await?;

                    // Fjall write
                    self.store.insert_edge(&edge)?;

                    // AdjacencyIndex update
                    self.adjacency.insert(
                        &edge.from_id,
                        &et.name,
                        &edge.to_id,
                        edge.confidence,
                        edge.created_at,
                        EdgeSource::Inferred,
                        None,
                        None,
                    );

                    created_count += 1;
                }
            }
            processed += 1;
        }
        Ok(processed)
    }

    /// Prune Inferred edges whose effective confidence has decayed below the prune_threshold.
    /// Only edges with `EdgeSource::Inferred` are considered; Structural and Confirmed edges
    /// are always skipped. Returns the number of pruned edges.
    pub async fn sweep_decayed_edges(&self) -> Result<usize, EngineError> {
        let mut pruned = 0;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        for et_ref in self.edge_types.iter() {
            let et = et_ref.value();
            // Only process edge types with a prune threshold
            let prune_threshold = match et.decay_config.prune_threshold {
                Some(t) => t as f32,
                None => continue,
            };

            // Scan all edges of this type
            let edges = match self.store.scan_edges(&et.name) {
                Ok(edges) => edges,
                Err(_) => continue,
            };

            for edge in &edges {
                // Only prune Inferred edges
                if edge.source != EdgeSource::Inferred {
                    continue;
                }

                let elapsed = now_ms.saturating_sub(edge.created_at);
                let effective = crate::edge::effective_confidence(
                    edge.confidence,
                    elapsed,
                    &et.decay_config,
                );

                if effective < prune_threshold {
                    // WAL: TxBegin -> EdgeDelete -> commit
                    let tx_id = self.wal.next_tx_id();
                    self.wal.append(RecordType::TxBegin, &et.name, tx_id, 1, vec![]);

                    #[derive(serde::Serialize)]
                    struct EdgeDeletePayload<'a> {
                        edge_type: &'a str,
                        from_id: &'a str,
                        to_id: &'a str,
                    }
                    let payload = rmp_serde::to_vec_named(&EdgeDeletePayload {
                        edge_type: &et.name,
                        from_id: edge.from_id.as_str(),
                        to_id: edge.to_id.as_str(),
                    })
                    .map_err(|e| EngineError::Storage(e.to_string()))?;
                    self.wal.append(RecordType::EdgeDelete, &et.name, tx_id, 1, payload);
                    self.wal.commit(tx_id).await?;

                    // Fjall delete
                    self.store.delete_edge(&et.name, edge.from_id.as_str(), edge.to_id.as_str())?;

                    // AdjacencyIndex remove
                    self.adjacency.remove(&edge.from_id, &et.name, &edge.to_id);

                    pruned += 1;
                }
            }
        }
        Ok(pruned)
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
            computed_at: 0,
            model_version: String::new(),
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
        WhereClause::Neq(field, lit) => {
            let expected = literal_to_value(lit);
            if field == "id" {
                return Value::String(entity.id.to_string()) != expected;
            }
            entity
                .metadata
                .get(field)
                .map(|v| *v != expected)
                .unwrap_or(true) // missing field != value is true
        }
        WhereClause::Not(inner) => !entity_matches(entity, inner),
        WhereClause::IsNull(field) => {
            if field == "id" {
                return false; // id is never null
            }
            !entity.metadata.contains_key(field)
                || entity.metadata.get(field) == Some(&Value::Null)
        }
        WhereClause::IsNotNull(field) => {
            if field == "id" {
                return true;
            }
            entity
                .metadata
                .get(field)
                .map(|v| *v != Value::Null)
                .unwrap_or(false)
        }
        WhereClause::In(field, values) => {
            let entity_val = if field == "id" {
                Value::String(entity.id.to_string())
            } else {
                match entity.metadata.get(field) {
                    Some(v) => v.clone(),
                    None => return false,
                }
            };
            values.iter().any(|lit| literal_to_value(lit) == entity_val)
        }
        WhereClause::Like(field, pattern) => {
            let val = if field == "id" {
                entity.id.to_string()
            } else {
                match entity.metadata.get(field) {
                    Some(Value::String(s)) => s.clone(),
                    _ => return false,
                }
            };
            like_match(&val, pattern)
        }
    }
}

/// SQL LIKE pattern matching. `%` = any sequence, `_` = any single character.
fn like_match(value: &str, pattern: &str) -> bool {
    let v: Vec<char> = value.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    like_match_recursive(&v, 0, &p, 0)
}

fn like_match_recursive(value: &[char], vi: usize, pattern: &[char], pi: usize) -> bool {
    if pi == pattern.len() {
        return vi == value.len();
    }
    match pattern[pi] {
        '%' => {
            for i in vi..=value.len() {
                if like_match_recursive(value, i, pattern, pi + 1) {
                    return true;
                }
            }
            false
        }
        '_' => vi < value.len() && like_match_recursive(value, vi + 1, pattern, pi + 1),
        c => {
            vi < value.len()
                && value[vi] == c
                && like_match_recursive(value, vi + 1, pattern, pi + 1)
        }
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

// ---------------------------------------------------------------------------
// JOIN helpers
// ---------------------------------------------------------------------------

fn join_row_matches(combined: &HashMap<String, Value>, clause: &WhereClause) -> bool {
    // WHERE clauses in JOIN context use qualified field names (e.g., "e.type")
    // The combined row already has alias-qualified keys
    match clause {
        WhereClause::Eq(field, lit) => {
            let expected = literal_to_value(lit);
            combined.get(field).map(|v| *v == expected).unwrap_or(false)
        }
        WhereClause::Neq(field, lit) => {
            let expected = literal_to_value(lit);
            combined.get(field).map(|v| *v != expected).unwrap_or(true)
        }
        WhereClause::Gt(field, lit) => {
            let threshold = literal_to_value(lit);
            combined.get(field).map(|v| value_gt(v, &threshold)).unwrap_or(false)
        }
        WhereClause::Lt(field, lit) => {
            let threshold = literal_to_value(lit);
            combined.get(field).map(|v| value_lt(v, &threshold)).unwrap_or(false)
        }
        WhereClause::Gte(field, lit) => {
            let threshold = literal_to_value(lit);
            combined.get(field).map(|v| value_gt(v, &threshold) || v == &threshold).unwrap_or(false)
        }
        WhereClause::Lte(field, lit) => {
            let threshold = literal_to_value(lit);
            combined.get(field).map(|v| value_lt(v, &threshold) || v == &threshold).unwrap_or(false)
        }
        WhereClause::And(l, r) => join_row_matches(combined, l) && join_row_matches(combined, r),
        WhereClause::Or(l, r) => join_row_matches(combined, l) || join_row_matches(combined, r),
        WhereClause::Not(inner) => !join_row_matches(combined, inner),
        WhereClause::IsNull(field) => {
            combined.get(field).map(|v| matches!(v, Value::Null)).unwrap_or(true)
        }
        WhereClause::IsNotNull(field) => {
            combined.get(field).map(|v| !matches!(v, Value::Null)).unwrap_or(false)
        }
        WhereClause::In(field, values) => {
            let fv = combined.get(field);
            values.iter().any(|lit| {
                let expected = literal_to_value(lit);
                fv.map(|v| *v == expected).unwrap_or(false)
            })
        }
        WhereClause::Like(field, pattern) => {
            combined.get(field).and_then(|v| match v {
                Value::String(s) => Some(like_match(s, pattern)),
                _ => None,
            }).unwrap_or(false)
        }
    }
}

fn project_join_row(combined: &HashMap<String, Value>, fields: &JoinFieldList) -> Row {
    let values = match fields {
        JoinFieldList::All => combined.clone(),
        JoinFieldList::Named(qfs) => {
            let mut projected = HashMap::new();
            for qf in qfs {
                let key = format!("{}.{}", qf.alias, qf.field);
                let val = combined.get(&key).cloned().unwrap_or(Value::Null);
                projected.insert(key, val);
            }
            projected
        }
    };
    Row { values, score: None }
}

fn build_join_columns(rows: &[Row], fields: &JoinFieldList) -> Vec<String> {
    match fields {
        JoinFieldList::Named(qfs) => {
            qfs.iter().map(|qf| format!("{}.{}", qf.alias, qf.field)).collect()
        }
        JoinFieldList::All => {
            let mut cols = Vec::new();
            for row in rows {
                for key in row.values.keys() {
                    if !cols.contains(key) {
                        cols.push(key.clone());
                    }
                }
            }
            cols.sort();
            cols
        }
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

// ---------------------------------------------------------------------------
// ORDER BY helpers
// ---------------------------------------------------------------------------

fn sort_rows(rows: &mut [Row], order_by: &[trondb_tql::OrderByClause]) {
    if order_by.is_empty() {
        return;
    }
    rows.sort_by(|a, b| {
        for clause in order_by {
            let a_val = a.values.get(&clause.field);
            let b_val = b.values.get(&clause.field);
            let cmp = compare_values(a_val, b_val);
            let cmp = match clause.direction {
                trondb_tql::SortDirection::Asc => cmp,
                trondb_tql::SortDirection::Desc => cmp.reverse(),
            };
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
        }
        std::cmp::Ordering::Equal
    });
}

fn compare_values(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) | (Some(Value::Null), Some(Value::Null)) => std::cmp::Ordering::Equal,
        (None | Some(Value::Null), _) => std::cmp::Ordering::Greater, // NULLs sort last
        (_, None | Some(Value::Null)) => std::cmp::Ordering::Less,
        (Some(Value::Int(x)), Some(Value::Int(y))) => x.cmp(y),
        (Some(Value::Float(x)), Some(Value::Float(y))) => {
            x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
        }
        (Some(Value::Int(x)), Some(Value::Float(y))) => {
            (*x as f64).partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
        }
        (Some(Value::Float(x)), Some(Value::Int(y))) => {
            x.partial_cmp(&(*y as f64)).unwrap_or(std::cmp::Ordering::Equal)
        }
        (Some(Value::String(x)), Some(Value::String(y))) => x.cmp(y),
        (Some(Value::Bool(x)), Some(Value::Bool(y))) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    }
}

/// Extract the primary collection name from a plan (for cost estimation).
fn plan_collection_name(plan: &Plan) -> Option<&str> {
    match plan {
        Plan::Fetch(p) => Some(&p.collection),
        Plan::Search(p) => Some(&p.collection),
        Plan::Insert(p) => Some(&p.collection),
        Plan::DeleteEntity(p) => Some(&p.collection),
        Plan::UpdateEntity(p) => Some(&p.collection),
        Plan::Demote(p) => Some(&p.collection),
        Plan::Promote(p) => Some(&p.collection),
        Plan::ExplainTiers(p) => Some(&p.collection),
        Plan::Join(p) => Some(&p.from_collection),
        _ => None,
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
            if !p.order_by.is_empty() {
                let order_str = p.order_by.iter()
                    .map(|o| format!("{} {}", o.field, match o.direction {
                        trondb_tql::SortDirection::Asc => "ASC",
                        trondb_tql::SortDirection::Desc => "DESC",
                    }))
                    .collect::<Vec<_>>()
                    .join(", ");
                props.push(("order_by", order_str));
            }
            if let Some(limit) = p.limit {
                props.push(("limit", limit.to_string()));
            }
            if !p.hints.is_empty() {
                let hints_str = p.hints.iter()
                    .map(|h| format!("{:?}", h))
                    .collect::<Vec<_>>()
                    .join(", ");
                props.push(("hints", hints_str));
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
                SearchStrategy::NaturalLanguage => "NaturalLanguageSearch",
            };
            props.push(("strategy", strategy_str.into()));
            props.push(("k", p.k.to_string()));
            props.push(("confidence_threshold", p.confidence_threshold.to_string()));
            if let Some(pf) = &p.pre_filter {
                props.push(("pre_filter", format!("ScalarPreFilter ({})", pf.index_name)));
            }
            if let Some(ref tp) = p.two_pass {
                props.push(("two_pass", format!("first_pass_k={}, binary={}", tp.first_pass_k, tp.use_binary_first_pass)));
            }
            if !p.hints.is_empty() {
                let hints_str = p.hints.iter()
                    .map(|h| format!("{:?}", h))
                    .collect::<Vec<_>>()
                    .join(", ");
                props.push(("hints", hints_str));
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
        Plan::Infer(p) => {
            props.push(("mode", "Probabilistic".into()));
            props.push(("verb", "INFER".into()));
            props.push(("from_id", p.from_id.clone()));
        }
        Plan::ConfirmEdge(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "CONFIRM EDGE".into()));
            props.push(("edge_type", p.edge_type.clone()));
        }
        Plan::ExplainHistory(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "EXPLAIN HISTORY".into()));
            props.push(("entity_id", p.entity_id.clone()));
        }
        Plan::DropCollection(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "DROP COLLECTION".into()));
            props.push(("collection", p.name.clone()));
            props.push(("tier", "Fjall".into()));
        }
        Plan::DropEdgeType(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "DROP EDGE TYPE".into()));
            props.push(("edge_type", p.name.clone()));
            props.push(("tier", "Fjall".into()));
        }
        Plan::Join(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "FETCH JOIN".into()));
            props.push(("from_collection", p.from_collection.clone()));
            props.push(("from_alias", p.from_alias.clone()));
            let joins_str = p.joins.iter()
                .map(|j| format!("{:?} {} AS {}", j.join_type, j.collection, j.alias))
                .collect::<Vec<_>>()
                .join(", ");
            props.push(("joins", joins_str));
            props.push(("tier", "Fjall".into()));
        }
        Plan::TraverseMatch(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "TRAVERSE MATCH".into()));
            props.push(("from_id", p.from_id.clone()));
            if let Some(ref et) = p.pattern.edge.edge_type {
                props.push(("edge_type", et.clone()));
            }
            props.push(("direction", format!("{:?}", p.pattern.edge.direction)));
            props.push(("depth", format!("{}..{}", p.min_depth, p.max_depth)));
            if let Some(conf) = p.confidence_threshold {
                props.push(("confidence_threshold", format!("{conf}")));
            }
            props.push(("tier", "Ram".into()));
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
    use crate::planner::{ConfirmEdgePlan, CreateCollectionPlan, CreateEdgeTypePlan, DeleteEntityPlan, DropCollectionPlan, DropEdgeTypePlan, ExplainHistoryPlan, FetchPlan, FetchStrategy, InferPlan, InsertEdgePlan, InsertPlan, JoinPlan, PreFilter, SearchPlan, TraverseMatchPlan, TraversePlan};
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
        (Executor::new(store, wal, Arc::new(LocationTable::new()), Arc::new(VectoriserRegistry::new()), Arc::new(InferenceAuditBuffer::new(1000))), dir)
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
            valid_from: None,
            valid_to: None,
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
                temporal: None,
                order_by: vec![],
                limit: None,
                strategy: FetchStrategy::FullScan,
                hints: vec![],
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
                temporal: None,
                order_by: vec![],
                limit: None,
                strategy: FetchStrategy::FullScan,
                hints: vec![],
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
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![],
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
            valid_from: None,
            valid_to: None,
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
            valid_from: None,
            valid_to: None,
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
            valid_from: None,
            valid_to: None,
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
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![Literal::String("v2".into()), Literal::String("Paris".into())],
            vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.4, 0.5, 0.6]))],
            collocate_with: None,
            affinity_group: None,
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // FETCH with FieldIndexLookup strategy
        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FieldIndexLookup("idx_city".into()),
            hints: vec![],
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
                valid_from: None,
                valid_to: None,
            })).await.unwrap();
        }

        // FETCH WHERE score >= 30 using FieldIndexRange strategy
        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "items".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Gte("score".into(), Literal::Int(30))),
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FieldIndexRange("idx_score".into()),
            hints: vec![],
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
                valid_from: None,
                valid_to: None,
            })).await.unwrap();
        }

        // FETCH WHERE score > 30 (strict) — should exclude score=30
        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "items".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Gt("score".into(), Literal::Int(30))),
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FieldIndexRange("idx_score".into()),
            hints: vec![],
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
                valid_from: None,
                valid_to: None,
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
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FieldIndexRange("idx_score".into()),
            hints: vec![],
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
                valid_from: None,
                valid_to: None,
            })).await.unwrap();
        }

        // FETCH WHERE score < 30 (strict) — should exclude score=30
        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "items".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Lt("score".into(), Literal::Int(30))),
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FieldIndexRange("idx_score".into()),
            hints: vec![],
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
                valid_from: None,
                valid_to: None,
            })).await.unwrap();
        }

        // FETCH WHERE score <= 30 — should include score=30
        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "items".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Lte("score".into(), Literal::Int(30))),
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FieldIndexRange("idx_score".into()),
            hints: vec![],
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
                valid_from: None,
                valid_to: None,
            })).await.unwrap();
        }

        // FETCH WHERE score >= 30 via FullScan (no index available)
        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "scores".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Gte("score".into(), Literal::Int(30))),
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![],
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
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "docs".into(),
            fields: vec!["id".into()],
            values: vec![Literal::String("d2".into())],
            vectors: vec![("keywords".to_string(), VectorLiteral::Sparse(vec![(1, 0.3), (99, 0.9)]))],
            collocate_with: None,
            affinity_group: None,
            valid_from: None,
            valid_to: None,
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
            query_text: None,
            using_repr: None,
            hints: vec![],
            two_pass: None,
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
            valid_from: None,
            valid_to: None,
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
            valid_from: None,
            valid_to: None,
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
            query_text: None,
            using_repr: None,
            hints: vec![],
            two_pass: None,
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
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![Literal::String("v2".into()), Literal::String("Paris".into())],
            vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.9, 0.1, 0.0]))],
            collocate_with: None,
            affinity_group: None,
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "city".into()],
            values: vec![Literal::String("v3".into()), Literal::String("London".into())],
            vectors: vec![("default".to_string(), VectorLiteral::Dense(vec![0.8, 0.2, 0.0]))],
            collocate_with: None,
            affinity_group: None,
            valid_from: None,
            valid_to: None,
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
            query_text: None,
            using_repr: None,
            hints: vec![],
            two_pass: None,
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
            query_text: None,
            using_repr: None,
            hints: vec![],
            two_pass: None,
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
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FieldIndexLookup("idx_city".into()),
            hints: vec![],
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
            query_text: None,
            using_repr: None,
            hints: vec![],
            two_pass: None,
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
    async fn explain_shows_force_full_scan_hint() {
        let (exec, _dir) = setup_executor().await;

        let fetch_plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![trondb_tql::QueryHint::ForceFullScan],
        });

        let result = exec
            .execute(&Plan::Explain(Box::new(fetch_plan)))
            .await
            .unwrap();

        // Verify strategy is FullScan (forced by hint)
        let strategy_row = result.rows.iter()
            .find(|r| r.values.get("property") == Some(&Value::String("strategy".into())))
            .expect("should have 'strategy' property");
        assert_eq!(
            strategy_row.values.get("value"),
            Some(&Value::String("FullScan".into()))
        );

        // Verify hints appear in EXPLAIN output
        let hints_row = result.rows.iter()
            .find(|r| r.values.get("property") == Some(&Value::String("hints".into())))
            .expect("should have 'hints' property");
        assert_eq!(
            hints_row.values.get("value"),
            Some(&Value::String("ForceFullScan".into()))
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
            inference_config: None,
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
                valid_from: None,
                valid_to: None,
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
            inference_config: None,
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
                valid_from: None,
                valid_to: None,
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
            inference_config: None,
        }))
        .await
        .unwrap();

        for (from, to) in [("a", "b"), ("b", "c")] {
            exec.execute(&Plan::InsertEdge(InsertEdgePlan {
                edge_type: "knows".into(),
                from_id: from.into(),
                to_id: to.into(),
                metadata: vec![],
                valid_from: None,
                valid_to: None,
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
            inference_config: None,
        }))
        .await
        .unwrap();

        for to in ["b", "c", "d"] {
            exec.execute(&Plan::InsertEdge(InsertEdgePlan {
                edge_type: "knows".into(),
                from_id: "a".into(),
                to_id: to.into(),
                metadata: vec![],
                valid_from: None,
                valid_to: None,
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
            inference_config: None,
        }))
        .await
        .unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(),
            from_id: "a".into(),
            to_id: "b".into(),
            metadata: vec![],
            valid_from: None,
            valid_to: None,
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
                temporal: None,
                order_by: vec![],
                limit: None,
                strategy: FetchStrategy::FullScan,
                hints: vec![],
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
                temporal: None,
                order_by: vec![],
                limit: None,
                strategy: FetchStrategy::FullScan,
                hints: vec![],
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
            inference_config: None,
        }))
        .await
        .unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(),
            from_id: "p1".into(),
            to_id: "p2".into(),
            metadata: vec![],
            valid_from: None,
            valid_to: None,
        }))
        .await
        .unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(),
            from_id: "p3".into(),
            to_id: "p1".into(),
            metadata: vec![],
            valid_from: None,
            valid_to: None,
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
                query_text: None,
                using_repr: None,
                hints: vec![],
                two_pass: None,
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
                query_text: None,
                using_repr: None,
                hints: vec![],
                two_pass: None,
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
            valid_from: None,
            valid_to: None,
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
            valid_from: None,
            valid_to: None,
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
                temporal: None,
                order_by: vec![],
                limit: None,
                strategy: FetchStrategy::FieldIndexLookup("idx_city".into()),
                hints: vec![],
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
                temporal: None,
                order_by: vec![],
                limit: None,
                strategy: FetchStrategy::FieldIndexLookup("idx_city".into()),
                hints: vec![],
            }))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("id"),
            Some(&Value::String("v2".into()))
        );
    }

    // -----------------------------------------------------------------------
    // Task 9 (Phase 10): SEARCH dirty exclusion
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn search_excludes_dirty_entities() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = FjallStore::open(&dir.path().join("store")).unwrap();
        let wal_config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };
        let wal = WalWriter::open(wal_config).await.unwrap();
        let location = Arc::new(LocationTable::new());
        let exec = Executor::new(store, wal, location.clone(), Arc::new(VectoriserRegistry::new()), Arc::new(InferenceAuditBuffer::new(1000)));

        // Create collection with a representation that has FIELDS
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "venues".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "semantic".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
                fields: vec!["name".into()],
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

        // Insert two entities with explicit vectors
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "name".into()],
            values: vec![Literal::String("v1".into()), Literal::String("Jazz Club".into())],
            vectors: vec![("semantic".to_string(), VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
            collocate_with: None,
            affinity_group: None,
            valid_from: None,
            valid_to: None,
        }))
        .await
        .unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "name".into()],
            values: vec![Literal::String("v2".into()), Literal::String("Rock Bar".into())],
            vectors: vec![("semantic".to_string(), VectorLiteral::Dense(vec![0.9, 0.1, 0.0]))],
            collocate_with: None,
            affinity_group: None,
            valid_from: None,
            valid_to: None,
        }))
        .await
        .unwrap();

        // Both should appear in a SEARCH before marking dirty
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
                query_text: None,
                using_repr: None,
                hints: vec![],
                two_pass: None,
            }))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 2, "both entities should appear before dirty");

        // Mark v1's representation as Dirty in the Location Table
        // repr_index 0 = "semantic" (first representation in schema)
        let repr_key = ReprKey {
            entity_id: LogicalId::from_string("v1"),
            repr_index: 0,
        };
        location.transition(&repr_key, LocState::Dirty).unwrap();

        // SEARCH should now exclude v1 (dirty), returning only v2
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
                query_text: None,
                using_repr: None,
                hints: vec![],
                two_pass: None,
            }))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 1, "dirty entity should be excluded");
        assert_eq!(
            result.rows[0].values.get("id"),
            Some(&Value::String("v2".into()))
        );

        // Also test Recomputing state exclusion
        let repr_key_v2 = ReprKey {
            entity_id: LogicalId::from_string("v2"),
            repr_index: 0,
        };
        // v1: Dirty -> Recomputing
        location.transition(&repr_key, LocState::Recomputing).unwrap();
        // v2: Clean -> Dirty -> Recomputing
        location.transition(&repr_key_v2, LocState::Dirty).unwrap();
        location.transition(&repr_key_v2, LocState::Recomputing).unwrap();

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
                query_text: None,
                using_repr: None,
                hints: vec![],
                two_pass: None,
            }))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 0, "both recomputing entities should be excluded");
    }

    // -----------------------------------------------------------------------
    // Task 11 (Phase 10): Natural Language SEARCH via vectoriser encode_query
    // -----------------------------------------------------------------------

    /// A test vectoriser that produces deterministic dense vectors from field values and query text.
    struct TestDenseVectoriser;

    #[async_trait::async_trait]
    impl crate::vectoriser::Vectoriser for TestDenseVectoriser {
        fn id(&self) -> &str { "test-dense" }
        fn model_id(&self) -> &str { "test-dense-model" }
        fn output_size(&self) -> usize { 3 }
        fn output_kind(&self) -> crate::vectoriser::VectorKind { crate::vectoriser::VectorKind::Dense }

        async fn encode(&self, fields: &crate::vectoriser::FieldSet) -> Result<VectorData, crate::vectoriser::VectoriserError> {
            // Produce a vector based on field content
            let combined: String = fields.values().cloned().collect();
            let v = if combined.contains("jazz") || combined.contains("Jazz") {
                vec![1.0_f32, 0.0, 0.0]
            } else if combined.contains("rock") || combined.contains("Rock") {
                vec![0.0_f32, 1.0, 0.0]
            } else {
                vec![0.0_f32, 0.0, 1.0]
            };
            Ok(VectorData::Dense(v))
        }

        async fn encode_query(&self, query: &str) -> Result<VectorData, crate::vectoriser::VectoriserError> {
            // Query about jazz → vector near [1,0,0]
            let v = if query.contains("jazz") || query.contains("Jazz") {
                vec![1.0_f32, 0.0, 0.0]
            } else if query.contains("rock") || query.contains("Rock") {
                vec![0.0_f32, 1.0, 0.0]
            } else {
                vec![0.5_f32, 0.5, 0.0]
            };
            Ok(VectorData::Dense(v))
        }
    }

    #[tokio::test]
    async fn search_natural_language() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = FjallStore::open(&dir.path().join("store")).unwrap();
        let wal_config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };
        let wal = WalWriter::open(wal_config).await.unwrap();
        let location = Arc::new(LocationTable::new());
        let registry = Arc::new(VectoriserRegistry::new());

        // Register the test vectoriser for venues:semantic
        registry.register("venues", "semantic", Arc::new(TestDenseVectoriser));

        let exec = Executor::new(store, wal, location, registry, Arc::new(InferenceAuditBuffer::new(1000)));

        // Create collection with a managed representation
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "venues".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "semantic".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
                fields: vec!["name".into()],
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

        // Insert entities — auto-vectorisation will call encode() via the registered vectoriser
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "name".into()],
            values: vec![Literal::String("v1".into()), Literal::String("jazz club".into())],
            vectors: vec![],
            collocate_with: None,
            affinity_group: None,
            valid_from: None,
            valid_to: None,
        }))
        .await
        .unwrap();

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "name".into()],
            values: vec![Literal::String("v2".into()), Literal::String("rock bar".into())],
            vectors: vec![],
            collocate_with: None,
            affinity_group: None,
            valid_from: None,
            valid_to: None,
        }))
        .await
        .unwrap();

        // Natural language SEARCH: "jazz" should find v1 first
        let result = exec
            .execute(&Plan::Search(SearchPlan {
                collection: "venues".into(),
                fields: FieldList::All,
                dense_vector: None,
                sparse_vector: None,
                filter: None,
                pre_filter: None,
                k: 10,
                confidence_threshold: 0.0,
                strategy: SearchStrategy::NaturalLanguage,
                query_text: Some("jazz".into()),
                using_repr: Some("semantic".into()),
                hints: vec![],
                two_pass: None,
            }))
            .await
            .unwrap();

        assert!(!result.rows.is_empty(), "should find at least one entity");
        // v1 (jazz) should be the top result since query vector [1,0,0] is closest to v1's [1,0,0]
        assert_eq!(
            result.rows[0].values.get("id"),
            Some(&Value::String("v1".into())),
            "jazz query should find v1 first"
        );

        // Natural language SEARCH: "rock" should find v2 first
        let result = exec
            .execute(&Plan::Search(SearchPlan {
                collection: "venues".into(),
                fields: FieldList::All,
                dense_vector: None,
                sparse_vector: None,
                filter: None,
                pre_filter: None,
                k: 10,
                confidence_threshold: 0.0,
                strategy: SearchStrategy::NaturalLanguage,
                query_text: Some("rock".into()),
                using_repr: Some("semantic".into()),
                hints: vec![],
                two_pass: None,
            }))
            .await
            .unwrap();

        assert!(!result.rows.is_empty(), "should find at least one entity");
        assert_eq!(
            result.rows[0].values.get("id"),
            Some(&Value::String("v2".into())),
            "rock query should find v2 first"
        );
    }

    #[tokio::test]
    async fn search_natural_language_auto_selects_repr() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = FjallStore::open(&dir.path().join("store")).unwrap();
        let wal_config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };
        let wal = WalWriter::open(wal_config).await.unwrap();
        let location = Arc::new(LocationTable::new());
        let registry = Arc::new(VectoriserRegistry::new());
        registry.register("venues", "semantic", Arc::new(TestDenseVectoriser));

        let exec = Executor::new(store, wal, location, registry, Arc::new(InferenceAuditBuffer::new(1000)));

        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "venues".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "semantic".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
                fields: vec!["name".into()],
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

        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "name".into()],
            values: vec![Literal::String("v1".into()), Literal::String("jazz club".into())],
            vectors: vec![],
            collocate_with: None,
            affinity_group: None,
            valid_from: None,
            valid_to: None,
        }))
        .await
        .unwrap();

        // SEARCH without USING — should auto-select "semantic" (first non-sparse repr with FIELDS)
        let result = exec
            .execute(&Plan::Search(SearchPlan {
                collection: "venues".into(),
                fields: FieldList::All,
                dense_vector: None,
                sparse_vector: None,
                filter: None,
                pre_filter: None,
                k: 10,
                confidence_threshold: 0.0,
                strategy: SearchStrategy::NaturalLanguage,
                query_text: Some("jazz".into()),
                using_repr: None,
                hints: vec![],
                two_pass: None,
            }))
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 1, "should find the inserted entity");
        assert_eq!(
            result.rows[0].values.get("id"),
            Some(&Value::String("v1".into()))
        );
    }

    // -----------------------------------------------------------------------
    // WAL replay: ReprDirty / ReprWrite
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn wal_replay_repr_dirty_transitions_location_to_dirty() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "events", 4).await;

        // Insert an entity so we have a Fjall record
        insert_entity(
            &exec,
            "events",
            "e1",
            vec![("name", Literal::String("Jazz".into()))],
            Some(vec![1.0, 0.0, 0.0, 0.0]),
        )
        .await;

        // Register a location entry for the entity's representation (Clean)
        let repr_key = ReprKey {
            entity_id: LogicalId::from_string("e1"),
            repr_index: 0,
        };
        exec.location.register(
            repr_key.clone(),
            LocationDescriptor {
                tier: Tier::Fjall,
                node_address: NodeAddress::localhost(),
                state: LocState::Clean,
                version: 1,
                encoding: Encoding::Float32,
                last_accessed: 0,
            },
        );

        // Build a ReprDirty WAL record
        let dirty_payload = rmp_serde::to_vec_named(&repr_key).unwrap();
        let wal_record = WalRecord {
            lsn: 100,
            ts: 0,
            tx_id: 50,
            record_type: RecordType::ReprDirty,
            schema_ver: 1,
            collection: "events".into(),
            payload: dirty_payload,
        };

        let (replayed, unhandled) = exec.replay_wal_records(&[wal_record]).unwrap();
        assert_eq!(replayed, 1);
        assert!(unhandled.is_empty());

        // Location should now be Dirty
        let loc = exec.location.get(&repr_key).unwrap();
        assert_eq!(loc.state, LocState::Dirty);
    }

    #[tokio::test]
    async fn wal_replay_repr_dirty_skips_missing_location() {
        let (exec, _dir) = setup_executor().await;

        // No location entry registered — ReprDirty should still count as replayed
        let repr_key = ReprKey {
            entity_id: LogicalId::from_string("ghost"),
            repr_index: 0,
        };
        let dirty_payload = rmp_serde::to_vec_named(&repr_key).unwrap();
        let wal_record = WalRecord {
            lsn: 101,
            ts: 0,
            tx_id: 51,
            record_type: RecordType::ReprDirty,
            schema_ver: 1,
            collection: "events".into(),
            payload: dirty_payload,
        };

        let (replayed, unhandled) = exec.replay_wal_records(&[wal_record]).unwrap();
        assert_eq!(replayed, 1);
        assert!(unhandled.is_empty());
    }

    #[tokio::test]
    async fn wal_replay_repr_write_stores_entity_and_cleans_location() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "events", 4).await;

        // Build an entity with one representation
        let entity = Entity {
            id: LogicalId::from_string("e1"),
            raw_data: bytes::Bytes::new(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("name".into(), Value::String("Blues".into()));
                m
            },
            representations: vec![Representation {
                name: "default".into(),
                repr_type: ReprType::Atomic,
                fields: vec!["name".into()],
                vector: VectorData::Dense(vec![0.5, 0.5, 0.0, 0.0]),
                recipe_hash: [0u8; 32],
                state: ReprState::Clean,
                computed_at: 0,
                model_version: String::new(),
            }],
            schema_version: 1,
            valid_from: None,
            valid_to: None,
            tx_time: 0,
        };

        // Register the location entry in Dirty state (simulates crash after UPDATE)
        let repr_key = ReprKey {
            entity_id: LogicalId::from_string("e1"),
            repr_index: 0,
        };
        exec.location.register(
            repr_key.clone(),
            LocationDescriptor {
                tier: Tier::Fjall,
                node_address: NodeAddress::localhost(),
                state: LocState::Clean,
                version: 1,
                encoding: Encoding::Float32,
                last_accessed: 0,
            },
        );
        // Transition to Dirty to simulate the state after UPDATE
        exec.location.transition(&repr_key, LocState::Dirty).unwrap();
        assert_eq!(exec.location.get(&repr_key).unwrap().state, LocState::Dirty);

        // Build a ReprWrite WAL record (payload is the full entity)
        let payload = rmp_serde::to_vec_named(&entity).unwrap();
        let wal_record = WalRecord {
            lsn: 200,
            ts: 0,
            tx_id: 60,
            record_type: RecordType::ReprWrite,
            schema_ver: 1,
            collection: "events".into(),
            payload,
        };

        let (replayed, unhandled) = exec.replay_wal_records(&[wal_record]).unwrap();
        assert_eq!(replayed, 1);
        assert!(unhandled.is_empty());

        // Location should now be Clean (Dirty → Recomputing → Clean)
        let loc = exec.location.get(&repr_key).unwrap();
        assert_eq!(loc.state, LocState::Clean);

        // Entity should be in Fjall
        let fetched_entity = exec.store.get("events", &LogicalId::from_string("e1")).unwrap();
        assert_eq!(
            fetched_entity.metadata.get("name"),
            Some(&Value::String("Blues".into()))
        );

        // entity_collections mapping should be set
        assert_eq!(
            exec.entity_collections.get(&LogicalId::from_string("e1")).map(|r| r.clone()),
            Some("events".to_string())
        );
    }

    #[tokio::test]
    async fn wal_replay_repr_write_cleans_recomputing_location() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "events", 4).await;

        let entity = Entity {
            id: LogicalId::from_string("e2"),
            raw_data: bytes::Bytes::new(),
            metadata: HashMap::new(),
            representations: vec![Representation {
                name: "default".into(),
                repr_type: ReprType::Atomic,
                fields: vec![],
                vector: VectorData::Dense(vec![1.0, 0.0, 0.0, 0.0]),
                recipe_hash: [0u8; 32],
                state: ReprState::Clean,
                computed_at: 0,
                model_version: String::new(),
            }],
            schema_version: 1,
            valid_from: None,
            valid_to: None,
            tx_time: 0,
        };

        // Register location entry and transition to Recomputing
        let repr_key = ReprKey {
            entity_id: LogicalId::from_string("e2"),
            repr_index: 0,
        };
        exec.location.register(
            repr_key.clone(),
            LocationDescriptor {
                tier: Tier::Fjall,
                node_address: NodeAddress::localhost(),
                state: LocState::Clean,
                version: 1,
                encoding: Encoding::Float32,
                last_accessed: 0,
            },
        );
        exec.location.transition(&repr_key, LocState::Dirty).unwrap();
        exec.location.transition(&repr_key, LocState::Recomputing).unwrap();
        assert_eq!(exec.location.get(&repr_key).unwrap().state, LocState::Recomputing);

        let payload = rmp_serde::to_vec_named(&entity).unwrap();
        let wal_record = WalRecord {
            lsn: 300,
            ts: 0,
            tx_id: 70,
            record_type: RecordType::ReprWrite,
            schema_ver: 1,
            collection: "events".into(),
            payload,
        };

        let (replayed, _) = exec.replay_wal_records(&[wal_record]).unwrap();
        assert_eq!(replayed, 1);

        // Should be Clean (Recomputing → Clean)
        let loc = exec.location.get(&repr_key).unwrap();
        assert_eq!(loc.state, LocState::Clean);
    }

    #[tokio::test]
    async fn wal_replay_repr_dirty_then_repr_write_full_cycle() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "events", 4).await;

        // Insert entity via normal path
        insert_entity(
            &exec,
            "events",
            "e3",
            vec![("name", Literal::String("Rock".into()))],
            Some(vec![0.0, 1.0, 0.0, 0.0]),
        )
        .await;

        // Register location for repr 0
        let repr_key = ReprKey {
            entity_id: LogicalId::from_string("e3"),
            repr_index: 0,
        };
        exec.location.register(
            repr_key.clone(),
            LocationDescriptor {
                tier: Tier::Fjall,
                node_address: NodeAddress::localhost(),
                state: LocState::Clean,
                version: 1,
                encoding: Encoding::Float32,
                last_accessed: 0,
            },
        );

        // Simulate replay: first ReprDirty, then ReprWrite
        let dirty_payload = rmp_serde::to_vec_named(&repr_key).unwrap();
        let dirty_record = WalRecord {
            lsn: 400,
            ts: 0,
            tx_id: 80,
            record_type: RecordType::ReprDirty,
            schema_ver: 1,
            collection: "events".into(),
            payload: dirty_payload,
        };

        let updated_entity = Entity {
            id: LogicalId::from_string("e3"),
            raw_data: bytes::Bytes::new(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("name".into(), Value::String("Metal".into()));
                m
            },
            representations: vec![Representation {
                name: "default".into(),
                repr_type: ReprType::Atomic,
                fields: vec!["name".into()],
                vector: VectorData::Dense(vec![0.0, 0.0, 1.0, 0.0]),
                recipe_hash: [0u8; 32],
                state: ReprState::Clean,
                computed_at: 0,
                model_version: String::new(),
            }],
            schema_version: 1,
            valid_from: None,
            valid_to: None,
            tx_time: 0,
        };
        let write_payload = rmp_serde::to_vec_named(&updated_entity).unwrap();
        let write_record = WalRecord {
            lsn: 401,
            ts: 0,
            tx_id: 81,
            record_type: RecordType::ReprWrite,
            schema_ver: 1,
            collection: "events".into(),
            payload: write_payload,
        };

        // Replay both in order
        let (replayed, unhandled) = exec.replay_wal_records(&[dirty_record, write_record]).unwrap();
        assert_eq!(replayed, 2);
        assert!(unhandled.is_empty());

        // Final state should be Clean
        let loc = exec.location.get(&repr_key).unwrap();
        assert_eq!(loc.state, LocState::Clean);

        // Entity should have the updated data
        let fetched = exec.store.get("events", &LogicalId::from_string("e3")).unwrap();
        assert_eq!(
            fetched.metadata.get("name"),
            Some(&Value::String("Metal".into()))
        );
    }

    // -----------------------------------------------------------------------
    // INFER executor tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn infer_proposes_edges_sorted_by_similarity() {
        let (exec, _dir) = setup_executor().await;

        // Create two collections: artists (source) and venues (target)
        create_collection(&exec, "artists", 3).await;
        create_collection(&exec, "venues", 3).await;

        // Insert source artist with a known vector
        insert_entity(
            &exec, "artists", "artist1",
            vec![("name", Literal::String("Band A".into()))],
            Some(vec![1.0, 0.0, 0.0]),
        ).await;

        // Insert target venues with vectors at various similarities to artist1
        // venue_close: very similar vector to artist1
        insert_entity(
            &exec, "venues", "venue_close",
            vec![("name", Literal::String("Close Venue".into()))],
            Some(vec![0.95, 0.1, 0.0]),
        ).await;
        // venue_mid: moderately similar
        insert_entity(
            &exec, "venues", "venue_mid",
            vec![("name", Literal::String("Mid Venue".into()))],
            Some(vec![0.5, 0.5, 0.0]),
        ).await;
        // venue_far: dissimilar
        insert_entity(
            &exec, "venues", "venue_far",
            vec![("name", Literal::String("Far Venue".into()))],
            Some(vec![0.0, 0.0, 1.0]),
        ).await;

        // Create edge type: performs_at (artists -> venues)
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "performs_at".into(),
            from_collection: "artists".into(),
            to_collection: "venues".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        // Run INFER
        let result = exec.execute(&Plan::Infer(InferPlan {
            from_id: "artist1".into(),
            edge_types: vec!["performs_at".into()],
            limit: Some(10),
            confidence_floor: None,
        })).await.unwrap();

        // Should have proposals
        assert!(!result.rows.is_empty(), "INFER should return proposals");

        // Results should be sorted by score descending
        let scores: Vec<f32> = result.rows.iter()
            .map(|r| r.score.unwrap())
            .collect();
        for w in scores.windows(2) {
            assert!(w[0] >= w[1], "Results should be sorted by score descending: {} >= {}", w[0], w[1]);
        }

        // First result should be venue_close (most similar)
        let first_to_id = result.rows[0].values.get("to_id").unwrap();
        assert_eq!(*first_to_id, Value::String("venue_close".into()),
            "First proposal should be the most similar entity");

        // All rows should have correct column values
        for row in &result.rows {
            assert_eq!(row.values.get("from_id"), Some(&Value::String("artist1".into())));
            assert_eq!(row.values.get("edge_type"), Some(&Value::String("performs_at".into())));
            assert!(row.values.get("to_id").is_some());
            assert!(row.values.get("confidence").is_some());
        }

        // Stats should be Probabilistic
        assert_eq!(result.stats.mode, QueryMode::Probabilistic);
    }

    #[tokio::test]
    async fn infer_excludes_existing_edges() {
        let (exec, _dir) = setup_executor().await;

        // Create collections
        create_collection(&exec, "artists", 3).await;
        create_collection(&exec, "venues", 3).await;

        // Insert entities
        insert_entity(
            &exec, "artists", "art1",
            vec![("name", Literal::String("Artist 1".into()))],
            Some(vec![1.0, 0.0, 0.0]),
        ).await;
        insert_entity(
            &exec, "venues", "ven1",
            vec![("name", Literal::String("Venue 1".into()))],
            Some(vec![0.95, 0.1, 0.0]),
        ).await;
        insert_entity(
            &exec, "venues", "ven2",
            vec![("name", Literal::String("Venue 2".into()))],
            Some(vec![0.9, 0.2, 0.0]),
        ).await;

        // Create edge type
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "plays_at".into(),
            from_collection: "artists".into(),
            to_collection: "venues".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        // Insert an edge from art1 -> ven1 (so ven1 should be excluded from INFER results)
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "plays_at".into(),
            from_id: "art1".into(),
            to_id: "ven1".into(),
            metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // Run INFER
        let result = exec.execute(&Plan::Infer(InferPlan {
            from_id: "art1".into(),
            edge_types: vec!["plays_at".into()],
            limit: Some(10),
            confidence_floor: None,
        })).await.unwrap();

        // ven1 should NOT appear in results (already has an edge)
        let to_ids: Vec<String> = result.rows.iter()
            .map(|r| match r.values.get("to_id").unwrap() {
                Value::String(s) => s.clone(),
                _ => panic!("expected string"),
            })
            .collect();
        assert!(!to_ids.contains(&"ven1".to_string()),
            "Entities with existing edges should be excluded from INFER results");
        // ven2 SHOULD appear
        assert!(to_ids.contains(&"ven2".to_string()),
            "Entities without existing edges should appear in INFER results");
    }

    #[tokio::test]
    async fn infer_applies_confidence_floor() {
        let (exec, _dir) = setup_executor().await;

        create_collection(&exec, "src", 3).await;
        create_collection(&exec, "tgt", 3).await;

        insert_entity(
            &exec, "src", "s1",
            vec![],
            Some(vec![1.0, 0.0, 0.0]),
        ).await;

        // Insert target with a moderately similar vector
        insert_entity(
            &exec, "tgt", "t1",
            vec![],
            Some(vec![0.5, 0.5, 0.0]),
        ).await;
        // Insert target with a very similar vector
        insert_entity(
            &exec, "tgt", "t2",
            vec![],
            Some(vec![0.99, 0.01, 0.0]),
        ).await;

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "related".into(),
            from_collection: "src".into(),
            to_collection: "tgt".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        // Run INFER with a high confidence floor — only the very similar entity should pass
        let result = exec.execute(&Plan::Infer(InferPlan {
            from_id: "s1".into(),
            edge_types: vec!["related".into()],
            limit: Some(10),
            confidence_floor: Some(0.95),
        })).await.unwrap();

        // t2 (very similar) should be included, t1 may be filtered out
        let to_ids: Vec<String> = result.rows.iter()
            .map(|r| match r.values.get("to_id").unwrap() {
                Value::String(s) => s.clone(),
                _ => panic!("expected string"),
            })
            .collect();
        assert!(to_ids.contains(&"t2".to_string()),
            "Highly similar entity should pass the confidence floor");
        // With floor 0.95, the moderately similar entity (cosine ~0.707) should be excluded
        assert!(!to_ids.contains(&"t1".to_string()),
            "Moderately similar entity should be below the confidence floor");
    }

    #[tokio::test]
    async fn infer_applies_limit() {
        let (exec, _dir) = setup_executor().await;

        create_collection(&exec, "src", 3).await;
        create_collection(&exec, "tgt", 3).await;

        insert_entity(
            &exec, "src", "s1",
            vec![],
            Some(vec![1.0, 0.0, 0.0]),
        ).await;

        // Insert many target entities
        for i in 0..5 {
            let frac = 1.0 - (i as f64 * 0.1);
            insert_entity(
                &exec, "tgt", &format!("t{}", i),
                vec![],
                Some(vec![frac, 1.0 - frac, 0.0]),
            ).await;
        }

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "linked".into(),
            from_collection: "src".into(),
            to_collection: "tgt".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        // INFER with limit 2
        let result = exec.execute(&Plan::Infer(InferPlan {
            from_id: "s1".into(),
            edge_types: vec!["linked".into()],
            limit: Some(2),
            confidence_floor: None,
        })).await.unwrap();

        assert!(result.rows.len() <= 2, "INFER should respect the limit: got {} rows", result.rows.len());
    }

    #[tokio::test]
    async fn infer_auto_discovers_edge_types() {
        let (exec, _dir) = setup_executor().await;

        create_collection(&exec, "people", 3).await;
        create_collection(&exec, "venues", 3).await;

        insert_entity(
            &exec, "people", "p1",
            vec![],
            Some(vec![1.0, 0.0, 0.0]),
        ).await;
        insert_entity(
            &exec, "venues", "v1",
            vec![],
            Some(vec![0.9, 0.1, 0.0]),
        ).await;

        // Create edge type: visits (people -> venues)
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "visits".into(),
            from_collection: "people".into(),
            to_collection: "venues".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        // Run INFER with empty edge_types — should auto-discover "visits"
        let result = exec.execute(&Plan::Infer(InferPlan {
            from_id: "p1".into(),
            edge_types: vec![],
            limit: Some(10),
            confidence_floor: None,
        })).await.unwrap();

        assert!(!result.rows.is_empty(), "INFER with empty edge_types should auto-discover applicable types");
        assert_eq!(
            result.rows[0].values.get("edge_type"),
            Some(&Value::String("visits".into())),
        );
    }

    #[tokio::test]
    async fn infer_returns_empty_for_no_applicable_types() {
        let (exec, _dir) = setup_executor().await;

        create_collection(&exec, "lonely", 3).await;

        insert_entity(
            &exec, "lonely", "l1",
            vec![],
            Some(vec![1.0, 0.0, 0.0]),
        ).await;

        // No edge types defined — INFER should return empty
        let result = exec.execute(&Plan::Infer(InferPlan {
            from_id: "l1".into(),
            edge_types: vec![],
            limit: Some(10),
            confidence_floor: None,
        })).await.unwrap();

        assert!(result.rows.is_empty(), "INFER with no applicable edge types should return empty");
    }

    // -----------------------------------------------------------------------
    // CONFIRM EDGE executor tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn confirm_creates_new_confirmed_edge() {
        let (exec, _dir) = setup_executor().await;

        // Create collections and edge type
        create_collection(&exec, "people", 3).await;
        create_collection(&exec, "places", 3).await;

        insert_entity(&exec, "people", "p1", vec![], None).await;
        insert_entity(&exec, "places", "pl1", vec![], None).await;

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "visits".into(),
            from_collection: "people".into(),
            to_collection: "places".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        // CONFIRM EDGE when no edge exists — should create new Confirmed edge
        let result = exec.execute(&Plan::ConfirmEdge(ConfirmEdgePlan {
            from_id: "p1".into(),
            to_id: "pl1".into(),
            edge_type: "visits".into(),
            confidence: 0.9,
        })).await.unwrap();

        let msg = match result.rows[0].values.get("result").unwrap() {
            Value::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(msg.contains("confirmed"), "Result message should mention confirmed: {}", msg);

        // Verify edge was persisted in Fjall with Confirmed source
        let stored = exec.store.get_edge("visits", "p1", "pl1").unwrap();
        assert!(stored.is_some(), "Edge should exist in Fjall after CONFIRM");
        let edge = stored.unwrap();
        assert_eq!(edge.source, EdgeSource::Confirmed);
        assert!((edge.confidence - 0.9).abs() < 0.001);

        // Verify AdjacencyIndex was updated
        let adj_entries = exec.adjacency.get(&LogicalId::from_string("p1"), "visits");
        assert_eq!(adj_entries.len(), 1);
        assert_eq!(adj_entries[0].to_id, LogicalId::from_string("pl1"));
        assert_eq!(adj_entries[0].source, EdgeSource::Confirmed);
        assert!((adj_entries[0].confidence - 0.9).abs() < 0.001);
    }

    #[tokio::test]
    async fn confirm_rejects_structural_edge() {
        let (exec, _dir) = setup_executor().await;

        create_collection(&exec, "artists", 3).await;
        create_collection(&exec, "venues", 3).await;

        insert_entity(&exec, "artists", "a1", vec![], None).await;
        insert_entity(&exec, "venues", "v1", vec![], None).await;

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "plays_at".into(),
            from_collection: "artists".into(),
            to_collection: "venues".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        // Insert a structural edge (normal INSERT EDGE creates structural edges)
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "plays_at".into(),
            from_id: "a1".into(),
            to_id: "v1".into(),
            metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // CONFIRM should fail on structural edge
        let result = exec.execute(&Plan::ConfirmEdge(ConfirmEdgePlan {
            from_id: "a1".into(),
            to_id: "v1".into(),
            edge_type: "plays_at".into(),
            confidence: 0.95,
        })).await;

        assert!(result.is_err(), "CONFIRM should reject structural edges");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("structural"), "Error should mention structural: {}", err_msg);
    }

    #[tokio::test]
    async fn confirm_updates_existing_confirmed_edge_confidence() {
        let (exec, _dir) = setup_executor().await;

        create_collection(&exec, "people", 3).await;
        create_collection(&exec, "places", 3).await;

        insert_entity(&exec, "people", "p1", vec![], None).await;
        insert_entity(&exec, "places", "pl1", vec![], None).await;

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "visits".into(),
            from_collection: "people".into(),
            to_collection: "places".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        // First CONFIRM — creates new Confirmed edge at 0.8
        exec.execute(&Plan::ConfirmEdge(ConfirmEdgePlan {
            from_id: "p1".into(),
            to_id: "pl1".into(),
            edge_type: "visits".into(),
            confidence: 0.8,
        })).await.unwrap();

        // Verify initial state
        let edge = exec.store.get_edge("visits", "p1", "pl1").unwrap().unwrap();
        assert_eq!(edge.source, EdgeSource::Confirmed);
        assert!((edge.confidence - 0.8).abs() < 0.001);
        let original_created_at = edge.created_at;

        // Re-CONFIRM with different confidence — should update confidence, preserve created_at
        let result = exec.execute(&Plan::ConfirmEdge(ConfirmEdgePlan {
            from_id: "p1".into(),
            to_id: "pl1".into(),
            edge_type: "visits".into(),
            confidence: 0.95,
        })).await.unwrap();

        let msg = match result.rows[0].values.get("result").unwrap() {
            Value::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(msg.contains("re-confirmed"), "Result message should mention re-confirmed: {}", msg);

        // Verify updated state in Fjall
        let updated = exec.store.get_edge("visits", "p1", "pl1").unwrap().unwrap();
        assert_eq!(updated.source, EdgeSource::Confirmed);
        assert!((updated.confidence - 0.95).abs() < 0.001);
        assert_eq!(updated.created_at, original_created_at, "created_at should be preserved on re-confirmation");

        // Verify AdjacencyIndex reflects updated confidence
        let adj_entries = exec.adjacency.get(&LogicalId::from_string("p1"), "visits");
        assert_eq!(adj_entries.len(), 1);
        assert!((adj_entries[0].confidence - 0.95).abs() < 0.001);
    }

    #[tokio::test]
    async fn explain_history_returns_audit_entries() {
        let (exec, _dir) = setup_executor().await;

        // Create collections
        create_collection(&exec, "artists", 3).await;
        create_collection(&exec, "venues", 3).await;

        // Insert source entity
        insert_entity(
            &exec, "artists", "artist1",
            vec![("name", Literal::String("Rock Artist".into()))],
            Some(vec![1.0, 0.0, 0.0]),
        ).await;

        // Insert target entities
        insert_entity(
            &exec, "venues", "venue1",
            vec![("name", Literal::String("Close Venue".into()))],
            Some(vec![0.95, 0.1, 0.0]),
        ).await;
        insert_entity(
            &exec, "venues", "venue2",
            vec![("name", Literal::String("Mid Venue".into()))],
            Some(vec![0.5, 0.5, 0.0]),
        ).await;

        // Create edge type
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "performs_at".into(),
            from_collection: "artists".into(),
            to_collection: "venues".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        // EXPLAIN HISTORY before any INFER — should be empty
        let result = exec.execute(&Plan::ExplainHistory(ExplainHistoryPlan {
            entity_id: "artist1".into(),
            limit: None,
        })).await.unwrap();
        assert!(result.rows.is_empty(), "EXPLAIN HISTORY should return no entries before INFER");

        // Run INFER to generate audit entries
        let infer_result = exec.execute(&Plan::Infer(InferPlan {
            from_id: "artist1".into(),
            edge_types: vec!["performs_at".into()],
            limit: Some(10),
            confidence_floor: None,
        })).await.unwrap();
        assert!(!infer_result.rows.is_empty(), "INFER should return proposals");

        // Now EXPLAIN HISTORY should return entries
        let result = exec.execute(&Plan::ExplainHistory(ExplainHistoryPlan {
            entity_id: "artist1".into(),
            limit: None,
        })).await.unwrap();

        assert_eq!(result.rows.len(), 1, "EXPLAIN HISTORY should return 1 audit entry after 1 INFER");

        let row = &result.rows[0];
        assert_eq!(row.values.get("entity_id"), Some(&Value::String("artist1".into())));
        assert_eq!(row.values.get("edge_type"), Some(&Value::String("all".into())));
        assert_eq!(row.values.get("trigger"), Some(&Value::String("Explicit".into())));

        // candidates_evaluated should be > 0
        if let Some(Value::Int(n)) = row.values.get("candidates_evaluated") {
            assert!(*n > 0, "candidates_evaluated should be > 0, got {}", n);
        } else {
            panic!("candidates_evaluated should be an Int");
        }

        // Run INFER again
        exec.execute(&Plan::Infer(InferPlan {
            from_id: "artist1".into(),
            edge_types: vec!["performs_at".into()],
            limit: Some(10),
            confidence_floor: None,
        })).await.unwrap();

        // EXPLAIN HISTORY should now return 2 entries
        let result = exec.execute(&Plan::ExplainHistory(ExplainHistoryPlan {
            entity_id: "artist1".into(),
            limit: None,
        })).await.unwrap();
        assert_eq!(result.rows.len(), 2, "EXPLAIN HISTORY should return 2 audit entries after 2 INFERs");

        // Test limit parameter
        let result = exec.execute(&Plan::ExplainHistory(ExplainHistoryPlan {
            entity_id: "artist1".into(),
            limit: Some(1),
        })).await.unwrap();
        assert_eq!(result.rows.len(), 1, "EXPLAIN HISTORY with limit 1 should return 1 entry");

        // EXPLAIN HISTORY for a different entity should return empty
        let result = exec.execute(&Plan::ExplainHistory(ExplainHistoryPlan {
            entity_id: "venue1".into(),
            limit: None,
        })).await.unwrap();
        assert!(result.rows.is_empty(), "EXPLAIN HISTORY for non-inferred entity should be empty");
    }

    // -----------------------------------------------------------------------
    // Background inference queue tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn background_inference_creates_inferred_edges() {
        let (exec, _dir) = setup_executor().await;

        // Create source and target collections
        create_collection(&exec, "artists", 3).await;
        create_collection(&exec, "venues", 3).await;

        // Create edge type with INFER AUTO enabled
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "performs_at".into(),
            from_collection: "artists".into(),
            to_collection: "venues".into(),
            decay_config: None,
            inference_config: Some(crate::edge::InferenceConfig {
                auto: true,
                confidence_floor: 0.0,
                limit: 10,
            }),
        })).await.unwrap();

        // Insert target venues first (so HNSW has something to search)
        insert_entity(
            &exec, "venues", "venue1",
            vec![("name", Literal::String("Jazz Club".into()))],
            Some(vec![0.95, 0.1, 0.0]),
        ).await;
        insert_entity(
            &exec, "venues", "venue2",
            vec![("name", Literal::String("Rock Bar".into()))],
            Some(vec![0.5, 0.5, 0.0]),
        ).await;

        // Insert source artist — this should enqueue the entity for background inference
        insert_entity(
            &exec, "artists", "artist1",
            vec![("name", Literal::String("Band A".into()))],
            Some(vec![1.0, 0.0, 0.0]),
        ).await;

        // Verify the entity was enqueued
        assert!(
            exec.inference_queue().contains(&LogicalId::from_string("artist1")),
            "artist1 should be in the inference queue after INSERT"
        );

        // Directly call drain_inference_queue (don't wait for background timer)
        let processed = exec.drain_inference_queue().await.unwrap();
        assert_eq!(processed, 1, "should have processed 1 entity from queue");

        // Queue should be empty after drain
        assert!(exec.inference_queue().is_empty(), "inference queue should be empty after drain");

        // Verify inferred edges were created
        let edges = exec.adjacency().get(&LogicalId::from_string("artist1"), "performs_at");
        assert!(!edges.is_empty(), "inferred edges should have been created");

        // All inferred edges should have EdgeSource::Inferred
        for edge in &edges {
            assert_eq!(edge.source, EdgeSource::Inferred, "edges should be Inferred");
            assert!(edge.confidence > 0.0, "confidence should be > 0");
        }

        // venue1 should be the closest match (most similar vector)
        let to_ids: Vec<String> = edges.iter().map(|e| e.to_id.to_string()).collect();
        assert!(to_ids.contains(&"venue1".to_string()), "venue1 should be in inferred edges");
    }

    #[tokio::test]
    async fn inference_queue_populated_on_update() {
        let (exec, _dir) = setup_executor().await;

        create_collection(&exec, "artists", 3).await;
        create_collection(&exec, "venues", 3).await;

        // Create edge type with auto inference
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "performs_at".into(),
            from_collection: "artists".into(),
            to_collection: "venues".into(),
            decay_config: None,
            inference_config: Some(crate::edge::InferenceConfig {
                auto: true,
                confidence_floor: 0.0,
                limit: 10,
            }),
        })).await.unwrap();

        // Insert artist
        insert_entity(
            &exec, "artists", "artist1",
            vec![("name", Literal::String("Band A".into()))],
            Some(vec![1.0, 0.0, 0.0]),
        ).await;

        // Drain queue from the INSERT
        exec.drain_inference_queue().await.unwrap();
        assert!(exec.inference_queue().is_empty());

        // Update artist — should re-enqueue
        exec.execute(&Plan::UpdateEntity(crate::planner::UpdateEntityPlan {
            collection: "artists".into(),
            entity_id: "artist1".into(),
            assignments: vec![
                ("name".into(), Literal::String("Band B".into())),
            ],
        })).await.unwrap();

        assert!(
            exec.inference_queue().contains(&LogicalId::from_string("artist1")),
            "artist1 should be re-enqueued after UPDATE"
        );
    }

    #[tokio::test]
    async fn inference_queue_not_populated_without_auto() {
        let (exec, _dir) = setup_executor().await;

        create_collection(&exec, "artists", 3).await;
        create_collection(&exec, "venues", 3).await;

        // Create edge type WITHOUT auto inference
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "performs_at".into(),
            from_collection: "artists".into(),
            to_collection: "venues".into(),
            decay_config: None,
            inference_config: None, // default: auto=false
        })).await.unwrap();

        // Insert artist
        insert_entity(
            &exec, "artists", "artist1",
            vec![("name", Literal::String("Band A".into()))],
            Some(vec![1.0, 0.0, 0.0]),
        ).await;

        // Queue should be empty since no auto inference edge types
        assert!(
            exec.inference_queue().is_empty(),
            "inference queue should be empty when no auto edge types exist"
        );
    }

    #[tokio::test]
    async fn drain_inference_queue_skips_existing_edges() {
        let (exec, _dir) = setup_executor().await;

        create_collection(&exec, "artists", 3).await;
        create_collection(&exec, "venues", 3).await;

        // Create edge type with auto inference
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "performs_at".into(),
            from_collection: "artists".into(),
            to_collection: "venues".into(),
            decay_config: None,
            inference_config: Some(crate::edge::InferenceConfig {
                auto: true,
                confidence_floor: 0.0,
                limit: 10,
            }),
        })).await.unwrap();

        // Insert venues
        insert_entity(
            &exec, "venues", "venue1",
            vec![("name", Literal::String("Jazz Club".into()))],
            Some(vec![0.95, 0.1, 0.0]),
        ).await;

        // Insert artist
        insert_entity(
            &exec, "artists", "artist1",
            vec![("name", Literal::String("Band A".into()))],
            Some(vec![1.0, 0.0, 0.0]),
        ).await;

        // Manually insert an edge from artist1 -> venue1 (structural)
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "performs_at".into(),
            from_id: "artist1".into(),
            to_id: "venue1".into(),
            metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // Drain the inference queue
        exec.drain_inference_queue().await.unwrap();

        // The edges for artist1 -> venue1 should still be just the structural one
        let edges = exec.adjacency().get(&LogicalId::from_string("artist1"), "performs_at");
        let venue1_edges: Vec<_> = edges.iter()
            .filter(|e| e.to_id == LogicalId::from_string("venue1"))
            .collect();
        assert_eq!(venue1_edges.len(), 1, "should not create duplicate edge to venue1");
        assert_eq!(venue1_edges[0].source, EdgeSource::Structural, "original edge should be Structural");
    }

    // -----------------------------------------------------------------------
    // DecaySweeper tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn decay_sweeper_prunes_old_inferred_edges() {
        let (exec, _dir) = setup_executor().await;

        // Create source and target collections
        create_collection(&exec, "artists", 3).await;
        create_collection(&exec, "venues", 3).await;

        // Create edge type with:
        //  - Exponential decay with very aggressive rate
        //  - prune_threshold high enough that even fresh edges decay below it quickly
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "performs_at".into(),
            from_collection: "artists".into(),
            to_collection: "venues".into(),
            decay_config: Some(trondb_tql::DecayConfigDecl {
                decay_fn: Some(trondb_tql::DecayFnDecl::Exponential),
                decay_rate: Some(10.0),  // very fast decay
                floor: Some(0.0),
                promote_threshold: None,
                prune_threshold: Some(0.99),  // very high threshold — fresh edges will quickly fall below
            }),
            inference_config: Some(crate::edge::InferenceConfig {
                auto: true,
                confidence_floor: 0.0,
                limit: 10,
            }),
        })).await.unwrap();

        // Insert target venues
        insert_entity(
            &exec, "venues", "venue1",
            vec![("name", Literal::String("Jazz Club".into()))],
            Some(vec![0.95, 0.1, 0.0]),
        ).await;
        insert_entity(
            &exec, "venues", "venue2",
            vec![("name", Literal::String("Rock Bar".into()))],
            Some(vec![0.5, 0.5, 0.0]),
        ).await;

        // Insert source artist — this enqueues for background inference
        insert_entity(
            &exec, "artists", "artist1",
            vec![("name", Literal::String("Band A".into()))],
            Some(vec![1.0, 0.0, 0.0]),
        ).await;

        // Drain inference queue to create inferred edges
        exec.drain_inference_queue().await.unwrap();

        // Verify inferred edges were created
        let edges_before = exec.adjacency().get(&LogicalId::from_string("artist1"), "performs_at");
        assert!(!edges_before.is_empty(), "inferred edges should have been created");
        for edge in &edges_before {
            assert_eq!(edge.source, EdgeSource::Inferred, "edges should be Inferred");
        }

        // Small sleep to let decay accumulate (with rate=10.0, even 10ms is massive decay)
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Run the decay sweeper
        let pruned = exec.sweep_decayed_edges().await.unwrap();
        assert!(pruned > 0, "decay sweeper should have pruned at least one edge");

        // Verify edges are gone from AdjacencyIndex
        let edges_after = exec.adjacency().get(&LogicalId::from_string("artist1"), "performs_at");
        assert!(edges_after.is_empty(), "all inferred edges should have been pruned");

        // Verify edges are gone from Fjall
        let stored_edges = exec.scan_edges("performs_at").unwrap();
        let remaining_inferred: Vec<_> = stored_edges.iter()
            .filter(|e| e.source == EdgeSource::Inferred)
            .collect();
        assert!(remaining_inferred.is_empty(), "no inferred edges should remain in Fjall");
    }

    #[tokio::test]
    async fn decay_sweeper_skips_structural_and_confirmed_edges() {
        let (exec, _dir) = setup_executor().await;

        create_collection(&exec, "people", 3).await;

        // Create edge type with aggressive decay + prune threshold
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: Some(trondb_tql::DecayConfigDecl {
                decay_fn: Some(trondb_tql::DecayFnDecl::Exponential),
                decay_rate: Some(10.0),
                floor: Some(0.0),
                promote_threshold: None,
                prune_threshold: Some(0.99),
            }),
            inference_config: None,
        })).await.unwrap();

        // Insert entities
        insert_entity(
            &exec, "people", "alice",
            vec![("name", Literal::String("Alice".into()))],
            Some(vec![1.0, 0.0, 0.0]),
        ).await;
        insert_entity(
            &exec, "people", "bob",
            vec![("name", Literal::String("Bob".into()))],
            Some(vec![0.0, 1.0, 0.0]),
        ).await;

        // Create a structural edge via INSERT EDGE
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(),
            from_id: "alice".into(),
            to_id: "bob".into(),
            metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // Verify the structural edge exists
        let edges_before = exec.adjacency().get(&LogicalId::from_string("alice"), "knows");
        assert_eq!(edges_before.len(), 1);
        assert_eq!(edges_before[0].source, EdgeSource::Structural);

        // Sleep to let decay accumulate
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Run the decay sweeper
        let pruned = exec.sweep_decayed_edges().await.unwrap();
        assert_eq!(pruned, 0, "structural edges should not be pruned");

        // Verify the structural edge still exists
        let edges_after = exec.adjacency().get(&LogicalId::from_string("alice"), "knows");
        assert_eq!(edges_after.len(), 1, "structural edge should remain after sweep");
    }

    #[tokio::test]
    async fn decay_sweeper_skips_edge_types_without_prune_threshold() {
        let (exec, _dir) = setup_executor().await;

        create_collection(&exec, "artists", 3).await;
        create_collection(&exec, "venues", 3).await;

        // Create edge type WITHOUT prune threshold
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "performs_at".into(),
            from_collection: "artists".into(),
            to_collection: "venues".into(),
            decay_config: Some(trondb_tql::DecayConfigDecl {
                decay_fn: Some(trondb_tql::DecayFnDecl::Exponential),
                decay_rate: Some(10.0),
                floor: Some(0.0),
                promote_threshold: None,
                prune_threshold: None,  // no prune threshold
            }),
            inference_config: Some(crate::edge::InferenceConfig {
                auto: true,
                confidence_floor: 0.0,
                limit: 10,
            }),
        })).await.unwrap();

        // Insert target venue
        insert_entity(
            &exec, "venues", "venue1",
            vec![("name", Literal::String("Jazz Club".into()))],
            Some(vec![0.95, 0.1, 0.0]),
        ).await;

        // Insert source artist
        insert_entity(
            &exec, "artists", "artist1",
            vec![("name", Literal::String("Band A".into()))],
            Some(vec![1.0, 0.0, 0.0]),
        ).await;

        // Drain inference queue
        exec.drain_inference_queue().await.unwrap();

        let edges_before = exec.adjacency().get(&LogicalId::from_string("artist1"), "performs_at");
        assert!(!edges_before.is_empty(), "inferred edges should exist");

        // Sleep then sweep
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let pruned = exec.sweep_decayed_edges().await.unwrap();
        assert_eq!(pruned, 0, "should not prune when no prune_threshold is set");

        // Edges should still be there
        let edges_after = exec.adjacency().get(&LogicalId::from_string("artist1"), "performs_at");
        assert_eq!(edges_before.len(), edges_after.len(), "edges should remain unchanged");
    }

    #[tokio::test]
    async fn fetch_where_neq() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;
        insert_entity(&exec, "venues", "v1", vec![("status", Literal::String("active".into()))], None).await;
        insert_entity(&exec, "venues", "v2", vec![("status", Literal::String("archived".into()))], None).await;
        insert_entity(&exec, "venues", "v3", vec![("status", Literal::String("active".into()))], None).await;

        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Neq("status".into(), Literal::String("archived".into()))),
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![],
        })).await.unwrap();
        assert_eq!(result.rows.len(), 2);
    }

    #[tokio::test]
    async fn fetch_where_is_null() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;
        insert_entity(&exec, "venues", "v1", vec![("name", Literal::String("Blue Note".into()))], None).await;
        insert_entity(&exec, "venues", "v2", vec![], None).await;

        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::IsNull("name".into())),
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![],
        })).await.unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].values.get("id").unwrap(), &Value::String("v2".into()));
    }

    #[tokio::test]
    async fn fetch_where_is_not_null() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;
        insert_entity(&exec, "venues", "v1", vec![("name", Literal::String("Blue Note".into()))], None).await;
        insert_entity(&exec, "venues", "v2", vec![], None).await;

        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::IsNotNull("name".into())),
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![],
        })).await.unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].values.get("id").unwrap(), &Value::String("v1".into()));
    }

    #[tokio::test]
    async fn fetch_where_in() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;
        insert_entity(&exec, "venues", "v1", vec![("category", Literal::String("music".into()))], None).await;
        insert_entity(&exec, "venues", "v2", vec![("category", Literal::String("theatre".into()))], None).await;
        insert_entity(&exec, "venues", "v3", vec![("category", Literal::String("food".into()))], None).await;

        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::In("category".into(), vec![
                Literal::String("music".into()),
                Literal::String("theatre".into()),
            ])),
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![],
        })).await.unwrap();
        assert_eq!(result.rows.len(), 2);
    }

    #[tokio::test]
    async fn fetch_where_like() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;
        insert_entity(&exec, "venues", "v1", vec![("name", Literal::String("Jazz Cafe".into()))], None).await;
        insert_entity(&exec, "venues", "v2", vec![("name", Literal::String("Jazz Club Bristol".into()))], None).await;
        insert_entity(&exec, "venues", "v3", vec![("name", Literal::String("Rock Bar".into()))], None).await;

        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Like("name".into(), "Jazz%".into())),
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![],
        })).await.unwrap();
        assert_eq!(result.rows.len(), 2);
    }

    #[tokio::test]
    async fn fetch_where_not() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;
        insert_entity(&exec, "venues", "v1", vec![("active", Literal::Bool(true))], None).await;
        insert_entity(&exec, "venues", "v2", vec![("active", Literal::Bool(false))], None).await;

        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Not(Box::new(
                WhereClause::Eq("active".into(), Literal::Bool(true))
            ))),
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![],
        })).await.unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].values.get("id").unwrap(), &Value::String("v2".into()));
    }

    #[tokio::test]
    async fn fetch_order_by_asc() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;
        insert_entity(&exec, "venues", "v1", vec![("name", Literal::String("Charlie".into()))], None).await;
        insert_entity(&exec, "venues", "v2", vec![("name", Literal::String("Alpha".into()))], None).await;
        insert_entity(&exec, "venues", "v3", vec![("name", Literal::String("Bravo".into()))], None).await;

        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            temporal: None,
            order_by: vec![trondb_tql::OrderByClause {
                field: "name".into(),
                direction: trondb_tql::SortDirection::Asc,
            }],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![],
        })).await.unwrap();

        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.rows[0].values.get("name").unwrap(), &Value::String("Alpha".into()));
        assert_eq!(result.rows[1].values.get("name").unwrap(), &Value::String("Bravo".into()));
        assert_eq!(result.rows[2].values.get("name").unwrap(), &Value::String("Charlie".into()));
    }

    #[tokio::test]
    async fn fetch_order_by_desc_with_limit() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;
        insert_entity(&exec, "venues", "v1", vec![("score", Literal::Int(10))], None).await;
        insert_entity(&exec, "venues", "v2", vec![("score", Literal::Int(30))], None).await;
        insert_entity(&exec, "venues", "v3", vec![("score", Literal::Int(20))], None).await;

        let result = exec.execute(&Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            temporal: None,
            order_by: vec![trondb_tql::OrderByClause {
                field: "score".into(),
                direction: trondb_tql::SortDirection::Desc,
            }],
            limit: Some(2),
            strategy: FetchStrategy::FullScan,
            hints: vec![],
        })).await.unwrap();

        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0].values.get("score").unwrap(), &Value::Int(30));
        assert_eq!(result.rows[1].values.get("score").unwrap(), &Value::Int(20));
    }

    #[tokio::test]
    async fn drop_collection_removes_everything() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;

        // Insert entities
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

        // Verify entities exist
        let result = exec
            .execute(&Plan::Fetch(FetchPlan {
                collection: "venues".into(),
                fields: FieldList::All,
                filter: None,
                temporal: None,
                order_by: vec![],
                limit: None,
                strategy: FetchStrategy::FullScan,
                hints: vec![],
            }))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 2);

        // DROP COLLECTION
        let result = exec
            .execute(&Plan::DropCollection(DropCollectionPlan {
                name: "venues".into(),
            }))
            .await
            .unwrap();
        assert!(result.rows[0]
            .values
            .get("result")
            .unwrap()
            .to_string()
            .contains("dropped"));

        // Verify FETCH returns CollectionNotFound
        let err = exec
            .execute(&Plan::Fetch(FetchPlan {
                collection: "venues".into(),
                fields: FieldList::All,
                filter: None,
                temporal: None,
                order_by: vec![],
                limit: None,
                strategy: FetchStrategy::FullScan,
                hints: vec![],
            }))
            .await;
        assert!(err.is_err());
        match err.unwrap_err() {
            EngineError::CollectionNotFound(name) => assert_eq!(name, "venues"),
            other => panic!("expected CollectionNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drop_edge_type_removes_edges() {
        let (exec, _dir) = setup_executor().await;

        // Create two collections
        create_collection(&exec, "people", 3).await;
        create_collection(&exec, "venues", 3).await;

        // Insert entities
        insert_entity(
            &exec,
            "people",
            "p1",
            vec![("name", Literal::String("Alice".into()))],
            Some(vec![1.0, 0.0, 0.0]),
        )
        .await;
        insert_entity(
            &exec,
            "venues",
            "v1",
            vec![("name", Literal::String("Venue A".into()))],
            Some(vec![0.0, 1.0, 0.0]),
        )
        .await;

        // Create edge type
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "visits".into(),
            from_collection: "people".into(),
            to_collection: "venues".into(),
            decay_config: None,
            inference_config: None,
        }))
        .await
        .unwrap();

        // Insert an edge
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "visits".into(),
            from_id: "p1".into(),
            to_id: "v1".into(),
            metadata: vec![],
            valid_from: None,
            valid_to: None,
        }))
        .await
        .unwrap();

        // Verify traverse works
        let result = exec
            .execute(&Plan::Traverse(TraversePlan {
                edge_type: "visits".into(),
                from_id: "p1".into(),
                depth: 1,
                limit: None,
            }))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 1);

        // DROP EDGE TYPE
        let result = exec
            .execute(&Plan::DropEdgeType(DropEdgeTypePlan {
                name: "visits".into(),
            }))
            .await
            .unwrap();
        assert!(result.rows[0]
            .values
            .get("result")
            .unwrap()
            .to_string()
            .contains("dropped"));

        // Verify traverse now returns error (edge type not found)
        let err = exec
            .execute(&Plan::Traverse(TraversePlan {
                edge_type: "visits".into(),
                from_id: "p1".into(),
                depth: 1,
                limit: None,
            }))
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn end_to_end_phase_12_query_language() {
        let (exec, _dir) = setup_executor().await;

        // Create collection with indexed field
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: "events".into(),
            representations: vec![trondb_tql::RepresentationDecl {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: trondb_tql::Metric::Cosine,
                sparse: false,
                fields: vec![],
            }],
            fields: vec![
                trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
                trondb_tql::FieldDecl { name: "category".into(), field_type: trondb_tql::FieldType::Text },
                trondb_tql::FieldDecl { name: "score".into(), field_type: trondb_tql::FieldType::Int },
            ],
            indexes: vec![trondb_tql::IndexDecl {
                name: "idx_category".into(),
                fields: vec!["category".into()],
                partial_condition: None,
            }],
            vectoriser_config: None,
        }))
        .await
        .unwrap();

        // Insert test data
        insert_entity(
            &exec,
            "events",
            "e1",
            vec![
                ("name", Literal::String("Jazz Night".into())),
                ("category", Literal::String("music".into())),
                ("score", Literal::Int(90)),
            ],
            None,
        )
        .await;
        insert_entity(
            &exec,
            "events",
            "e2",
            vec![
                ("name", Literal::String("Comedy Hour".into())),
                ("category", Literal::String("comedy".into())),
                ("score", Literal::Int(75)),
            ],
            None,
        )
        .await;
        insert_entity(
            &exec,
            "events",
            "e3",
            vec![
                ("name", Literal::String("Jazz Festival".into())),
                ("category", Literal::String("music".into())),
                ("score", Literal::Int(95)),
            ],
            None,
        )
        .await;
        insert_entity(
            &exec,
            "events",
            "e4",
            vec![
                ("name", Literal::String("Rock Concert".into())),
                ("category", Literal::String("music".into())),
                ("score", Literal::Int(80)),
            ],
            None,
        )
        .await;

        // Test 1: IN operator — all 4 entities match music or comedy
        let result = exec
            .execute(&Plan::Fetch(FetchPlan {
                collection: "events".into(),
                fields: FieldList::All,
                filter: Some(WhereClause::In(
                    "category".into(),
                    vec![
                        Literal::String("music".into()),
                        Literal::String("comedy".into()),
                    ],
                )),
                temporal: None,
                order_by: vec![],
                limit: None,
                strategy: FetchStrategy::FullScan,
                hints: vec![],
            }))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 4);

        // Test 2: LIKE + ORDER BY DESC + LIMIT
        let result = exec
            .execute(&Plan::Fetch(FetchPlan {
                collection: "events".into(),
                fields: FieldList::All,
                filter: Some(WhereClause::Like("name".into(), "Jazz%".into())),
                temporal: None,
                order_by: vec![trondb_tql::OrderByClause {
                    field: "score".into(),
                    direction: trondb_tql::SortDirection::Desc,
                }],
                limit: Some(1),
                strategy: FetchStrategy::FullScan,
                hints: vec![],
            }))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("name").unwrap(),
            &Value::String("Jazz Festival".into())
        );

        // Test 3: NOT + != combined — exclude comedy AND names starting with Rock
        let result = exec
            .execute(&Plan::Fetch(FetchPlan {
                collection: "events".into(),
                fields: FieldList::All,
                filter: Some(WhereClause::And(
                    Box::new(WhereClause::Neq(
                        "category".into(),
                        Literal::String("comedy".into()),
                    )),
                    Box::new(WhereClause::Not(Box::new(WhereClause::Like(
                        "name".into(),
                        "Rock%".into(),
                    )))),
                )),
                temporal: None,
                order_by: vec![],
                limit: None,
                strategy: FetchStrategy::FullScan,
                hints: vec![],
            }))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 2); // Jazz Night + Jazz Festival

        // Test 4: DROP COLLECTION
        exec.execute(&Plan::DropCollection(DropCollectionPlan {
            name: "events".into(),
        }))
        .await
        .unwrap();

        // Verify collection is gone — FETCH should error
        let err = exec
            .execute(&Plan::Fetch(FetchPlan {
                collection: "events".into(),
                fields: FieldList::All,
                filter: None,
                temporal: None,
                order_by: vec![],
                limit: None,
                strategy: FetchStrategy::FullScan,
                hints: vec![],
            }))
            .await;
        assert!(err.is_err());
    }

    // -----------------------------------------------------------------------
    // JOIN tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn join_inner_structural() {
        let (exec, _dir) = setup_executor().await;

        // Create collections
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
            fields: vec![
                trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
                trondb_tql::FieldDecl { name: "venue_id".into(), field_type: trondb_tql::FieldType::Text },
            ],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

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
            fields: vec![
                trondb_tql::FieldDecl { name: "address".into(), field_type: trondb_tql::FieldType::Text },
            ],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

        // Insert venue
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["id".into(), "address".into()],
            values: vec![Literal::String("v1".into()), Literal::String("123 Main St".into())],
            vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
            collocate_with: None,
            affinity_group: None,
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // Insert person referencing venue
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "people".into(),
            fields: vec!["id".into(), "name".into(), "venue_id".into()],
            values: vec![
                Literal::String("p1".into()),
                Literal::String("Alice".into()),
                Literal::String("v1".into()),
            ],
            vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![0.0, 1.0, 0.0]))],
            collocate_with: None,
            affinity_group: None,
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // Execute INNER JOIN
        use trondb_tql::{JoinClause, JoinType, QualifiedField};
        let result = exec.execute(&Plan::Join(JoinPlan {
            fields: trondb_tql::JoinFieldList::Named(vec![
                QualifiedField { alias: "p".into(), field: "name".into() },
                QualifiedField { alias: "v".into(), field: "address".into() },
            ]),
            from_collection: "people".into(),
            from_alias: "p".into(),
            joins: vec![JoinClause {
                join_type: JoinType::Inner,
                collection: "venues".into(),
                alias: "v".into(),
                on_left: QualifiedField { alias: "p".into(), field: "venue_id".into() },
                on_right: QualifiedField { alias: "v".into(), field: "id".into() },
                confidence_threshold: None,
            }],
            filter: None,
            order_by: vec![],
            limit: None,
            hints: vec![],
        })).await.unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("p.name"),
            Some(&Value::String("Alice".into()))
        );
        assert_eq!(
            result.rows[0].values.get("v.address"),
            Some(&Value::String("123 Main St".into()))
        );
    }

    #[tokio::test]
    async fn join_inner_no_match() {
        let (exec, _dir) = setup_executor().await;

        // Create collections
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
            fields: vec![
                trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
                trondb_tql::FieldDecl { name: "venue_id".into(), field_type: trondb_tql::FieldType::Text },
            ],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

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
            fields: vec![],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

        // Insert person with non-existent venue_id
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "people".into(),
            fields: vec!["id".into(), "name".into(), "venue_id".into()],
            values: vec![
                Literal::String("p1".into()),
                Literal::String("Alice".into()),
                Literal::String("nonexistent".into()),
            ],
            vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![0.0, 1.0, 0.0]))],
            collocate_with: None,
            affinity_group: None,
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // INNER JOIN should return 0 rows (no matching venue)
        use trondb_tql::{JoinClause, JoinType, QualifiedField};
        let result = exec.execute(&Plan::Join(JoinPlan {
            fields: trondb_tql::JoinFieldList::All,
            from_collection: "people".into(),
            from_alias: "p".into(),
            joins: vec![JoinClause {
                join_type: JoinType::Inner,
                collection: "venues".into(),
                alias: "v".into(),
                on_left: QualifiedField { alias: "p".into(), field: "venue_id".into() },
                on_right: QualifiedField { alias: "v".into(), field: "id".into() },
                confidence_threshold: None,
            }],
            filter: None,
            order_by: vec![],
            limit: None,
            hints: vec![],
        })).await.unwrap();

        assert_eq!(result.rows.len(), 0);
    }

    #[tokio::test]
    async fn join_left_includes_unmatched() {
        let (exec, _dir) = setup_executor().await;

        // Create collections
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
            fields: vec![
                trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
                trondb_tql::FieldDecl { name: "venue_id".into(), field_type: trondb_tql::FieldType::Text },
            ],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

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
            fields: vec![],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

        // Insert person with no matching venue
        exec.execute(&Plan::Insert(InsertPlan {
            collection: "people".into(),
            fields: vec!["id".into(), "name".into(), "venue_id".into()],
            values: vec![
                Literal::String("p1".into()),
                Literal::String("Alice".into()),
                Literal::String("nonexistent".into()),
            ],
            vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![0.0, 1.0, 0.0]))],
            collocate_with: None,
            affinity_group: None,
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // LEFT JOIN should return 1 row with NULLs for venue fields
        use trondb_tql::{JoinClause, JoinType, QualifiedField};
        let result = exec.execute(&Plan::Join(JoinPlan {
            fields: trondb_tql::JoinFieldList::Named(vec![
                QualifiedField { alias: "p".into(), field: "name".into() },
                QualifiedField { alias: "v".into(), field: "id".into() },
            ]),
            from_collection: "people".into(),
            from_alias: "p".into(),
            joins: vec![JoinClause {
                join_type: JoinType::Left,
                collection: "venues".into(),
                alias: "v".into(),
                on_left: QualifiedField { alias: "p".into(), field: "venue_id".into() },
                on_right: QualifiedField { alias: "v".into(), field: "id".into() },
                confidence_threshold: None,
            }],
            filter: None,
            order_by: vec![],
            limit: None,
            hints: vec![],
        })).await.unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].values.get("v.id"), Some(&Value::Null));
    }

    // -----------------------------------------------------------------------
    // TRAVERSE MATCH tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn traverse_match_forward() {
        let (exec, _dir) = setup_executor().await;

        // Create people collection
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
            fields: vec![
                trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
            ],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

        // Insert people a -> b -> c
        for (id, name) in &[("a", "Alice"), ("b", "Bob"), ("c", "Charlie")] {
            exec.execute(&Plan::Insert(InsertPlan {
                collection: "people".into(),
                fields: vec!["id".into(), "name".into()],
                values: vec![Literal::String(id.to_string()), Literal::String(name.to_string())],
                vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
                collocate_with: None,
                affinity_group: None,
                valid_from: None,
                valid_to: None,
            })).await.unwrap();
        }

        // Create edge type
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        // Insert edges: a->b, b->c
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(),
            from_id: "a".into(),
            to_id: "b".into(),
            metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(),
            from_id: "b".into(),
            to_id: "c".into(),
            metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // TRAVERSE MATCH from a, depth 1..2 — should get b (depth 1) and c (depth 2)
        use trondb_tql::{MatchPattern, EdgePattern, EdgeDirection};
        let result = exec.execute(&Plan::TraverseMatch(TraverseMatchPlan {
            from_id: "a".into(),
            pattern: MatchPattern {
                source_var: "a".into(),
                edge: EdgePattern {
                    variable: Some("e".into()),
                    edge_type: Some("knows".into()),
                    direction: EdgeDirection::Forward,
                },
                target_var: "b".into(),
            },
            min_depth: 1,
            max_depth: 2,
            confidence_threshold: None,
            temporal: None,
            limit: None,
        })).await.unwrap();

        assert_eq!(result.rows.len(), 2);
        // Both should have _edge.confidence and _depth
        for row in &result.rows {
            assert!(row.values.contains_key("_edge.confidence"));
            assert!(row.values.contains_key("_depth"));
        }
    }

    #[tokio::test]
    async fn traverse_match_min_depth_filter() {
        let (exec, _dir) = setup_executor().await;

        // Same setup: a -> b -> c
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
            fields: vec![
                trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
            ],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

        for (id, name) in &[("a", "Alice"), ("b", "Bob"), ("c", "Charlie")] {
            exec.execute(&Plan::Insert(InsertPlan {
                collection: "people".into(),
                fields: vec!["id".into(), "name".into()],
                values: vec![Literal::String(id.to_string()), Literal::String(name.to_string())],
                vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
                collocate_with: None,
                affinity_group: None,
                valid_from: None,
                valid_to: None,
            })).await.unwrap();
        }

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(), from_id: "a".into(), to_id: "b".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(), from_id: "b".into(), to_id: "c".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // TRAVERSE MATCH from a, depth 2..3 — should only get c (depth 2), not b (depth 1)
        use trondb_tql::{MatchPattern, EdgePattern, EdgeDirection};
        let result = exec.execute(&Plan::TraverseMatch(TraverseMatchPlan {
            from_id: "a".into(),
            pattern: MatchPattern {
                source_var: "a".into(),
                edge: EdgePattern {
                    variable: Some("e".into()),
                    edge_type: Some("knows".into()),
                    direction: EdgeDirection::Forward,
                },
                target_var: "b".into(),
            },
            min_depth: 2,
            max_depth: 3,
            confidence_threshold: None,
            temporal: None,
            limit: None,
        })).await.unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("name"),
            Some(&Value::String("Charlie".into()))
        );
    }

    #[tokio::test]
    async fn traverse_match_backward() {
        let (exec, _dir) = setup_executor().await;

        // a -> b -> c, traverse backward from c
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
            fields: vec![
                trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
            ],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

        for (id, name) in &[("a", "Alice"), ("b", "Bob"), ("c", "Charlie")] {
            exec.execute(&Plan::Insert(InsertPlan {
                collection: "people".into(),
                fields: vec!["id".into(), "name".into()],
                values: vec![Literal::String(id.to_string()), Literal::String(name.to_string())],
                vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
                collocate_with: None,
                affinity_group: None,
                valid_from: None,
                valid_to: None,
            })).await.unwrap();
        }

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(), from_id: "a".into(), to_id: "b".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(), from_id: "b".into(), to_id: "c".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // Backward traversal from c — should find b (depth 1) and a (depth 2)
        use trondb_tql::{MatchPattern, EdgePattern, EdgeDirection};
        let result = exec.execute(&Plan::TraverseMatch(TraverseMatchPlan {
            from_id: "c".into(),
            pattern: MatchPattern {
                source_var: "a".into(),
                edge: EdgePattern {
                    variable: None,
                    edge_type: Some("knows".into()),
                    direction: EdgeDirection::Backward,
                },
                target_var: "b".into(),
            },
            min_depth: 1,
            max_depth: 2,
            confidence_threshold: None,
            temporal: None,
            limit: None,
        })).await.unwrap();

        assert_eq!(result.rows.len(), 2);
        // Verify we got Bob and Alice
        let names: Vec<&Value> = result.rows.iter()
            .filter_map(|r| r.values.get("name"))
            .collect();
        assert!(names.contains(&&Value::String("Bob".into())));
        assert!(names.contains(&&Value::String("Alice".into())));
    }

    #[tokio::test]
    async fn traverse_match_undirected() {
        let (exec, _dir) = setup_executor().await;

        // a -> b -> c, undirected from b — should find both a and c
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
            fields: vec![
                trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
            ],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

        for (id, name) in &[("a", "Alice"), ("b", "Bob"), ("c", "Charlie")] {
            exec.execute(&Plan::Insert(InsertPlan {
                collection: "people".into(),
                fields: vec!["id".into(), "name".into()],
                values: vec![Literal::String(id.to_string()), Literal::String(name.to_string())],
                vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
                collocate_with: None,
                affinity_group: None,
                valid_from: None,
                valid_to: None,
            })).await.unwrap();
        }

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(), from_id: "a".into(), to_id: "b".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(), from_id: "b".into(), to_id: "c".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // Undirected traversal from b, depth 1 — should find a and c
        use trondb_tql::{MatchPattern, EdgePattern, EdgeDirection};
        let result = exec.execute(&Plan::TraverseMatch(TraverseMatchPlan {
            from_id: "b".into(),
            pattern: MatchPattern {
                source_var: "x".into(),
                edge: EdgePattern {
                    variable: None,
                    edge_type: Some("knows".into()),
                    direction: EdgeDirection::Undirected,
                },
                target_var: "y".into(),
            },
            min_depth: 1,
            max_depth: 1,
            confidence_threshold: None,
            temporal: None,
            limit: None,
        })).await.unwrap();

        assert_eq!(result.rows.len(), 2);
        let names: Vec<&Value> = result.rows.iter()
            .filter_map(|r| r.values.get("name"))
            .collect();
        assert!(names.contains(&&Value::String("Alice".into())));
        assert!(names.contains(&&Value::String("Charlie".into())));
    }

    #[tokio::test]
    async fn traverse_match_cycle_detection() {
        let (exec, _dir) = setup_executor().await;

        // a -> b -> c -> a (cycle)
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
            fields: vec![
                trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
            ],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

        for (id, name) in &[("a", "Alice"), ("b", "Bob"), ("c", "Charlie")] {
            exec.execute(&Plan::Insert(InsertPlan {
                collection: "people".into(),
                fields: vec!["id".into(), "name".into()],
                values: vec![Literal::String(id.to_string()), Literal::String(name.to_string())],
                vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
                collocate_with: None,
                affinity_group: None,
                valid_from: None,
                valid_to: None,
            })).await.unwrap();
        }

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(), from_id: "a".into(), to_id: "b".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(), from_id: "b".into(), to_id: "c".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(), from_id: "c".into(), to_id: "a".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // Traverse with large depth — cycle detection should prevent infinite loop
        use trondb_tql::{MatchPattern, EdgePattern, EdgeDirection};
        let result = exec.execute(&Plan::TraverseMatch(TraverseMatchPlan {
            from_id: "a".into(),
            pattern: MatchPattern {
                source_var: "x".into(),
                edge: EdgePattern {
                    variable: None,
                    edge_type: Some("knows".into()),
                    direction: EdgeDirection::Forward,
                },
                target_var: "y".into(),
            },
            min_depth: 1,
            max_depth: 10,
            confidence_threshold: None,
            temporal: None,
            limit: None,
        })).await.unwrap();

        // Should find exactly b and c (not revisit a or infinite loop)
        assert_eq!(result.rows.len(), 2);
    }

    #[tokio::test]
    async fn traverse_match_limit() {
        let (exec, _dir) = setup_executor().await;

        // a -> b -> c
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
            fields: vec![
                trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
            ],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

        for (id, name) in &[("a", "Alice"), ("b", "Bob"), ("c", "Charlie")] {
            exec.execute(&Plan::Insert(InsertPlan {
                collection: "people".into(),
                fields: vec!["id".into(), "name".into()],
                values: vec![Literal::String(id.to_string()), Literal::String(name.to_string())],
                vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
                collocate_with: None,
                affinity_group: None,
                valid_from: None,
                valid_to: None,
            })).await.unwrap();
        }

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(), from_id: "a".into(), to_id: "b".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(), from_id: "b".into(), to_id: "c".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // Limit to 1 result
        use trondb_tql::{MatchPattern, EdgePattern, EdgeDirection};
        let result = exec.execute(&Plan::TraverseMatch(TraverseMatchPlan {
            from_id: "a".into(),
            pattern: MatchPattern {
                source_var: "x".into(),
                edge: EdgePattern {
                    variable: None,
                    edge_type: Some("knows".into()),
                    direction: EdgeDirection::Forward,
                },
                target_var: "y".into(),
            },
            min_depth: 1,
            max_depth: 5,
            confidence_threshold: None,
            temporal: None,
            limit: Some(1),
        })).await.unwrap();

        assert_eq!(result.rows.len(), 1);
    }

    #[tokio::test]
    async fn traverse_match_no_edge_type_filter() {
        let (exec, _dir) = setup_executor().await;

        // a -> b via "knows", a -> c via "likes"
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
            fields: vec![
                trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
            ],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

        for (id, name) in &[("a", "Alice"), ("b", "Bob"), ("c", "Charlie")] {
            exec.execute(&Plan::Insert(InsertPlan {
                collection: "people".into(),
                fields: vec!["id".into(), "name".into()],
                values: vec![Literal::String(id.to_string()), Literal::String(name.to_string())],
                vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
                collocate_with: None,
                affinity_group: None,
                valid_from: None,
                valid_to: None,
            })).await.unwrap();
        }

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "likes".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(), from_id: "a".into(), to_id: "b".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "likes".into(), from_id: "a".into(), to_id: "c".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // No edge type filter — should traverse both types and find b + c
        use trondb_tql::{MatchPattern, EdgePattern, EdgeDirection};
        let result = exec.execute(&Plan::TraverseMatch(TraverseMatchPlan {
            from_id: "a".into(),
            pattern: MatchPattern {
                source_var: "x".into(),
                edge: EdgePattern {
                    variable: None,
                    edge_type: None, // no filter
                    direction: EdgeDirection::Forward,
                },
                target_var: "y".into(),
            },
            min_depth: 1,
            max_depth: 1,
            confidence_threshold: None,
            temporal: None,
            limit: None,
        })).await.unwrap();

        assert_eq!(result.rows.len(), 2);
        let names: Vec<&Value> = result.rows.iter()
            .filter_map(|r| r.values.get("name"))
            .collect();
        assert!(names.contains(&&Value::String("Bob".into())));
        assert!(names.contains(&&Value::String("Charlie".into())));
    }

    #[tokio::test]
    async fn traverse_match_edge_type_metadata() {
        let (exec, _dir) = setup_executor().await;

        // Verify _edge.confidence and _depth are correctly reported
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
            fields: vec![
                trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
            ],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

        for (id, name) in &[("a", "Alice"), ("b", "Bob")] {
            exec.execute(&Plan::Insert(InsertPlan {
                collection: "people".into(),
                fields: vec!["id".into(), "name".into()],
                values: vec![Literal::String(id.to_string()), Literal::String(name.to_string())],
                vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
                collocate_with: None,
                affinity_group: None,
                valid_from: None,
                valid_to: None,
            })).await.unwrap();
        }

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(), from_id: "a".into(), to_id: "b".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        use trondb_tql::{MatchPattern, EdgePattern, EdgeDirection};
        let result = exec.execute(&Plan::TraverseMatch(TraverseMatchPlan {
            from_id: "a".into(),
            pattern: MatchPattern {
                source_var: "x".into(),
                edge: EdgePattern {
                    variable: Some("e".into()),
                    edge_type: Some("knows".into()),
                    direction: EdgeDirection::Forward,
                },
                target_var: "y".into(),
            },
            min_depth: 1,
            max_depth: 1,
            confidence_threshold: None,
            temporal: None,
            limit: None,
        })).await.unwrap();

        assert_eq!(result.rows.len(), 1);
        // Structural edges have confidence 1.0
        assert_eq!(
            result.rows[0].values.get("_edge.confidence"),
            Some(&Value::Float(1.0))
        );
        assert_eq!(
            result.rows[0].values.get("_depth"),
            Some(&Value::Int(1))
        );
        assert_eq!(
            result.rows[0].values.get("_edge.type"),
            Some(&Value::String("knows".into()))
        );
    }

    #[tokio::test]
    async fn traverse_match_existing_traverse_unchanged() {
        // Verify backward compat: the existing Plan::Traverse still works exactly as before
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
            fields: vec![
                trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
            ],
            indexes: vec![],
            vectoriser_config: None,
        })).await.unwrap();

        for (id, name) in &[("a", "Alice"), ("b", "Bob")] {
            exec.execute(&Plan::Insert(InsertPlan {
                collection: "people".into(),
                fields: vec!["id".into(), "name".into()],
                values: vec![Literal::String(id.to_string()), Literal::String(name.to_string())],
                vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
                collocate_with: None,
                affinity_group: None,
                valid_from: None,
                valid_to: None,
            })).await.unwrap();
        }

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "knows".into(),
            from_collection: "people".into(),
            to_collection: "people".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "knows".into(), from_id: "a".into(), to_id: "b".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // Use the old Plan::Traverse — should still work
        let result = exec.execute(&Plan::Traverse(TraversePlan {
            edge_type: "knows".into(),
            from_id: "a".into(),
            depth: 1,
            limit: None,
        })).await.unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("name"),
            Some(&Value::String("Bob".into()))
        );
    }

    // -----------------------------------------------------------------------
    // End-to-End Integration Tests (TQL parse → plan → execute)
    // -----------------------------------------------------------------------

    /// Helper: create a collection with fields (using direct Plan execution).
    async fn create_collection_with_fields(
        exec: &Executor,
        name: &str,
        dims: usize,
        fields: Vec<trondb_tql::FieldDecl>,
    ) {
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
            fields,
            indexes: vec![],
            vectoriser_config: None,
        }))
        .await
        .unwrap();
    }

    /// Helper: insert an entity with metadata and optional vector.
    async fn insert_entity_full(
        exec: &Executor,
        collection: &str,
        id: &str,
        metadata: Vec<(&str, Literal)>,
        valid_from: Option<String>,
        valid_to: Option<String>,
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
            vectors: vec![("default".into(), trondb_tql::VectorLiteral::Dense(vec![1.0, 0.0, 0.0]))],
            collocate_with: None,
            affinity_group: None,
            valid_from: None,
            valid_to: None,
        }))
        .await
        .unwrap();
    }

    /// Comprehensive end-to-end test: sets up two collections with edges,
    /// then runs TQL strings through parse → plan → execute for JOIN and
    /// TRAVERSE MATCH operations.
    #[tokio::test]
    async fn e2e_join_and_traverse_match() {
        let (exec, _dir) = setup_executor().await;

        // ---------------------------------------------------------------
        // 1. Create two collections: "artists" and "venues"
        // ---------------------------------------------------------------
        create_collection_with_fields(
            &exec,
            "artists",
            3,
            vec![
                trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
                trondb_tql::FieldDecl { name: "genre".into(), field_type: trondb_tql::FieldType::Text },
                trondb_tql::FieldDecl { name: "venue_id".into(), field_type: trondb_tql::FieldType::Text },
            ],
        ).await;

        create_collection_with_fields(
            &exec,
            "venues",
            3,
            vec![
                trondb_tql::FieldDecl { name: "address".into(), field_type: trondb_tql::FieldType::Text },
                trondb_tql::FieldDecl { name: "capacity".into(), field_type: trondb_tql::FieldType::Int },
            ],
        ).await;

        // ---------------------------------------------------------------
        // 2. Insert entities in both collections
        // ---------------------------------------------------------------
        // Venues
        insert_entity_full(&exec, "venues", "v1",
            vec![("address", Literal::String("100 Main St".into())), ("capacity", Literal::Int(500))], None, None).await;
        insert_entity_full(&exec, "venues", "v2",
            vec![("address", Literal::String("200 Oak Ave".into())), ("capacity", Literal::Int(1000))], None, None).await;

        // Artists: a1 and a2 have venue_ids, a3 has no venue
        insert_entity_full(&exec, "artists", "a1",
            vec![("name", Literal::String("Alice".into())), ("genre", Literal::String("jazz".into())), ("venue_id", Literal::String("v1".into()))], None, None).await;
        insert_entity_full(&exec, "artists", "a2",
            vec![("name", Literal::String("Bob".into())), ("genre", Literal::String("rock".into())), ("venue_id", Literal::String("v2".into()))], None, None).await;
        insert_entity_full(&exec, "artists", "a3",
            vec![("name", Literal::String("Charlie".into())), ("genre", Literal::String("blues".into())), ("venue_id", Literal::String("nonexistent".into()))], None, None).await;

        // ---------------------------------------------------------------
        // 3. Create an edge type linking artists to venues
        // ---------------------------------------------------------------
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "performs_at".into(),
            from_collection: "artists".into(),
            to_collection: "venues".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        // ---------------------------------------------------------------
        // 4. Insert structural edges (a1 -> v1, a2 -> v2)
        // ---------------------------------------------------------------
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "performs_at".into(), from_id: "a1".into(), to_id: "v1".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "performs_at".into(), from_id: "a2".into(), to_id: "v2".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // ---------------------------------------------------------------
        // 5. Test INNER JOIN across the edge (structural, field-based)
        // ---------------------------------------------------------------
        {
            let stmt = trondb_tql::parse(
                "FETCH a.name, v.address FROM artists AS a INNER JOIN venues AS v ON a.venue_id = v.id;"
            ).unwrap();
            let p = crate::planner::plan(&stmt, &exec.schemas).unwrap();
            assert!(matches!(p, Plan::Join(_)));

            let result = exec.execute(&p).await.unwrap();
            // a1 matches v1, a2 matches v2; a3 has no match so excluded by INNER
            assert_eq!(result.rows.len(), 2);

            let names: Vec<&Value> = result.rows.iter()
                .filter_map(|r| r.values.get("a.name"))
                .collect();
            assert!(names.contains(&&Value::String("Alice".into())));
            assert!(names.contains(&&Value::String("Bob".into())));

            // Verify venue addresses are present
            let addresses: Vec<&Value> = result.rows.iter()
                .filter_map(|r| r.values.get("v.address"))
                .collect();
            assert!(addresses.contains(&&Value::String("100 Main St".into())));
            assert!(addresses.contains(&&Value::String("200 Oak Ave".into())));
        }

        // ---------------------------------------------------------------
        // 6. Test LEFT JOIN (Charlie has no matching venue)
        // ---------------------------------------------------------------
        {
            let stmt = trondb_tql::parse(
                "FETCH a.name, v.address FROM artists AS a LEFT JOIN venues AS v ON a.venue_id = v.id;"
            ).unwrap();
            let p = crate::planner::plan(&stmt, &exec.schemas).unwrap();
            assert!(matches!(p, Plan::Join(_)));

            let result = exec.execute(&p).await.unwrap();
            // All 3 artists should appear; a3 has NULL venue fields
            assert_eq!(result.rows.len(), 3);

            // Find Charlie's row
            let charlie_row = result.rows.iter()
                .find(|r| r.values.get("a.name") == Some(&Value::String("Charlie".into())))
                .expect("Charlie should be in LEFT JOIN results");
            // Venue address should be Null for unmatched
            assert_eq!(charlie_row.values.get("v.address"), Some(&Value::Null));
        }

        // ---------------------------------------------------------------
        // 7. Test TRAVERSE MATCH forward with depth range
        // ---------------------------------------------------------------
        // Create a self-referencing edge type for artist chain: a1 -> a2 -> a3
        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "collaborates".into(),
            from_collection: "artists".into(),
            to_collection: "artists".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "collaborates".into(), from_id: "a1".into(), to_id: "a2".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "collaborates".into(), from_id: "a2".into(), to_id: "a3".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        {
            let stmt = trondb_tql::parse(
                "TRAVERSE FROM 'a1' MATCH (x)-[e:collaborates]->(y) DEPTH 1..2;"
            ).unwrap();
            let p = crate::planner::plan(&stmt, &exec.schemas).unwrap();
            assert!(matches!(p, Plan::TraverseMatch(_)));

            let result = exec.execute(&p).await.unwrap();
            // Depth 1: a2, Depth 2: a3
            assert_eq!(result.rows.len(), 2);

            // Verify depth metadata is present
            for row in &result.rows {
                assert!(row.values.contains_key("_depth"));
                assert!(row.values.contains_key("_edge.confidence"));
                assert!(row.values.contains_key("_edge.type"));
            }

            // Verify names found
            let names: Vec<&Value> = result.rows.iter()
                .filter_map(|r| r.values.get("name"))
                .collect();
            assert!(names.contains(&&Value::String("Bob".into())));
            assert!(names.contains(&&Value::String("Charlie".into())));
        }

        // ---------------------------------------------------------------
        // 8. Test TRAVERSE MATCH with confidence threshold
        //    Structural edges have confidence=1.0, so threshold of 0.5
        //    should still return results, but threshold of 1.5 should not.
        // ---------------------------------------------------------------
        {
            let stmt = trondb_tql::parse(
                "TRAVERSE FROM 'a1' MATCH (x)-[e:collaborates]->(y) DEPTH 1..2 CONFIDENCE > 0.5;"
            ).unwrap();
            let p = crate::planner::plan(&stmt, &exec.schemas).unwrap();
            let result = exec.execute(&p).await.unwrap();
            // Structural edges have confidence 1.0 > 0.5, so should return both
            assert_eq!(result.rows.len(), 2);
        }
        {
            // High threshold: no edges pass
            let stmt = trondb_tql::parse(
                "TRAVERSE FROM 'a1' MATCH (x)-[e:collaborates]->(y) DEPTH 1..1 CONFIDENCE > 1.5;"
            ).unwrap();
            let p = crate::planner::plan(&stmt, &exec.schemas).unwrap();
            let result = exec.execute(&p).await.unwrap();
            assert_eq!(result.rows.len(), 0);
        }

        // ---------------------------------------------------------------
        // 9. Verify backward compatibility of legacy TRAVERSE
        // ---------------------------------------------------------------
        {
            let stmt = trondb_tql::parse(
                "TRAVERSE collaborates FROM 'a1' DEPTH 2;"
            ).unwrap();
            let p = crate::planner::plan(&stmt, &exec.schemas).unwrap();
            // Legacy TRAVERSE should produce Plan::Traverse (not TraverseMatch)
            assert!(matches!(p, Plan::Traverse(_)));

            let result = exec.execute(&p).await.unwrap();
            // Legacy traverse from a1 depth 2: a2 (hop 1), a3 (hop 2)
            assert_eq!(result.rows.len(), 2);
            let names: Vec<&Value> = result.rows.iter()
                .filter_map(|r| r.values.get("name"))
                .collect();
            assert!(names.contains(&&Value::String("Bob".into())));
            assert!(names.contains(&&Value::String("Charlie".into())));
        }
    }

    /// E2E test: INNER JOIN with WHERE filter on alias-qualified fields.
    #[tokio::test]
    async fn e2e_inner_join_with_where() {
        let (exec, _dir) = setup_executor().await;

        create_collection_with_fields(&exec, "people", 3, vec![
            trondb_tql::FieldDecl { name: "name".into(), field_type: trondb_tql::FieldType::Text },
            trondb_tql::FieldDecl { name: "venue_id".into(), field_type: trondb_tql::FieldType::Text },
        ]).await;

        create_collection_with_fields(&exec, "venues", 3, vec![
            trondb_tql::FieldDecl { name: "address".into(), field_type: trondb_tql::FieldType::Text },
        ]).await;

        insert_entity_full(&exec, "venues", "v1",
            vec![("address", Literal::String("123 Main St".into()))], None, None).await;
        insert_entity_full(&exec, "venues", "v2",
            vec![("address", Literal::String("456 Oak Ave".into()))], None, None).await;

        insert_entity_full(&exec, "people", "p1",
            vec![("name", Literal::String("Alice".into())), ("venue_id", Literal::String("v1".into()))], None, None).await;
        insert_entity_full(&exec, "people", "p2",
            vec![("name", Literal::String("Bob".into())), ("venue_id", Literal::String("v2".into()))], None, None).await;

        // INNER JOIN with WHERE filtering to only Alice's row
        let stmt = trondb_tql::parse(
            "FETCH p.name, v.address FROM people AS p INNER JOIN venues AS v ON p.venue_id = v.id WHERE p.name = 'Alice';"
        ).unwrap();
        let p = crate::planner::plan(&stmt, &exec.schemas).unwrap();
        let result = exec.execute(&p).await.unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("p.name"),
            Some(&Value::String("Alice".into()))
        );
        assert_eq!(
            result.rows[0].values.get("v.address"),
            Some(&Value::String("123 Main St".into()))
        );
    }

    /// E2E test: TRAVERSE MATCH with LIMIT.
    #[tokio::test]
    async fn e2e_traverse_match_with_limit() {
        let (exec, _dir) = setup_executor().await;

        create_collection_with_fields(&exec, "nodes", 3, vec![
            trondb_tql::FieldDecl { name: "label".into(), field_type: trondb_tql::FieldType::Text },
        ]).await;

        for (id, label) in &[("n1", "Start"), ("n2", "Mid"), ("n3", "End")] {
            insert_entity_full(&exec, "nodes", id,
                vec![("label", Literal::String(label.to_string()))], None, None).await;
        }

        exec.execute(&Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "links".into(),
            from_collection: "nodes".into(),
            to_collection: "nodes".into(),
            decay_config: None,
            inference_config: None,
        })).await.unwrap();

        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "links".into(), from_id: "n1".into(), to_id: "n2".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();
        exec.execute(&Plan::InsertEdge(InsertEdgePlan {
            edge_type: "links".into(), from_id: "n2".into(), to_id: "n3".into(), metadata: vec![],
            valid_from: None,
            valid_to: None,
        })).await.unwrap();

        // TRAVERSE MATCH with LIMIT 1
        let stmt = trondb_tql::parse(
            "TRAVERSE FROM 'n1' MATCH (x)-[e:links]->(y) DEPTH 1..2 LIMIT 1;"
        ).unwrap();
        let p = crate::planner::plan(&stmt, &exec.schemas).unwrap();
        let result = exec.execute(&p).await.unwrap();

        assert_eq!(result.rows.len(), 1);
    }

    /// E2E test: wildcard SELECT in JOIN returns all fields from both sides.
    #[tokio::test]
    async fn e2e_join_wildcard_select() {
        let (exec, _dir) = setup_executor().await;

        create_collection_with_fields(&exec, "items", 3, vec![
            trondb_tql::FieldDecl { name: "title".into(), field_type: trondb_tql::FieldType::Text },
            trondb_tql::FieldDecl { name: "cat_id".into(), field_type: trondb_tql::FieldType::Text },
        ]).await;

        create_collection_with_fields(&exec, "categories", 3, vec![
            trondb_tql::FieldDecl { name: "label".into(), field_type: trondb_tql::FieldType::Text },
        ]).await;

        insert_entity_full(&exec, "categories", "c1",
            vec![("label", Literal::String("Books".into()))], None, None).await;
        insert_entity_full(&exec, "items", "i1",
            vec![("title", Literal::String("Rust in Action".into())), ("cat_id", Literal::String("c1".into()))], None, None).await;

        // FETCH * => all fields from both collections
        let stmt = trondb_tql::parse(
            "FETCH * FROM items AS i INNER JOIN categories AS c ON i.cat_id = c.id;"
        ).unwrap();
        let p = crate::planner::plan(&stmt, &exec.schemas).unwrap();
        let result = exec.execute(&p).await.unwrap();

        assert_eq!(result.rows.len(), 1);
        let row = &result.rows[0];
        // Should contain alias-qualified keys for both sides
        assert_eq!(row.values.get("i.title"), Some(&Value::String("Rust in Action".into())));
        assert_eq!(row.values.get("c.label"), Some(&Value::String("Books".into())));
        // IDs should be present
        assert!(row.values.contains_key("i.id"));
        assert!(row.values.contains_key("c.id"));
    }

    // -----------------------------------------------------------------------
    // Task 4 tests: Executor cost threading + EXPLAIN enhancement
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn explain_shows_cost_breakdown() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;

        // Insert a few entities so we have a non-zero collection size
        insert_entity(&exec, "venues", "v1", vec![("name", Literal::String("The Fleece".into()))], Some(vec![1.0, 0.0, 0.0])).await;
        insert_entity(&exec, "venues", "v2", vec![("name", Literal::String("The Exchange".into()))], Some(vec![0.0, 1.0, 0.0])).await;

        let search_plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: None,
            pre_filter: None,
            k: 10,
            confidence_threshold: 0.0,
            strategy: SearchStrategy::Hnsw,
            query_text: None,
            using_repr: None,
            hints: vec![],
            two_pass: None,
        });

        let result = exec
            .execute(&Plan::Explain(Box::new(search_plan)))
            .await
            .unwrap();

        // Should have cost-related rows in the EXPLAIN output
        let has_cost_row = result.rows.iter().any(|r| {
            r.values.get("property").map(|v| v.to_string()) == Some("estimated_acu".to_string())
        });
        assert!(has_cost_row, "EXPLAIN should include estimated_acu");

        let has_breakdown = result.rows.iter().any(|r| {
            r.values.get("property").map(|v| v.to_string()) == Some("cost_breakdown".to_string())
        });
        assert!(has_breakdown, "EXPLAIN should include cost_breakdown");
    }

    #[tokio::test]
    async fn max_acu_rejects_expensive_query() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;

        // Insert enough entities that a full scan is expensive
        for i in 0..50 {
            insert_entity(
                &exec,
                "venues",
                &format!("v{i}"),
                vec![("name", Literal::String(format!("Venue {i}")))],
                Some(vec![1.0, 0.0, 0.0]),
            )
            .await;
        }

        let plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![trondb_tql::QueryHint::MaxAcu(5.0)], // budget of 5 ACU, but scan will cost 50*0.5=25 ACU
        });
        let result = exec.execute(&plan).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            EngineError::AcuBudgetExceeded { .. } => {}
            other => panic!("expected AcuBudgetExceeded, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn query_result_includes_cost() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;
        insert_entity(&exec, "venues", "v1", vec![("name", Literal::String("The Fleece".into()))], Some(vec![1.0, 0.0, 0.0])).await;

        let plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![],
        });
        let result = exec.execute(&plan).await.unwrap();
        assert!(result.stats.cost.is_some(), "QueryStats should include cost estimate");
        let cost = result.stats.cost.unwrap();
        assert!(cost.total_acu > 0.0, "Cost should be non-zero for a scan");
    }

    // -----------------------------------------------------------------------
    // Task 7 tests: Optimisation rules integrated into executor + EXPLAIN
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn explain_shows_optimisation_rules() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;
        insert_entity(&exec, "venues", "v1", vec![("name", Literal::String("The Fleece".into()))], Some(vec![1.0, 0.0, 0.0])).await;

        let search_plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: None,
            pre_filter: None,
            k: 10,
            confidence_threshold: 0.0,
            strategy: SearchStrategy::Hnsw,
            query_text: None,
            using_repr: None,
            hints: vec![],
            two_pass: None,
        });

        let result = exec
            .execute(&Plan::Explain(Box::new(search_plan)))
            .await
            .unwrap();

        // Should have rules_applied row
        let has_rules = result.rows.iter().any(|r| {
            r.values.get("property").map(|v| v.to_string()) == Some("rules_applied".to_string())
        });
        assert!(has_rules, "EXPLAIN should show applied optimisation rules");
    }

    #[tokio::test]
    async fn warnings_propagated_to_result() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;

        // Insert enough entities so deep TRAVERSE would trigger a warning
        insert_entity(&exec, "venues", "v1", vec![("name", Literal::String("The Fleece".into()))], Some(vec![1.0, 0.0, 0.0])).await;

        let plan = Plan::Traverse(TraversePlan {
            edge_type: "similar_to".into(),
            from_id: "v1".into(),
            depth: 8,
            limit: None,
        });

        // Execute directly -- the edge type won't exist so it will error,
        // but we can test the optimisation rules by going through EXPLAIN
        let explain_result = exec
            .execute(&Plan::Explain(Box::new(plan)))
            .await
            .unwrap();

        // Check warnings are in the EXPLAIN output
        let has_warnings = explain_result.rows.iter().any(|r| {
            r.values.get("property").map(|v| v.to_string()) == Some("warnings".to_string())
        });
        assert!(has_warnings, "EXPLAIN should show warnings for deep TRAVERSE");
    }

    // -----------------------------------------------------------------------
    // Task 9 tests: End-to-end integration test for the cost model
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn end_to_end_cost_model_flow() {
        let (exec, _dir) = setup_executor().await;
        create_collection(&exec, "venues", 3).await;

        // Insert entities
        for i in 0..20 {
            insert_entity(
                &exec,
                "venues",
                &format!("v{i}"),
                vec![("name", Literal::String(format!("Venue {i}")))],
                Some(vec![1.0, 0.0, 0.0]),
            )
            .await;
        }

        // 1. FETCH with full scan -- should have cost
        let fetch_plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![],
        });
        let result = exec.execute(&fetch_plan).await.unwrap();
        assert!(result.stats.cost.is_some());
        let cost = result.stats.cost.as_ref().unwrap();
        assert!(cost.total_acu > 0.0);

        // 2. SEARCH -- should have cost
        let search_plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: None,
            pre_filter: None,
            k: 5,
            confidence_threshold: 0.0,
            strategy: SearchStrategy::Hnsw,
            query_text: None,
            using_repr: None,
            hints: vec![],
            two_pass: None,
        });
        let result = exec.execute(&search_plan).await.unwrap();
        assert!(result.stats.cost.is_some());

        // 3. EXPLAIN shows cost breakdown
        let explain_result = exec
            .execute(&Plan::Explain(Box::new(search_plan.clone())))
            .await
            .unwrap();
        let has_acu = explain_result.rows.iter().any(|r| {
            r.values.get("property").map(|v| v.to_string()) == Some("estimated_acu".into())
        });
        assert!(has_acu);

        // 4. MAX_ACU enforcement
        let budget_plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![trondb_tql::QueryHint::MaxAcu(1.0)], // 1 ACU budget vs 20 * 0.5 = 10 ACU
        });
        let result = exec.execute(&budget_plan).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), EngineError::AcuBudgetExceeded { .. }));
    }
}
