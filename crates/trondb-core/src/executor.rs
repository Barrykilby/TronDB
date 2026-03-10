use std::collections::HashMap;
use std::time::Instant;

use dashmap::DashMap;

use crate::error::EngineError;
use crate::index::VectorIndex;
use crate::planner::{Plan, SearchPlan};
use crate::result::{QueryMode, QueryResult, QueryStats, Row};
use crate::store::Store;
use crate::types::{Entity, LogicalId, ReprState, ReprType, Representation, Value};
use trondb_tql::{FieldList, Literal, WhereClause};

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

pub struct Executor {
    store: Store,
    indexes: DashMap<String, VectorIndex>,
}

impl Executor {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            indexes: DashMap::new(),
        }
    }

    pub fn execute(&self, plan: &Plan) -> Result<QueryResult, EngineError> {
        let start = Instant::now();

        match plan {
            Plan::CreateCollection(p) => {
                self.store.create_collection(&p.name, p.dimensions)?;
                self.indexes
                    .insert(p.name.clone(), VectorIndex::new(p.dimensions));

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
                        tier: "RAM",
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
                        vector: vec_f32.clone(),
                        recipe_hash: [0u8; 32],
                        state: ReprState::Clean,
                    };
                    entity.representations.push(repr);

                    // Insert into vector index
                    if let Some(index) = self.indexes.get(&p.collection) {
                        index.insert(&id, &vec_f32);
                    }
                }

                self.store.insert(&p.collection, entity)?;

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
                        tier: "RAM",
                    },
                })
            }

            Plan::Fetch(p) => {
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
                        tier: "RAM",
                    },
                })
            }

            Plan::Search(p) => self.execute_search(p, start),

            Plan::Explain(inner) => {
                let rows = explain_plan(inner);
                Ok(QueryResult {
                    columns: vec!["property".into(), "value".into()],
                    rows,
                    stats: QueryStats {
                        elapsed: start.elapsed(),
                        entities_scanned: 0,
                        mode: QueryMode::Deterministic,
                        tier: "RAM",
                    },
                })
            }
        }
    }

    fn execute_search(&self, p: &SearchPlan, start: Instant) -> Result<QueryResult, EngineError> {
        let index = self
            .indexes
            .get(&p.collection)
            .ok_or_else(|| EngineError::CollectionNotFound(p.collection.clone()))?;

        let query_f32: Vec<f32> = p.query_vector.iter().map(|v| *v as f32).collect();
        let results = index.search(&query_f32, p.k);
        drop(index);

        let mut rows = Vec::new();
        for (id, score) in &results {
            if (*score as f64) < p.confidence_threshold {
                continue;
            }
            let entity = self.store.get(&p.collection, id)?;
            let mut row = entity_to_row(&entity, &FieldList::All);
            row.score = Some(*score);
            row.values
                .insert("_score".into(), Value::Float(*score as f64));
            rows.push(row);
        }

        let scanned = results.len();
        let mut columns = build_columns(&rows, &FieldList::All);
        if !columns.contains(&"_score".to_string()) {
            columns.push("_score".into());
        }

        Ok(QueryResult {
            columns,
            rows,
            stats: QueryStats {
                elapsed: start.elapsed(),
                entities_scanned: scanned,
                mode: QueryMode::Probabilistic,
                tier: "RAM",
            },
        })
    }

    pub fn collections(&self) -> Vec<String> {
        self.store.list_collections()
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
            props.push(("tier", "RAM".into()));
            props.push(("strategy", "FullScan".into()));
            if let Some(limit) = p.limit {
                props.push(("limit", limit.to_string()));
            }
        }
        Plan::Search(p) => {
            props.push(("mode", "Probabilistic".into()));
            props.push(("verb", "SEARCH".into()));
            props.push(("collection", p.collection.clone()));
            props.push(("tier", "RAM".into()));
            props.push(("encoding", "Float32".into()));
            props.push(("strategy", "BruteForce".into()));
            props.push(("k", p.k.to_string()));
            props.push(("confidence_threshold", p.confidence_threshold.to_string()));
        }
        Plan::Insert(p) => {
            props.push(("mode", "Deterministic".into()));
            props.push(("verb", "INSERT".into()));
            props.push(("collection", p.collection.clone()));
            props.push(("tier", "RAM".into()));
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
    use crate::planner::{CreateCollectionPlan, FetchPlan, InsertPlan};
    use trondb_tql::Literal;

    fn setup_executor() -> Executor {
        Executor::new(Store::new())
    }

    fn create_collection(exec: &Executor, name: &str, dims: usize) {
        exec.execute(&Plan::CreateCollection(CreateCollectionPlan {
            name: name.into(),
            dimensions: dims,
        }))
        .unwrap();
    }

    fn insert_entity(
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
        .unwrap();
    }

    #[test]
    fn execute_fetch_all() {
        let exec = setup_executor();
        create_collection(&exec, "venues", 3);

        insert_entity(
            &exec,
            "venues",
            "v1",
            vec![("name", Literal::String("Venue A".into()))],
            None,
        );
        insert_entity(
            &exec,
            "venues",
            "v2",
            vec![("name", Literal::String("Venue B".into()))],
            None,
        );

        let result = exec
            .execute(&Plan::Fetch(FetchPlan {
                collection: "venues".into(),
                fields: FieldList::All,
                filter: None,
                limit: None,
            }))
            .unwrap();

        assert_eq!(result.rows.len(), 2);
    }

    #[test]
    fn execute_fetch_with_filter() {
        let exec = setup_executor();
        create_collection(&exec, "venues", 3);

        insert_entity(
            &exec,
            "venues",
            "v1",
            vec![
                ("name", Literal::String("The Shard".into())),
                ("city", Literal::String("London".into())),
            ],
            None,
        );
        insert_entity(
            &exec,
            "venues",
            "v2",
            vec![
                ("name", Literal::String("Old Trafford".into())),
                ("city", Literal::String("Manchester".into())),
            ],
            None,
        );

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
            .unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("city"),
            Some(&Value::String("London".into()))
        );
    }

    #[test]
    fn execute_search() {
        let exec = setup_executor();
        create_collection(&exec, "venues", 3);

        insert_entity(
            &exec,
            "venues",
            "v1",
            vec![("name", Literal::String("Exact".into()))],
            Some(vec![1.0, 0.0, 0.0]),
        );
        insert_entity(
            &exec,
            "venues",
            "v2",
            vec![("name", Literal::String("Close".into()))],
            Some(vec![0.9, 0.1, 0.0]),
        );
        insert_entity(
            &exec,
            "venues",
            "v3",
            vec![("name", Literal::String("Far".into()))],
            Some(vec![0.0, 0.0, 1.0]),
        );

        let result = exec
            .execute(&Plan::Search(SearchPlan {
                collection: "venues".into(),
                query_vector: vec![1.0, 0.0, 0.0],
                k: 10,
                confidence_threshold: 0.0,
            }))
            .unwrap();

        assert_eq!(result.rows.len(), 3);
        // First result should be the exact match
        assert_eq!(
            result.rows[0].values.get("name"),
            Some(&Value::String("Exact".into()))
        );
        // Scores should be descending
        assert!(result.rows[0].score.unwrap() >= result.rows[1].score.unwrap());
        assert!(result.rows[1].score.unwrap() >= result.rows[2].score.unwrap());
    }

    #[test]
    fn execute_search_with_confidence_filter() {
        let exec = setup_executor();
        create_collection(&exec, "venues", 3);

        insert_entity(
            &exec,
            "venues",
            "v1",
            vec![("name", Literal::String("Exact".into()))],
            Some(vec![1.0, 0.0, 0.0]),
        );
        insert_entity(
            &exec,
            "venues",
            "v2",
            vec![("name", Literal::String("Close".into()))],
            Some(vec![0.9, 0.1, 0.0]),
        );
        insert_entity(
            &exec,
            "venues",
            "v3",
            vec![("name", Literal::String("Far".into()))],
            Some(vec![0.0, 0.0, 1.0]),
        );

        let result = exec
            .execute(&Plan::Search(SearchPlan {
                collection: "venues".into(),
                query_vector: vec![1.0, 0.0, 0.0],
                k: 10,
                confidence_threshold: 0.8,
            }))
            .unwrap();

        // "Far" vector [0,0,1] has ~0 similarity to [1,0,0], should be filtered
        assert_eq!(result.rows.len(), 2);
        for row in &result.rows {
            assert!(row.score.unwrap() >= 0.8);
        }
    }

    #[test]
    fn execute_explain() {
        let exec = setup_executor();

        let search_plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            query_vector: vec![1.0, 0.0, 0.0],
            k: 5,
            confidence_threshold: 0.5,
        });

        let result = exec
            .execute(&Plan::Explain(Box::new(search_plan)))
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
            Some(&Value::String("Probabilistic".into()))
        );
    }
}
