use crate::error::EngineError;
use trondb_tql::{FieldList, Literal, Statement, WhereClause};

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
}

#[derive(Debug, Clone)]
pub struct CreateCollectionPlan {
    pub name: String,
    pub dimensions: usize,
}

#[derive(Debug, Clone)]
pub struct InsertPlan {
    pub collection: String,
    pub fields: Vec<String>,
    pub values: Vec<Literal>,
    pub vector: Option<Vec<f64>>,
}

#[derive(Debug, Clone)]
pub struct FetchPlan {
    pub collection: String,
    pub fields: FieldList,
    pub filter: Option<WhereClause>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct SearchPlan {
    pub collection: String,
    pub query_vector: Vec<f64>,
    pub k: usize,
    pub confidence_threshold: f64,
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

// ---------------------------------------------------------------------------
// Planner
// ---------------------------------------------------------------------------

pub fn plan(stmt: &Statement) -> Result<Plan, EngineError> {
    match stmt {
        Statement::CreateCollection(s) => Ok(Plan::CreateCollection(CreateCollectionPlan {
            name: s.name.clone(),
            dimensions: s.dimensions,
        })),

        Statement::Insert(s) => Ok(Plan::Insert(InsertPlan {
            collection: s.collection.clone(),
            fields: s.fields.clone(),
            values: s.values.clone(),
            vector: s.vector.clone(),
        })),

        Statement::Fetch(s) => Ok(Plan::Fetch(FetchPlan {
            collection: s.collection.clone(),
            fields: s.fields.clone(),
            filter: s.filter.clone(),
            limit: s.limit,
        })),

        Statement::Search(s) => Ok(Plan::Search(SearchPlan {
            collection: s.collection.clone(),
            query_vector: s.near.clone(),
            k: s.limit.unwrap_or(10),
            confidence_threshold: s.confidence.unwrap_or(0.0),
        })),

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
            let inner_plan = plan(inner)?;
            Ok(Plan::Explain(Box::new(inner_plan)))
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

    #[test]
    fn plan_fetch_all() {
        let stmt = Statement::Fetch(FetchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            limit: Some(5),
        });
        let p = plan(&stmt).unwrap();
        match p {
            Plan::Fetch(fp) => {
                assert_eq!(fp.collection, "venues");
                assert_eq!(fp.fields, FieldList::All);
                assert!(fp.filter.is_none());
                assert_eq!(fp.limit, Some(5));
            }
            _ => panic!("expected FetchPlan"),
        }
    }

    #[test]
    fn plan_search_returns_search_plan() {
        let stmt = Statement::Search(SearchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            near: vec![1.0, 0.0, 0.0],
            confidence: Some(0.8),
            limit: Some(5),
        });
        let p = plan(&stmt).unwrap();
        match p {
            Plan::Search(sp) => {
                assert_eq!(sp.collection, "venues");
                assert_eq!(sp.query_vector, vec![1.0, 0.0, 0.0]);
                assert_eq!(sp.k, 5);
                assert!((sp.confidence_threshold - 0.8).abs() < 1e-9);
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
        let p = plan(&stmt).unwrap();
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
}
