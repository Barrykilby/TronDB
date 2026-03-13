use dashmap::DashMap;

use crate::edge::InferenceConfig;
use crate::error::EngineError;
use crate::types::CollectionSchema;
use trondb_tql::{FieldList, Literal, OrderByClause, QueryHint, Statement, VectorLiteral, WhereClause};

// ---------------------------------------------------------------------------
// Strategy enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum SearchStrategy {
    Hnsw,
    Sparse,
    Hybrid,
    NaturalLanguage,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FetchStrategy {
    FullScan,
    FieldIndexLookup(String), // index name
    FieldIndexRange(String),  // index name — for Gt/Lt/Gte/Lte/And range scans
}

#[derive(Debug, Clone)]
pub struct PreFilter {
    pub index_name: String,
    pub clause: WhereClause,
}

// ---------------------------------------------------------------------------
// Plan types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Plan {
    CreateCollection(CreateCollectionPlan),
    Insert(InsertPlan),
    Fetch(FetchPlan),
    Search(SearchPlan),
    Explain(Box<Plan>),
    CreateEdgeType(CreateEdgeTypePlan),
    InsertEdge(InsertEdgePlan),
    DeleteEntity(DeleteEntityPlan),
    DeleteEdge(DeleteEdgePlan),
    Traverse(TraversePlan),
    CreateAffinityGroup(CreateAffinityGroupPlan),
    AlterEntityDropAffinity(AlterEntityDropAffinityPlan),
    Demote(DemotePlan),
    Promote(PromotePlan),
    ExplainTiers(ExplainTiersPlan),
    UpdateEntity(UpdateEntityPlan),
    Infer(InferPlan),
    ConfirmEdge(ConfirmEdgePlan),
    ExplainHistory(ExplainHistoryPlan),
    DropCollection(DropCollectionPlan),
    DropEdgeType(DropEdgeTypePlan),
    Join(JoinPlan),
    TraverseMatch(TraverseMatchPlan),
    Upsert(UpsertPlan),
    Checkpoint(CheckpointPlan),
    Backup(BackupPlan),
    Restore(RestorePlan),
    AlterCollection(AlterCollectionPlan),
    Import(ImportPlan),
}

#[derive(Debug, Clone)]
pub struct CreateCollectionPlan {
    pub name: String,
    pub representations: Vec<trondb_tql::RepresentationDecl>,
    pub fields: Vec<trondb_tql::FieldDecl>,
    pub indexes: Vec<trondb_tql::IndexDecl>,
    pub vectoriser_config: Option<trondb_tql::VectoriserConfigDecl>,
}

#[derive(Debug, Clone)]
pub struct InsertPlan {
    pub collection: String,
    pub fields: Vec<String>,
    pub values: Vec<Literal>,
    pub vectors: Vec<(String, VectorLiteral)>,
    pub collocate_with: Option<Vec<String>>,
    pub affinity_group: Option<String>,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FetchPlan {
    pub collection: String,
    pub fields: FieldList,
    pub filter: Option<WhereClause>,
    pub temporal: Option<trondb_tql::TemporalClause>,
    pub order_by: Vec<OrderByClause>,
    pub limit: Option<usize>,
    pub strategy: FetchStrategy,
    pub hints: Vec<QueryHint>,
}

/// Configuration for two-pass search strategy.
/// First pass: fast shortlisting with quantised vectors.
/// Second pass: rescore survivors with full-precision vectors.
#[derive(Debug, Clone)]
pub struct TwoPassConfig {
    /// Number of candidates to shortlist in the first pass.
    /// Typically 3-5x the final k.
    pub first_pass_k: usize,
    /// Whether to use binary quantised vectors for the first pass.
    /// false = Int8 quantised, true = binary quantised.
    pub use_binary_first_pass: bool,
}

#[derive(Debug, Clone)]
pub struct SearchPlan {
    pub collection: String,
    pub fields: FieldList,
    pub dense_vector: Option<Vec<f64>>,
    pub sparse_vector: Option<Vec<(u32, f32)>>,
    pub filter: Option<WhereClause>,
    pub pre_filter: Option<PreFilter>,
    pub k: usize,
    pub confidence_threshold: f64,
    pub strategy: SearchStrategy,
    pub query_text: Option<String>,
    pub using_repr: Option<String>,
    pub hints: Vec<QueryHint>,
    pub two_pass: Option<TwoPassConfig>,
    pub within: Option<Box<TraverseMatchPlan>>,
}

#[derive(Debug, Clone)]
pub struct CreateEdgeTypePlan {
    pub name: String,
    pub from_collection: String,
    pub to_collection: String,
    pub decay_config: Option<trondb_tql::DecayConfigDecl>,
    pub inference_config: Option<InferenceConfig>,
}

#[derive(Debug, Clone)]
pub struct InsertEdgePlan {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
    pub metadata: Vec<(String, trondb_tql::Literal)>,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DeleteEntityPlan {
    pub entity_id: String,
    pub collection: String,
}

#[derive(Debug, Clone)]
pub struct DeleteEdgePlan {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
}

#[derive(Debug, Clone)]
pub struct TraversePlan {
    pub edge_type: String,
    pub from_id: String,
    pub depth: usize,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct CreateAffinityGroupPlan {
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct AlterEntityDropAffinityPlan {
    pub entity_id: String,
}

#[derive(Debug, Clone)]
pub struct DemotePlan {
    pub entity_id: String,
    pub collection: String,
    pub target_tier: trondb_tql::ast::TierTarget,
}

#[derive(Debug, Clone)]
pub struct PromotePlan {
    pub entity_id: String,
    pub collection: String,
}

#[derive(Debug, Clone)]
pub struct ExplainTiersPlan {
    pub collection: String,
}

#[derive(Debug, Clone)]
pub struct UpdateEntityPlan {
    pub entity_id: String,
    pub collection: String,
    pub assignments: Vec<(String, trondb_tql::Literal)>,
}

#[derive(Debug, Clone)]
pub struct CheckpointPlan;

#[derive(Debug, Clone)]
pub struct BackupPlan {
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct RestorePlan {
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct AlterCollectionPlan {
    pub collection: String,
    pub operation: trondb_tql::AlterCollectionOp,
}

#[derive(Debug, Clone)]
pub struct ImportPlan {
    pub collection: String,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct UpsertPlan {
    pub collection: String,
    pub fields: Vec<String>,
    pub values: Vec<Literal>,
    pub vectors: Vec<(String, VectorLiteral)>,
}

#[derive(Debug, Clone)]
pub struct InferPlan {
    pub from_id: String,
    pub edge_types: Vec<String>,
    pub limit: Option<usize>,
    pub confidence_floor: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct ConfirmEdgePlan {
    pub from_id: String,
    pub to_id: String,
    pub edge_type: String,
    pub confidence: f32,
}

#[derive(Debug, Clone)]
pub struct ExplainHistoryPlan {
    pub entity_id: String,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct DropCollectionPlan {
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct DropEdgeTypePlan {
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct TraverseMatchPlan {
    pub from_id: String,
    pub pattern: trondb_tql::MatchPattern,
    pub min_depth: usize,
    pub max_depth: usize,
    pub confidence_threshold: Option<f64>,
    pub temporal: Option<trondb_tql::TemporalClause>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct JoinPlan {
    pub fields: trondb_tql::JoinFieldList,
    pub from_collection: String,
    pub from_alias: String,
    pub joins: Vec<trondb_tql::JoinClause>,
    pub filter: Option<WhereClause>,
    pub order_by: Vec<OrderByClause>,
    pub limit: Option<usize>,
    pub hints: Vec<QueryHint>,
}

// ---------------------------------------------------------------------------
// Strategy selection helpers
// ---------------------------------------------------------------------------

/// Determine the search strategy based on which vectors are provided.
fn select_search_strategy(
    has_dense: bool,
    has_sparse: bool,
    has_query_text: bool,
) -> SearchStrategy {
    if has_query_text {
        return SearchStrategy::NaturalLanguage;
    }
    match (has_dense, has_sparse) {
        (true, false) => SearchStrategy::Hnsw,
        (false, true) => SearchStrategy::Sparse,
        (true, true) => SearchStrategy::Hybrid,
        (false, false) => unreachable!("parser guarantees at least one vector or query_text"),
    }
}

/// Determine the fetch strategy.
/// Returns `FieldIndexLookup` for `Eq` filters, `FieldIndexRange` for
/// `Gt`/`Lt`/`Gte`/`Lte`/`And` filters on indexed fields; otherwise `FullScan`.
fn select_fetch_strategy(
    filter: &Option<WhereClause>,
    schema: Option<&CollectionSchema>,
) -> FetchStrategy {
    if let (Some(clause), Some(schema)) = (filter, schema) {
        let field_name = first_field_in_clause(clause);
        for idx in &schema.indexes {
            if idx.fields.first().map(|f| f == &field_name).unwrap_or(false) {
                return match clause {
                    WhereClause::Eq(_, _) => FetchStrategy::FieldIndexLookup(idx.name.clone()),
                    WhereClause::Gt(_, _) | WhereClause::Lt(_, _)
                    | WhereClause::Gte(_, _) | WhereClause::Lte(_, _)
                    | WhereClause::And(_, _) => FetchStrategy::FieldIndexRange(idx.name.clone()),
                    _ => FetchStrategy::FullScan,
                };
            }
        }
    }
    FetchStrategy::FullScan
}

/// Determine if a pre-filter should be applied to a SEARCH WHERE clause.
/// Returns FieldNotIndexed if the referenced field is not covered by an index.
fn select_pre_filter(
    filter: &Option<WhereClause>,
    schema: Option<&CollectionSchema>,
) -> Result<Option<PreFilter>, EngineError> {
    let clause = match filter {
        Some(c) => c,
        None => return Ok(None),
    };

    let schema = match schema {
        Some(s) => s,
        None => return Ok(None), // no schema available, cannot pre-filter
    };

    // Extract the first field referenced in the WHERE clause
    let field_name = first_field_in_clause(clause);

    // Find an index that covers this field
    for idx in &schema.indexes {
        if idx.fields.contains(&field_name) {
            return Ok(Some(PreFilter {
                index_name: idx.name.clone(),
                clause: clause.clone(),
            }));
        }
    }

    // Field not indexed — error
    Err(EngineError::FieldNotIndexed(field_name))
}

/// Extract the first field name referenced in a WHERE clause.
fn first_field_in_clause(clause: &WhereClause) -> String {
    match clause {
        WhereClause::Eq(field, _) => field.clone(),
        WhereClause::Gt(field, _) => field.clone(),
        WhereClause::Lt(field, _) => field.clone(),
        WhereClause::Gte(field, _) => field.clone(),
        WhereClause::Lte(field, _) => field.clone(),
        WhereClause::And(left, _) => first_field_in_clause(left),
        WhereClause::Or(left, _) => first_field_in_clause(left),
        WhereClause::Neq(field, _) => field.clone(),
        WhereClause::Not(inner) => first_field_in_clause(inner),
        WhereClause::IsNull(field) => field.clone(),
        WhereClause::IsNotNull(field) => field.clone(),
        WhereClause::In(field, _) => field.clone(),
        WhereClause::Like(field, _) => field.clone(),
    }
}

// ---------------------------------------------------------------------------
// Planner
// ---------------------------------------------------------------------------

pub fn plan(
    stmt: &Statement,
    schemas: &DashMap<String, CollectionSchema>,
) -> Result<Plan, EngineError> {
    match stmt {
        Statement::CreateCollection(s) => Ok(Plan::CreateCollection(CreateCollectionPlan {
            name: s.name.clone(),
            representations: s.representations.clone(),
            fields: s.fields.clone(),
            indexes: s.indexes.clone(),
            vectoriser_config: s.vectoriser_config.clone(),
        })),

        Statement::Insert(s) => Ok(Plan::Insert(InsertPlan {
            collection: s.collection.clone(),
            fields: s.fields.clone(),
            values: s.values.clone(),
            vectors: s.vectors.clone(),
            collocate_with: s.collocate_with.clone(),
            affinity_group: s.affinity_group.clone(),
            valid_from: s.valid_from.clone(),
            valid_to: s.valid_to.clone(),
        })),

        Statement::Fetch(s) => {
            let schema = schemas.get(&s.collection).map(|r| r.clone());
            let strategy = if s.hints.contains(&QueryHint::ForceFullScan) {
                FetchStrategy::FullScan
            } else {
                select_fetch_strategy(&s.filter, schema.as_ref())
            };

            Ok(Plan::Fetch(FetchPlan {
                collection: s.collection.clone(),
                fields: s.fields.clone(),
                filter: s.filter.clone(),
                temporal: s.temporal.clone(),
                order_by: s.order_by.clone(),
                limit: s.limit,
                strategy,
                hints: s.hints.clone(),
            }))
        }

        Statement::Search(s) => {
            let has_dense = s.dense_vector.is_some();
            let has_sparse = s.sparse_vector.is_some();
            let has_query_text = s.query_text.is_some();
            let strategy = select_search_strategy(has_dense, has_sparse, has_query_text);

            // Validate sparse representation exists when Sparse/Hybrid selected
            if matches!(strategy, SearchStrategy::Sparse | SearchStrategy::Hybrid) {
                if let Some(schema_ref) = schemas.get(&s.collection) {
                    let has_sparse_repr = schema_ref.representations.iter().any(|r| r.sparse);
                    if !has_sparse_repr {
                        return Err(EngineError::SparseVectorRequired(s.collection.clone()));
                    }
                }
            }

            let schema = schemas.get(&s.collection).map(|r| r.clone());
            let pre_filter = if s.hints.contains(&QueryHint::NoPrefilter) {
                None
            } else {
                select_pre_filter(&s.filter, schema.as_ref())?
            };

            let k = s.limit.unwrap_or(10);
            let two_pass = if k >= 50 {
                Some(TwoPassConfig {
                    first_pass_k: k * 3,
                    use_binary_first_pass: false, // Int8 by default
                })
            } else {
                None
            };

            Ok(Plan::Search(SearchPlan {
                collection: s.collection.clone(),
                fields: s.fields.clone(),
                dense_vector: s.dense_vector.clone(),
                sparse_vector: s.sparse_vector.clone(),
                filter: s.filter.clone(),
                pre_filter,
                k,
                confidence_threshold: s.confidence.unwrap_or(0.0),
                strategy,
                query_text: s.query_text.clone(),
                using_repr: s.using_repr.clone(),
                hints: s.hints.clone(),
                two_pass,
                within: s.within.as_ref().map(|w| {
                    Box::new(TraverseMatchPlan {
                        from_id: w.from_id.clone(),
                        pattern: w.pattern.clone(),
                        min_depth: w.min_depth,
                        max_depth: w.max_depth,
                        confidence_threshold: w.confidence_threshold,
                        temporal: w.temporal.clone(),
                        limit: w.limit,
                    })
                }),
            }))
        }

        Statement::CreateEdgeType(s) => {
            let inference_config = s.inference_config.as_ref().map(|ic| {
                InferenceConfig {
                    auto: ic.auto,
                    confidence_floor: ic.confidence_floor.unwrap_or(0.5),
                    limit: ic.limit.unwrap_or(10),
                }
            });
            Ok(Plan::CreateEdgeType(CreateEdgeTypePlan {
                name: s.name.clone(),
                from_collection: s.from_collection.clone(),
                to_collection: s.to_collection.clone(),
                decay_config: s.decay_config.clone(),
                inference_config,
            }))
        }

        Statement::InsertEdge(s) => Ok(Plan::InsertEdge(InsertEdgePlan {
            edge_type: s.edge_type.clone(),
            from_id: s.from_id.clone(),
            to_id: s.to_id.clone(),
            metadata: s.metadata.clone(),
            valid_from: s.valid_from.clone(),
            valid_to: s.valid_to.clone(),
        })),

        Statement::Delete(s) => Ok(Plan::DeleteEntity(DeleteEntityPlan {
            entity_id: s.entity_id.clone(),
            collection: s.collection.clone(),
        })),

        Statement::DeleteEdge(s) => Ok(Plan::DeleteEdge(DeleteEdgePlan {
            edge_type: s.edge_type.clone(),
            from_id: s.from_id.clone(),
            to_id: s.to_id.clone(),
        })),

        Statement::Traverse(s) => Ok(Plan::Traverse(TraversePlan {
            edge_type: s.edge_type.clone(),
            from_id: s.from_id.clone(),
            depth: s.depth,
            limit: s.limit,
        })),

        Statement::Explain(inner) => {
            let inner_plan = plan(inner, schemas)?;
            Ok(Plan::Explain(Box::new(inner_plan)))
        }

        Statement::CreateAffinityGroup(s) => Ok(Plan::CreateAffinityGroup(
            CreateAffinityGroupPlan { name: s.name.clone() }
        )),

        Statement::AlterEntityDropAffinity(s) => Ok(Plan::AlterEntityDropAffinity(
            AlterEntityDropAffinityPlan { entity_id: s.entity_id.clone() }
        )),

        Statement::Demote(d) => Ok(Plan::Demote(DemotePlan {
            entity_id: d.entity_id.clone(),
            collection: d.collection.clone(),
            target_tier: d.target_tier.clone(),
        })),
        Statement::Promote(p) => Ok(Plan::Promote(PromotePlan {
            entity_id: p.entity_id.clone(),
            collection: p.collection.clone(),
        })),
        Statement::ExplainTiers(e) => Ok(Plan::ExplainTiers(ExplainTiersPlan {
            collection: e.collection.clone(),
        })),
        Statement::Update(s) => Ok(Plan::UpdateEntity(UpdateEntityPlan {
            entity_id: s.entity_id.clone(),
            collection: s.collection.clone(),
            assignments: s.assignments.clone(),
        })),

        Statement::Infer(s) => Ok(Plan::Infer(InferPlan {
            from_id: s.from_id.clone(),
            edge_types: s.edge_types.clone(),
            limit: s.limit,
            confidence_floor: s.confidence_floor,
        })),

        Statement::ConfirmEdge(s) => Ok(Plan::ConfirmEdge(ConfirmEdgePlan {
            from_id: s.from_id.clone(),
            to_id: s.to_id.clone(),
            edge_type: s.edge_type.clone(),
            confidence: s.confidence,
        })),

        Statement::ExplainHistory(s) => Ok(Plan::ExplainHistory(ExplainHistoryPlan {
            entity_id: s.entity_id.clone(),
            limit: s.limit,
        })),

        Statement::DropCollection(s) => Ok(Plan::DropCollection(DropCollectionPlan {
            name: s.name.clone(),
        })),
        Statement::DropEdgeType(s) => Ok(Plan::DropEdgeType(DropEdgeTypePlan {
            name: s.name.clone(),
        })),

        Statement::FetchJoin(s) => Ok(Plan::Join(JoinPlan {
            fields: s.fields.clone(),
            from_collection: s.from_collection.clone(),
            from_alias: s.from_alias.clone(),
            joins: s.joins.clone(),
            filter: s.filter.clone(),
            order_by: s.order_by.clone(),
            limit: s.limit,
            hints: s.hints.clone(),
        })),

        Statement::TraverseMatch(s) => Ok(Plan::TraverseMatch(TraverseMatchPlan {
            from_id: s.from_id.clone(),
            pattern: s.pattern.clone(),
            min_depth: s.min_depth,
            max_depth: s.max_depth,
            confidence_threshold: s.confidence_threshold,
            temporal: s.temporal.clone(),
            limit: s.limit,
        })),

        Statement::Checkpoint(_) => Ok(Plan::Checkpoint(CheckpointPlan)),

        Statement::Upsert(s) => Ok(Plan::Upsert(UpsertPlan {
            collection: s.collection.clone(),
            fields: s.fields.clone(),
            values: s.values.clone(),
            vectors: s.vectors.clone(),
        })),

        Statement::Backup(s) => Ok(Plan::Backup(BackupPlan { path: s.path.clone() })),
        Statement::Restore(s) => Ok(Plan::Restore(RestorePlan { path: s.path.clone() })),

        Statement::AlterCollection(s) => Ok(Plan::AlterCollection(AlterCollectionPlan {
            collection: s.collection.clone(),
            operation: s.operation.clone(),
        })),

        Statement::Import(s) => Ok(Plan::Import(ImportPlan {
            collection: s.collection.clone(),
            path: s.path.clone(),
        })),
    }
}

use crate::cost::{AcuEstimate, CostProvider};

// ---------------------------------------------------------------------------
// Cost estimation
// ---------------------------------------------------------------------------

/// Estimate the ACU cost of a plan. `collection_size` is the approximate
/// number of entities in the target collection (0 if unknown or not applicable).
pub fn estimate_plan_cost(
    plan: &Plan,
    cost: &dyn CostProvider,
    collection_size: usize,
) -> AcuEstimate {
    match plan {
        Plan::Fetch(p) => {
            let mut est = AcuEstimate::zero();
            match &p.strategy {
                FetchStrategy::FullScan => {
                    est.add("full_scan", collection_size, cost.full_scan_per_entity_acu());
                }
                FetchStrategy::FieldIndexLookup(_) => {
                    est.add("field_index_lookup", 1, cost.field_index_lookup_acu());
                }
                FetchStrategy::FieldIndexRange(_) => {
                    est.add("field_index_lookup", 1, cost.field_index_lookup_acu());
                }
            }
            est
        }
        Plan::Search(p) => {
            let mut est = AcuEstimate::zero();
            match &p.strategy {
                SearchStrategy::Hnsw => {
                    est.add("search_hnsw", 1, cost.search_hnsw_acu());
                }
                SearchStrategy::Sparse => {
                    est.add("search_sparse", 1, cost.search_sparse_acu());
                }
                SearchStrategy::Hybrid => {
                    est.add("search_hybrid", 1, cost.search_hybrid_acu());
                }
                SearchStrategy::NaturalLanguage => {
                    est.add("search_natural_language", 1, cost.search_natural_language_acu());
                }
            }
            if p.pre_filter.is_some() {
                est.add("pre_filter", 1, cost.pre_filter_acu());
            }
            if p.two_pass.is_some() {
                est.add("two_pass_rescore", 1, cost.two_pass_rescore_acu());
            }
            // Post-search: fetching k result entities from hot tier
            est.add("fetch_hot", p.k, cost.fetch_hot_acu());
            est
        }
        Plan::Traverse(p) => {
            let mut est = AcuEstimate::zero();
            est.add("traverse_hop", p.depth, cost.traverse_hop_acu());
            est
        }
        Plan::Infer(_) => {
            AcuEstimate::single("infer", 1, cost.infer_acu())
        }
        Plan::Join(p) => {
            // Join cost: full scan of left side + per-join full scan of right side
            let mut est = AcuEstimate::zero();
            est.add("full_scan", collection_size, cost.full_scan_per_entity_acu());
            for _ in &p.joins {
                est.add("join_scan", collection_size, cost.full_scan_per_entity_acu());
            }
            est
        }
        Plan::TraverseMatch(p) => {
            // Similar to Traverse: cost per hop over the depth range
            let mut est = AcuEstimate::zero();
            est.add("traverse_hop", p.max_depth, cost.traverse_hop_acu());
            est
        }
        // Write operations
        Plan::Insert(_) | Plan::CreateCollection(_) | Plan::CreateEdgeType(_)
        | Plan::InsertEdge(_) | Plan::DeleteEntity(_) | Plan::DeleteEdge(_)
        | Plan::CreateAffinityGroup(_) | Plan::AlterEntityDropAffinity(_)
        | Plan::Demote(_) | Plan::Promote(_) | Plan::UpdateEntity(_)
        | Plan::ConfirmEdge(_) | Plan::DropCollection(_) | Plan::DropEdgeType(_)
        | Plan::Upsert(_) | Plan::Backup(_) | Plan::AlterCollection(_)
        | Plan::Import(_) => {
            AcuEstimate::single("write", 1, cost.write_base_acu())
        }
        // Metadata / explain operations are free
        Plan::Explain(_) | Plan::ExplainTiers(_) | Plan::ExplainHistory(_)
        | Plan::Checkpoint(_) | Plan::Restore(_) => {
            AcuEstimate::zero()
        }
    }
}

/// Extract MAX_ACU hint from a plan's hint list.
fn extract_max_acu(plan: &Plan) -> Option<f64> {
    let hints = match plan {
        Plan::Fetch(p) => &p.hints,
        Plan::Search(p) => &p.hints,
        Plan::Join(p) => &p.hints,
        _ => return None,
    };
    hints.iter().find_map(|h| match h {
        QueryHint::MaxAcu(v) => Some(*v),
        _ => None,
    })
}

/// Check if the estimated cost exceeds the MAX_ACU budget (if set).
/// Returns Ok(()) if within budget or no budget set.
pub fn check_acu_budget(plan: &Plan, estimate: &AcuEstimate) -> Result<(), EngineError> {
    if let Some(budget) = extract_max_acu(plan) {
        if estimate.total_acu > budget {
            return Err(EngineError::AcuBudgetExceeded {
                estimated: estimate.total_acu,
                budget,
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use trondb_tql::{FetchStmt, SearchStmt};

    fn empty_schemas() -> DashMap<String, CollectionSchema> {
        DashMap::new()
    }

    #[test]
    fn plan_fetch_all() {
        let stmt = Statement::Fetch(FetchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            temporal: None,
            order_by: vec![],
            limit: Some(5),
            hints: vec![],
        });
        let p = plan(&stmt, &empty_schemas()).unwrap();
        match p {
            Plan::Fetch(fp) => {
                assert_eq!(fp.collection, "venues");
                assert_eq!(fp.fields, FieldList::All);
                assert!(fp.filter.is_none());
                assert_eq!(fp.limit, Some(5));
                assert_eq!(fp.strategy, FetchStrategy::FullScan);
            }
            _ => panic!("expected FetchPlan"),
        }
    }

    #[test]
    fn plan_search_returns_search_plan() {
        let stmt = Statement::Search(SearchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: None,
            confidence: Some(0.8),
            limit: Some(5),
            query_text: None,
            using_repr: None,
            hints: vec![],
            within: None,
        });
        let p = plan(&stmt, &empty_schemas()).unwrap();
        match p {
            Plan::Search(sp) => {
                assert_eq!(sp.collection, "venues");
                assert_eq!(sp.dense_vector, Some(vec![1.0, 0.0, 0.0]));
                assert_eq!(sp.k, 5);
                assert!((sp.confidence_threshold - 0.8).abs() < 1e-9);
                assert_eq!(sp.strategy, SearchStrategy::Hnsw);
            }
            _ => panic!("expected SearchPlan"),
        }
    }

    #[test]
    fn plan_explain_wraps_inner() {
        let stmt = Statement::Explain(Box::new(Statement::Fetch(FetchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            temporal: None,
            order_by: vec![],
            limit: None,
            hints: vec![],
        })));
        let p = plan(&stmt, &empty_schemas()).unwrap();
        match p {
            Plan::Explain(inner) => match *inner {
                Plan::Fetch(fp) => {
                    assert_eq!(fp.collection, "venues");
                    assert_eq!(fp.fields, FieldList::All);
                }
                _ => panic!("expected FetchPlan inside Explain"),
            },
            _ => panic!("expected Explain"),
        }
    }

    #[test]
    fn plan_search_sparse_strategy() {
        let stmt = Statement::Search(SearchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: None,
            sparse_vector: Some(vec![(1, 0.8), (42, 0.5)]),
            filter: None,
            confidence: None,
            limit: Some(10),
            query_text: None,
            using_repr: None,
            hints: vec![],
            within: None,
        });
        let p = plan(&stmt, &empty_schemas()).unwrap();
        match p {
            Plan::Search(sp) => {
                assert_eq!(sp.strategy, SearchStrategy::Sparse);
            }
            _ => panic!("expected SearchPlan"),
        }
    }

    #[test]
    fn plan_search_hybrid_strategy() {
        let stmt = Statement::Search(SearchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![0.1, 0.2]),
            sparse_vector: Some(vec![(1, 0.8)]),
            filter: None,
            confidence: None,
            limit: Some(10),
            query_text: None,
            using_repr: None,
            hints: vec![],
            within: None,
        });
        let p = plan(&stmt, &empty_schemas()).unwrap();
        match p {
            Plan::Search(sp) => {
                assert_eq!(sp.strategy, SearchStrategy::Hybrid);
            }
            _ => panic!("expected SearchPlan"),
        }
    }

    #[test]
    fn plan_search_where_with_indexed_field() {
        use crate::types::{Metric, StoredField, StoredIndex, StoredRepresentation, FieldType};

        let schemas = DashMap::new();
        schemas.insert("venues".into(), CollectionSchema {
            name: "venues".into(),
            representations: vec![StoredRepresentation {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: Metric::Cosine,
                sparse: false,
            fields: vec![],
            computed_at: 0,
            model_version: String::new(),
            }],
            fields: vec![StoredField {
                name: "city".into(),
                field_type: FieldType::Text,
            }],
            indexes: vec![StoredIndex {
                name: "idx_city".into(),
                fields: vec!["city".into()],
                partial_condition: None,
            }],
            vectoriser_config: None,
        });

        let stmt = Statement::Search(SearchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            confidence: None,
            limit: Some(10),
            query_text: None,
            using_repr: None,
            hints: vec![],
            within: None,
        });
        let p = plan(&stmt, &schemas).unwrap();
        match p {
            Plan::Search(sp) => {
                assert!(sp.pre_filter.is_some());
                assert_eq!(sp.pre_filter.unwrap().index_name, "idx_city");
            }
            _ => panic!("expected SearchPlan"),
        }
    }

    #[test]
    fn plan_search_where_unindexed_field_errors() {
        use crate::types::{Metric, StoredRepresentation};

        let schemas = DashMap::new();
        schemas.insert("venues".into(), CollectionSchema {
            name: "venues".into(),
            representations: vec![StoredRepresentation {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: Metric::Cosine,
                sparse: false,
            fields: vec![],
            computed_at: 0,
            model_version: String::new(),
            }],
            fields: vec![],
            indexes: vec![],
            vectoriser_config: None,
        });

        let stmt = Statement::Search(SearchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            confidence: None,
            limit: Some(10),
            query_text: None,
            using_repr: None,
            hints: vec![],
            within: None,
        });
        let result = plan(&stmt, &schemas);
        assert!(result.is_err());
        match result.unwrap_err() {
            EngineError::FieldNotIndexed(f) => assert_eq!(f, "city"),
            other => panic!("expected FieldNotIndexed, got {other:?}"),
        }
    }

    #[test]
    fn plan_fetch_with_indexed_field() {
        use crate::types::{Metric, StoredField, StoredIndex, StoredRepresentation, FieldType};

        let schemas = DashMap::new();
        schemas.insert("venues".into(), CollectionSchema {
            name: "venues".into(),
            representations: vec![StoredRepresentation {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: Metric::Cosine,
                sparse: false,
            fields: vec![],
            computed_at: 0,
            model_version: String::new(),
            }],
            fields: vec![StoredField {
                name: "city".into(),
                field_type: FieldType::Text,
            }],
            indexes: vec![StoredIndex {
                name: "idx_city".into(),
                fields: vec!["city".into()],
                partial_condition: None,
            }],
            vectoriser_config: None,
        });

        let stmt = Statement::Fetch(FetchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            temporal: None,
            order_by: vec![],
            limit: Some(10),
            hints: vec![],
        });
        let p = plan(&stmt, &schemas).unwrap();
        match p {
            Plan::Fetch(fp) => {
                assert_eq!(fp.strategy, FetchStrategy::FieldIndexLookup("idx_city".into()));
                assert_eq!(fp.collection, "venues");
            }
            _ => panic!("expected FetchPlan"),
        }
    }

    #[test]
    fn plan_fetch_gt_uses_field_index_range() {
        use crate::types::{Metric, StoredField, StoredIndex, StoredRepresentation, FieldType};

        let schemas = DashMap::new();
        schemas.insert("venues".into(), CollectionSchema {
            name: "venues".into(),
            representations: vec![StoredRepresentation {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: Metric::Cosine,
                sparse: false,
            fields: vec![],
            computed_at: 0,
            model_version: String::new(),
            }],
            fields: vec![StoredField {
                name: "score".into(),
                field_type: FieldType::Int,
            }],
            indexes: vec![StoredIndex {
                name: "idx_score".into(),
                fields: vec!["score".into()],
                partial_condition: None,
            }],
            vectoriser_config: None,
        });

        let stmt = Statement::Fetch(FetchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Gt("score".into(), Literal::Int(50))),
            temporal: None,
            order_by: vec![],
            limit: None,
            hints: vec![],
        });
        let p = plan(&stmt, &schemas).unwrap();
        match p {
            Plan::Fetch(fp) => {
                assert_eq!(fp.strategy, FetchStrategy::FieldIndexRange("idx_score".into()));
            }
            _ => panic!("expected FetchPlan"),
        }
    }

    #[test]
    fn plan_update_entity() {
        use trondb_tql::{UpdateStmt, Literal};

        let stmt = Statement::Update(UpdateStmt {
            entity_id: "v1".into(),
            collection: "venues".into(),
            assignments: vec![("name".into(), Literal::String("New".into()))],
        });
        let p = plan(&stmt, &empty_schemas()).unwrap();
        match p {
            Plan::UpdateEntity(up) => {
                assert_eq!(up.entity_id, "v1");
                assert_eq!(up.collection, "venues");
                assert_eq!(up.assignments.len(), 1);
            }
            _ => panic!("expected UpdateEntityPlan"),
        }
    }

    #[test]
    fn plan_search_natural_language_strategy() {
        let stmt = Statement::Search(SearchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: None,
            sparse_vector: None,
            filter: None,
            confidence: None,
            limit: Some(5),
            query_text: Some("live jazz in Bristol".into()),
            using_repr: Some("semantic".into()),
            hints: vec![],
            within: None,
        });
        let p = plan(&stmt, &empty_schemas()).unwrap();
        match p {
            Plan::Search(sp) => {
                assert_eq!(sp.strategy, SearchStrategy::NaturalLanguage);
                assert_eq!(sp.query_text, Some("live jazz in Bristol".into()));
                assert_eq!(sp.using_repr, Some("semantic".into()));
                assert_eq!(sp.k, 5);
            }
            _ => panic!("expected SearchPlan"),
        }
    }

    #[test]
    fn plan_fetch_gte_uses_field_index_range() {
        use crate::types::{Metric, StoredField, StoredIndex, StoredRepresentation, FieldType};

        let schemas = DashMap::new();
        schemas.insert("venues".into(), CollectionSchema {
            name: "venues".into(),
            representations: vec![StoredRepresentation {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: Metric::Cosine,
                sparse: false,
            fields: vec![],
            computed_at: 0,
            model_version: String::new(),
            }],
            fields: vec![StoredField {
                name: "score".into(),
                field_type: FieldType::Int,
            }],
            indexes: vec![StoredIndex {
                name: "idx_score".into(),
                fields: vec!["score".into()],
                partial_condition: None,
            }],
            vectoriser_config: None,
        });

        let stmt = Statement::Fetch(FetchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Gte("score".into(), Literal::Int(80))),
            temporal: None,
            order_by: vec![],
            limit: None,
            hints: vec![],
        });
        let p = plan(&stmt, &schemas).unwrap();
        match p {
            Plan::Fetch(fp) => {
                assert_eq!(fp.strategy, FetchStrategy::FieldIndexRange("idx_score".into()));
            }
            _ => panic!("expected FetchPlan"),
        }
    }

    #[test]
    fn plan_fetch_force_full_scan_overrides_index() {
        use crate::types::{Metric, StoredField, StoredIndex, StoredRepresentation, FieldType};

        let schemas = DashMap::new();
        schemas.insert("venues".into(), CollectionSchema {
            name: "venues".into(),
            representations: vec![StoredRepresentation {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: Metric::Cosine,
                sparse: false,
            fields: vec![],
            computed_at: 0,
            model_version: String::new(),
            }],
            fields: vec![StoredField {
                name: "city".into(),
                field_type: FieldType::Text,
            }],
            indexes: vec![StoredIndex {
                name: "idx_city".into(),
                fields: vec!["city".into()],
                partial_condition: None,
            }],
            vectoriser_config: None,
        });

        let stmt = Statement::Fetch(FetchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            temporal: None,
            order_by: vec![],
            limit: None,
            hints: vec![trondb_tql::QueryHint::ForceFullScan],
        });
        let p = plan(&stmt, &schemas).unwrap();
        match p {
            Plan::Fetch(fp) => {
                assert_eq!(fp.strategy, FetchStrategy::FullScan);
                assert_eq!(fp.hints, vec![trondb_tql::QueryHint::ForceFullScan]);
            }
            _ => panic!("expected FetchPlan"),
        }
    }

    #[test]
    fn plan_search_no_prefilter_suppresses_prefilter() {
        use crate::types::{Metric, StoredField, StoredIndex, StoredRepresentation, FieldType};

        let schemas = DashMap::new();
        schemas.insert("venues".into(), CollectionSchema {
            name: "venues".into(),
            representations: vec![StoredRepresentation {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: Metric::Cosine,
                sparse: false,
            fields: vec![],
            computed_at: 0,
            model_version: String::new(),
            }],
            fields: vec![StoredField {
                name: "city".into(),
                field_type: FieldType::Text,
            }],
            indexes: vec![StoredIndex {
                name: "idx_city".into(),
                fields: vec!["city".into()],
                partial_condition: None,
            }],
            vectoriser_config: None,
        });

        let stmt = Statement::Search(SearchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            confidence: None,
            limit: Some(10),
            query_text: None,
            using_repr: None,
            hints: vec![trondb_tql::QueryHint::NoPrefilter],
            within: None,
        });
        let p = plan(&stmt, &schemas).unwrap();
        match p {
            Plan::Search(sp) => {
                assert!(sp.pre_filter.is_none(), "NO_PREFILTER hint should suppress pre-filter");
                assert_eq!(sp.hints, vec![trondb_tql::QueryHint::NoPrefilter]);
            }
            _ => panic!("expected SearchPlan"),
        }
    }

    #[test]
    fn plan_search_unindexed_with_no_prefilter_succeeds() {
        use crate::types::{Metric, StoredRepresentation};

        let schemas = DashMap::new();
        schemas.insert("venues".into(), CollectionSchema {
            name: "venues".into(),
            representations: vec![StoredRepresentation {
                name: "default".into(),
                model: None,
                dimensions: Some(3),
                metric: Metric::Cosine,
                sparse: false,
            fields: vec![],
            computed_at: 0,
            model_version: String::new(),
            }],
            fields: vec![],
            indexes: vec![],
            vectoriser_config: None,
        });

        // Without the hint, this would fail with FieldNotIndexed
        let stmt = Statement::Search(SearchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            confidence: None,
            limit: Some(10),
            query_text: None,
            using_repr: None,
            hints: vec![trondb_tql::QueryHint::NoPrefilter],
            within: None,
        });
        let p = plan(&stmt, &schemas).unwrap();
        match p {
            Plan::Search(sp) => {
                assert!(sp.pre_filter.is_none());
            }
            _ => panic!("expected SearchPlan"),
        }
    }

    #[test]
    fn plan_traverse_match() {
        use trondb_tql::{TraverseMatchStmt, MatchPattern, EdgePattern, EdgeDirection};

        let stmt = Statement::TraverseMatch(TraverseMatchStmt {
            from_id: "ent_abc".into(),
            pattern: MatchPattern {
                source_var: "a".into(),
                edge: EdgePattern {
                    variable: Some("e".into()),
                    edge_type: Some("RELATED_TO".into()),
                    direction: EdgeDirection::Forward,
                },
                target_var: "b".into(),
            },
            min_depth: 1,
            max_depth: 3,
            confidence_threshold: Some(0.70),
            temporal: None,
            limit: None,
        });
        let p = plan(&stmt, &empty_schemas()).unwrap();
        match p {
            Plan::TraverseMatch(tp) => {
                assert_eq!(tp.from_id, "ent_abc");
                assert_eq!(tp.min_depth, 1);
                assert_eq!(tp.max_depth, 3);
                assert_eq!(tp.confidence_threshold, Some(0.70));
            }
            _ => panic!("expected TraverseMatchPlan"),
        }
    }

    use crate::cost::ConstantCostProvider;

    #[test]
    fn plan_join() {
        use trondb_tql::{FetchJoinStmt, JoinClause, JoinFieldList, JoinType, QualifiedField};

        let stmt = Statement::FetchJoin(FetchJoinStmt {
            fields: JoinFieldList::Named(vec![
                QualifiedField { alias: "e".into(), field: "name".into() },
                QualifiedField { alias: "v".into(), field: "address".into() },
            ]),
            from_collection: "entities".into(),
            from_alias: "e".into(),
            joins: vec![JoinClause {
                join_type: JoinType::Inner,
                collection: "venues".into(),
                alias: "v".into(),
                on_left: QualifiedField { alias: "e".into(), field: "venue_id".into() },
                on_right: QualifiedField { alias: "v".into(), field: "id".into() },
                confidence_threshold: None,
            }],
            filter: None,
            order_by: vec![],
            limit: Some(10),
            hints: vec![],
        });
        let p = plan(&stmt, &empty_schemas()).unwrap();
        match p {
            Plan::Join(jp) => {
                assert_eq!(jp.from_collection, "entities");
                assert_eq!(jp.joins.len(), 1);
                assert_eq!(jp.limit, Some(10));
            }
            _ => panic!("expected JoinPlan"),
        }
    }

    #[test]
    fn estimate_fetch_full_scan_cost() {
        let provider = ConstantCostProvider;
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
        let est = estimate_plan_cost(&plan, &provider, 1000);
        // FullScan on 1000 entities: 1000 * 0.5 = 500 ACU
        assert!((est.total_acu - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn estimate_fetch_field_index_cost() {
        let provider = ConstantCostProvider;
        let plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FieldIndexLookup("idx_city".into()),
            hints: vec![],
        });
        let est = estimate_plan_cost(&plan, &provider, 1000);
        // FieldIndexLookup: 2.0 ACU
        assert!((est.total_acu - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn estimate_search_hnsw_cost() {
        let provider = ConstantCostProvider;
        let plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: None,
            pre_filter: None,
            k: 10,
            confidence_threshold: 0.8,
            strategy: SearchStrategy::Hnsw,
            query_text: None,
            using_repr: None,
            hints: vec![],
            two_pass: None,
            within: None,
        });
        let est = estimate_plan_cost(&plan, &provider, 1000);
        // HNSW search: 50.0 + fetch k results: 10 * 1.0 = 60.0 ACU
        assert!((est.total_acu - 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn estimate_search_with_pre_filter_cost() {
        let provider = ConstantCostProvider;
        let plan = Plan::Search(SearchPlan {
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
            within: None,
        });
        let est = estimate_plan_cost(&plan, &provider, 1000);
        // HNSW: 50 + pre_filter: 5 + fetch k: 10*1 = 65 ACU
        assert!((est.total_acu - 65.0).abs() < f64::EPSILON);
    }

    #[test]
    fn estimate_traverse_cost() {
        let provider = ConstantCostProvider;
        let plan = Plan::Traverse(TraversePlan {
            edge_type: "similar_to".into(),
            from_id: "v1".into(),
            depth: 3,
            limit: None,
        });
        let est = estimate_plan_cost(&plan, &provider, 0);
        // 3 hops * 3.0 ACU = 9.0 ACU
        assert!((est.total_acu - 9.0).abs() < f64::EPSILON);
    }

    #[test]
    fn max_acu_budget_enforced() {
        let provider = ConstantCostProvider;
        let plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![QueryHint::MaxAcu(10.0)],
        });
        let est = estimate_plan_cost(&plan, &provider, 1000);
        let result = check_acu_budget(&plan, &est);
        assert!(result.is_err());
        match result.unwrap_err() {
            EngineError::AcuBudgetExceeded { estimated, budget } => {
                assert!((estimated - 500.0).abs() < f64::EPSILON);
                assert!((budget - 10.0).abs() < f64::EPSILON);
            }
            other => panic!("expected AcuBudgetExceeded, got {:?}", other),
        }
    }

    #[test]
    fn max_acu_budget_within_limit_passes() {
        let provider = ConstantCostProvider;
        let plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            temporal: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FieldIndexLookup("idx_city".into()),
            hints: vec![QueryHint::MaxAcu(10.0)],
        });
        let est = estimate_plan_cost(&plan, &provider, 1000);
        let result = check_acu_budget(&plan, &est);
        assert!(result.is_ok());
    }

    #[test]
    fn estimate_write_plans_have_base_cost() {
        let provider = ConstantCostProvider;
        let plan = Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec![],
            values: vec![],
            vectors: vec![],
            collocate_with: None,
            affinity_group: None,
            valid_from: None,
            valid_to: None,
        });
        let est = estimate_plan_cost(&plan, &provider, 0);
        assert!((est.total_acu - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn two_pass_selected_for_large_k() {
        let stmt = Statement::Search(SearchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: None,
            confidence: None,
            limit: Some(100),
            query_text: None,
            using_repr: None,
            hints: vec![],
            within: None,
        });
        let p = plan(&stmt, &empty_schemas()).unwrap();
        match p {
            Plan::Search(sp) => {
                assert!(sp.two_pass.is_some(), "two-pass should be selected for k=100");
                let tp = sp.two_pass.unwrap();
                assert_eq!(tp.first_pass_k, 300);
            }
            _ => panic!("expected SearchPlan"),
        }
    }

    #[test]
    fn two_pass_not_selected_for_small_k() {
        let stmt = Statement::Search(SearchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: None,
            confidence: None,
            limit: Some(10),
            query_text: None,
            using_repr: None,
            hints: vec![],
            within: None,
        });
        let p = plan(&stmt, &empty_schemas()).unwrap();
        match p {
            Plan::Search(sp) => {
                assert!(sp.two_pass.is_none(), "two-pass should NOT be selected for k=10");
            }
            _ => panic!("expected SearchPlan"),
        }
    }

    #[test]
    fn two_pass_adds_rescore_cost() {
        let provider = ConstantCostProvider;
        let plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: None,
            pre_filter: None,
            k: 100,
            confidence_threshold: 0.0,
            strategy: SearchStrategy::Hnsw,
            query_text: None,
            using_repr: None,
            hints: vec![],
            two_pass: Some(TwoPassConfig {
                first_pass_k: 300,
                use_binary_first_pass: false,
            }),
            within: None,
        });
        let est = estimate_plan_cost(&plan, &provider, 1000);
        // HNSW: 50 + two_pass_rescore: 15 + fetch k: 100*1.0 = 165 ACU
        assert!((est.total_acu - 165.0).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Temporal plan extension tests
    // -----------------------------------------------------------------------

    #[test]
    fn plan_fetch_with_as_of() {
        let schemas = empty_schemas();
        let stmt = trondb_tql::parse(
            "FETCH * FROM venues AS OF '2025-01-01T00:00:00Z' WHERE id = 'v1';"
        ).unwrap();
        let p = plan(&stmt, &schemas).unwrap();
        match p {
            Plan::Fetch(f) => {
                assert_eq!(f.temporal, Some(trondb_tql::TemporalClause::AsOf("2025-01-01T00:00:00Z".into())));
            }
            _ => panic!("expected Fetch plan"),
        }
    }

    #[test]
    fn plan_fetch_with_valid_during() {
        let schemas = empty_schemas();
        let stmt = trondb_tql::parse(
            "FETCH * FROM venues VALID DURING '2025-01-01'..'2025-06-30';"
        ).unwrap();
        let p = plan(&stmt, &schemas).unwrap();
        match p {
            Plan::Fetch(f) => {
                assert_eq!(f.temporal, Some(trondb_tql::TemporalClause::ValidDuring("2025-01-01".into(), "2025-06-30".into())));
            }
            _ => panic!("expected Fetch plan"),
        }
    }

    #[test]
    fn plan_fetch_with_as_of_transaction() {
        let schemas = empty_schemas();
        let stmt = trondb_tql::parse(
            "FETCH * FROM venues AS OF TRANSACTION 42891;"
        ).unwrap();
        let p = plan(&stmt, &schemas).unwrap();
        match p {
            Plan::Fetch(f) => {
                assert_eq!(f.temporal, Some(trondb_tql::TemporalClause::AsOfTransaction(42891)));
            }
            _ => panic!("expected Fetch plan"),
        }
    }

    #[test]
    fn plan_upsert() {
        let stmt = Statement::Upsert(trondb_tql::UpsertStmt {
            collection: "venues".into(),
            fields: vec!["id".into(), "name".into()],
            values: vec![Literal::String("v1".into()), Literal::String("X".into())],
            vectors: vec![],
        });
        let p = plan(&stmt, &empty_schemas()).unwrap();
        match p {
            Plan::Upsert(up) => {
                assert_eq!(up.collection, "venues");
                assert_eq!(up.fields, vec!["id", "name"]);
            }
            _ => panic!("expected UpsertPlan"),
        }
    }
}
