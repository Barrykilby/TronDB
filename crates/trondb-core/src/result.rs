use std::collections::HashMap;
use std::time::Duration;

use crate::types::Value;

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
    pub stats: QueryStats,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub values: HashMap<String, Value>,
    pub score: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct QueryStats {
    pub elapsed: Duration,
    pub entities_scanned: usize,
    pub mode: QueryMode,
    pub tier: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryMode {
    Deterministic,
    Probabilistic,
}
