pub mod error;
pub mod executor;
pub mod planner;
pub mod result;
pub mod store;
pub mod types;

use error::EngineError;
use executor::Executor;
use result::QueryResult;
use store::FjallStore;

// ---------------------------------------------------------------------------
// Engine — public API
// ---------------------------------------------------------------------------

pub struct Engine {
    executor: Executor,
}

impl Engine {
    /// Create an engine backed by a fresh temporary directory.
    /// Used for testing and ephemeral use cases.
    pub fn new() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let data_dir = std::env::temp_dir().join(format!("trondb_engine_{ts}"));
        std::fs::create_dir_all(&data_dir).expect("failed to create temp data dir");
        let store = FjallStore::open(&data_dir).expect("failed to open FjallStore");
        Self {
            executor: Executor::new(store),
        }
    }

    pub fn open(data_dir: &std::path::Path) -> Result<Self, error::EngineError> {
        let store = FjallStore::open(data_dir)?;
        Ok(Self {
            executor: Executor::new(store),
        })
    }

    pub fn execute_tql(&self, input: &str) -> Result<QueryResult, EngineError> {
        let stmt =
            trondb_tql::parse(input).map_err(|e| EngineError::InvalidQuery(e.to_string()))?;
        let execution_plan = planner::plan(&stmt)?;
        self.executor.execute(&execution_plan)
    }

    pub fn collections(&self) -> Vec<String> {
        self.executor.collections()
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;

    #[test]
    fn end_to_end_create_insert_fetch() {
        let engine = Engine::new();

        engine
            .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name, city) VALUES ('v1', 'The Shard', 'London') VECTOR [0.1, 0.2, 0.3];",
            )
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name, city) VALUES ('v2', 'Old Trafford', 'Manchester') VECTOR [0.4, 0.5, 0.6];",
            )
            .unwrap();

        let result = engine
            .execute_tql("FETCH * FROM venues WHERE city = 'London';")
            .unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("name"),
            Some(&Value::String("The Shard".into()))
        );
    }

    #[test]
    fn search_returns_unsupported() {
        let engine = Engine::new();

        let result = engine
            .execute_tql("SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] CONFIDENCE > 0.8;");

        assert!(result.is_err());
    }

    #[test]
    fn end_to_end_explain() {
        let engine = Engine::new();

        let result = engine
            .execute_tql("EXPLAIN FETCH * FROM venues;")
            .unwrap();

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
