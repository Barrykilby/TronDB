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
#[allow(dead_code)]
pub struct SearchPlan {
    pub collection: String,
    pub query_vector: Vec<f64>,
    pub k: usize,
    pub confidence_threshold: f64,
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

        Statement::Search(_) => Err(EngineError::UnsupportedOperation(
            "SEARCH requires vector index (Phase 4)".into(),
        )),

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
    fn plan_search_returns_unsupported() {
        let stmt = Statement::Search(SearchStmt {
            collection: "venues".into(),
            fields: FieldList::All,
            near: vec![1.0, 0.0, 0.0],
            confidence: Some(0.8),
            limit: Some(5),
        });
        let result = plan(&stmt);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, crate::error::EngineError::UnsupportedOperation(_)));
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
