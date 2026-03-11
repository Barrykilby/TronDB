use dashmap::DashMap;

use crate::error::EngineError;
use crate::types::CollectionSchema;
use trondb_tql::{FieldList, Literal, Statement, VectorLiteral, WhereClause};

// ---------------------------------------------------------------------------
// Strategy enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum SearchStrategy {
    Hnsw,
    Sparse,
    Hybrid,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FetchStrategy {
    FullScan,
    FieldIndexLookup(String), // index name
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
    DeleteEdge(DeleteEdgePlan),
    Traverse(TraversePlan),
    CreateAffinityGroup(CreateAffinityGroupPlan),
    AlterEntityDropAffinity(AlterEntityDropAffinityPlan),
}

#[derive(Debug, Clone)]
pub struct CreateCollectionPlan {
    pub name: String,
    pub representations: Vec<trondb_tql::RepresentationDecl>,
    pub fields: Vec<trondb_tql::FieldDecl>,
    pub indexes: Vec<trondb_tql::IndexDecl>,
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
}

#[derive(Debug, Clone)]
pub struct CreateEdgeTypePlan {
    pub name: String,
    pub from_collection: String,
    pub to_collection: String,
}

#[derive(Debug, Clone)]
pub struct InsertEdgePlan {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
    pub metadata: Vec<(String, trondb_tql::Literal)>,
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

// ---------------------------------------------------------------------------
// Strategy selection helpers
// ---------------------------------------------------------------------------

/// Determine the search strategy based on which vectors are provided.
fn select_search_strategy(
    has_dense: bool,
    has_sparse: bool,
) -> SearchStrategy {
    match (has_dense, has_sparse) {
        (true, false) => SearchStrategy::Hnsw,
        (false, true) => SearchStrategy::Sparse,
        (true, true) => SearchStrategy::Hybrid,
        (false, false) => unreachable!("parser guarantees at least one vector"),
    }
}

/// Determine the fetch strategy.
/// Returns `FieldIndexLookup` when there is an `Eq` filter on a field covered
/// by a declared index; otherwise falls back to `FullScan`.
fn select_fetch_strategy(
    filter: &Option<WhereClause>,
    schema: Option<&CollectionSchema>,
) -> FetchStrategy {
    if let (Some(WhereClause::Eq(field, _)), Some(schema)) = (filter, schema) {
        for idx in &schema.indexes {
            if idx.fields.first().map(|f| f == field).unwrap_or(false) {
                return FetchStrategy::FieldIndexLookup(idx.name.clone());
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
        WhereClause::And(left, _) => first_field_in_clause(left),
        WhereClause::Or(left, _) => first_field_in_clause(left),
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
                limit: s.limit,
                strategy,
            }))
        }

        Statement::Search(s) => {
            let has_dense = s.dense_vector.is_some();
            let has_sparse = s.sparse_vector.is_some();
            let strategy = select_search_strategy(has_dense, has_sparse);

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
            }))
        }

        Statement::CreateEdgeType(s) => Ok(Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: s.name.clone(),
            from_collection: s.from_collection.clone(),
            to_collection: s.to_collection.clone(),
        })),

        Statement::InsertEdge(s) => Ok(Plan::InsertEdge(InsertEdgePlan {
            edge_type: s.edge_type.clone(),
            from_id: s.from_id.clone(),
            to_id: s.to_id.clone(),
            metadata: s.metadata.clone(),
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
        });

        let stmt = Statement::Search(SearchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            confidence: None,
            limit: Some(10),
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
            }],
            fields: vec![],
            indexes: vec![],
        });

        let stmt = Statement::Search(SearchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            confidence: None,
            limit: Some(10),
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
        });

        let stmt = Statement::Fetch(FetchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
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
}
