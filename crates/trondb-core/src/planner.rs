use dashmap::DashMap;

use crate::edge::InferenceConfig;
use crate::error::EngineError;
use crate::types::CollectionSchema;
use trondb_tql::{FieldList, Literal, OrderByClause, Statement, VectorLiteral, WhereClause};

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
}

#[derive(Debug, Clone)]
pub struct FetchPlan {
    pub collection: String,
    pub fields: FieldList,
    pub filter: Option<WhereClause>,
    pub order_by: Vec<OrderByClause>,
    pub limit: Option<usize>,
    pub strategy: FetchStrategy,
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
        })),

        Statement::Fetch(s) => {
            let schema = schemas.get(&s.collection).map(|r| r.clone());
            let strategy = select_fetch_strategy(&s.filter, schema.as_ref());

            Ok(Plan::Fetch(FetchPlan {
                collection: s.collection.clone(),
                fields: s.fields.clone(),
                filter: s.filter.clone(),
                order_by: s.order_by.clone(),
                limit: s.limit,
                strategy,
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
            let pre_filter = select_pre_filter(&s.filter, schema.as_ref())?;

            Ok(Plan::Search(SearchPlan {
                collection: s.collection.clone(),
                fields: s.fields.clone(),
                dense_vector: s.dense_vector.clone(),
                sparse_vector: s.sparse_vector.clone(),
                filter: s.filter.clone(),
                pre_filter,
                k: s.limit.unwrap_or(10),
                confidence_threshold: s.confidence.unwrap_or(0.0),
                strategy,
                query_text: s.query_text.clone(),
                using_repr: s.using_repr.clone(),
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

        // DROP statements — plan types not yet implemented (Task 9)
        Statement::DropCollection(_) | Statement::DropEdgeType(_) => {
            Err(EngineError::InvalidQuery("DROP statements not yet implemented".into()))
        }
    }
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
            order_by: vec![],
            limit: Some(5),
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
            order_by: vec![],
            limit: None,
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
            order_by: vec![],
            limit: Some(10),
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
            order_by: vec![],
            limit: None,
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
            order_by: vec![],
            limit: None,
        });
        let p = plan(&stmt, &schemas).unwrap();
        match p {
            Plan::Fetch(fp) => {
                assert_eq!(fp.strategy, FetchStrategy::FieldIndexRange("idx_score".into()));
            }
            _ => panic!("expected FetchPlan"),
        }
    }
}
