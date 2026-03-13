//! Scatter-gather SEARCH with dense and hybrid merge logic.
//!
//! When a SEARCH query targets a collection that spans multiple nodes,
//! the router fans out the plan to all relevant nodes in parallel, collects
//! partial results, and merges them into a single `QueryResult`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tonic::Status;
use trondb_core::planner::{Plan, SearchPlan, SearchStrategy};
use trondb_core::result::{QueryMode, QueryResult, QueryStats, Row};
use trondb_core::types::Value;
use trondb_routing::node::NodeHandle;

// ---------------------------------------------------------------------------
// Dense merge
// ---------------------------------------------------------------------------

/// Merge multiple dense search results into a single result.
///
/// Collects all rows, sorts by score descending, takes the top `limit`,
/// and sums `entities_scanned` across all inputs.
pub fn merge_dense_results(results: Vec<QueryResult>, limit: usize) -> QueryResult {
    if results.is_empty() {
        return empty_result();
    }

    let mut all_rows: Vec<Row> = Vec::new();
    let mut total_scanned: usize = 0;
    let mut columns = Vec::new();
    let mut max_elapsed = Duration::ZERO;

    for r in results {
        total_scanned += r.stats.entities_scanned;
        if columns.is_empty() {
            columns = r.columns;
        }
        if r.stats.elapsed > max_elapsed {
            max_elapsed = r.stats.elapsed;
        }
        all_rows.extend(r.rows);
    }

    // Sort by score descending (None scores go last)
    all_rows.sort_by(|a, b| {
        let sa = a.score.unwrap_or(f32::NEG_INFINITY);
        let sb = b.score.unwrap_or(f32::NEG_INFINITY);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    all_rows.truncate(limit);

    QueryResult {
        columns,
        rows: all_rows,
        stats: QueryStats {
            elapsed: max_elapsed,
            entities_scanned: total_scanned,
            mode: QueryMode::Probabilistic,
            tier: "scatter-gather".to_string(),
            cost: None,
            warnings: vec![],
        },
    }
}

// ---------------------------------------------------------------------------
// Hybrid (RRF) merge
// ---------------------------------------------------------------------------

/// Merge multiple hybrid search results using Reciprocal Rank Fusion (RRF).
///
/// Each result set's rows are ranked 1..n (1-based, in the order they appear,
/// which is assumed to be sorted by the node's local score). The RRF score
/// for each unique entity is: sum over result sets of 1/(k + rank).
///
/// Entities are identified by their "id" field. If no "id" field is present,
/// rows are treated as unique (no cross-node dedup).
pub fn merge_hybrid_results(results: Vec<QueryResult>, limit: usize) -> QueryResult {
    const K: f64 = 60.0;

    if results.is_empty() {
        return empty_result();
    }

    let mut total_scanned: usize = 0;
    let mut columns = Vec::new();
    let mut max_elapsed = Duration::ZERO;

    // Map from entity id string -> (accumulated RRF score, Row)
    // We keep the first-seen Row as the representative.
    let mut rrf_scores: HashMap<String, (f64, Row)> = HashMap::new();
    // For rows without an "id" field, assign unique synthetic keys
    let mut anon_counter: u64 = 0;

    for r in &results {
        total_scanned += r.stats.entities_scanned;
        if columns.is_empty() {
            columns = r.columns.clone();
        }
        if r.stats.elapsed > max_elapsed {
            max_elapsed = r.stats.elapsed;
        }

        for (rank_idx, row) in r.rows.iter().enumerate() {
            let rank = (rank_idx + 1) as f64; // 1-based
            let rrf_contribution = 1.0 / (K + rank);

            let key = extract_row_id(row).unwrap_or_else(|| {
                anon_counter += 1;
                format!("__anon_{anon_counter}")
            });

            rrf_scores
                .entry(key)
                .and_modify(|(score, _)| {
                    *score += rrf_contribution;
                })
                .or_insert_with(|| (rrf_contribution, row.clone()));
        }
    }

    // Collect and sort by RRF score descending
    let mut scored_rows: Vec<(f64, Row)> = rrf_scores.into_values().collect();
    scored_rows.sort_by(|a, b| {
        b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
    });

    scored_rows.truncate(limit);

    // Set the RRF score as the row's score
    let rows: Vec<Row> = scored_rows
        .into_iter()
        .map(|(rrf_score, mut row)| {
            row.score = Some(rrf_score as f32);
            row
        })
        .collect();

    QueryResult {
        columns,
        rows,
        stats: QueryStats {
            elapsed: max_elapsed,
            entities_scanned: total_scanned,
            mode: QueryMode::Probabilistic,
            tier: "scatter-gather".to_string(),
            cost: None,
            warnings: vec![],
        },
    }
}

// ---------------------------------------------------------------------------
// Scatter-gather search orchestration
// ---------------------------------------------------------------------------

/// Fan out a SEARCH plan to multiple nodes, collect results, and merge.
///
/// - If a single node is provided, dispatches directly (no merge overhead).
/// - If multiple nodes, sends the plan to all concurrently and merges.
pub async fn scatter_gather_search(
    nodes: &[Arc<dyn NodeHandle>],
    plan: &Plan,
    limit: usize,
) -> Result<QueryResult, Status> {
    if nodes.is_empty() {
        return Err(Status::internal("no nodes available for scatter-gather"));
    }

    // Single node: direct dispatch
    if nodes.len() == 1 {
        return nodes[0]
            .execute(plan)
            .await
            .map_err(|e| Status::internal(e.to_string()));
    }

    // Multiple nodes: fan out in parallel
    let mut handles = Vec::with_capacity(nodes.len());
    for node in nodes {
        let node = node.clone();
        let plan = plan.clone();
        handles.push(tokio::spawn(async move { node.execute(&plan).await }));
    }

    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(Ok(result)) => results.push(result),
            Ok(Err(e)) => {
                tracing::warn!("scatter-gather node error: {e}");
                // Continue with results from other nodes
            }
            Err(e) => {
                tracing::warn!("scatter-gather join error: {e}");
            }
        }
    }

    if results.is_empty() {
        return Err(Status::internal(
            "all nodes failed during scatter-gather search",
        ));
    }

    // Determine merge strategy from the plan
    let is_hybrid = matches!(plan, Plan::Search(SearchPlan { strategy: SearchStrategy::Hybrid, .. }));

    let merged = if is_hybrid {
        merge_hybrid_results(results, limit)
    } else {
        merge_dense_results(results, limit)
    };

    Ok(merged)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the entity ID from a row's "id" field, if present and a string.
fn extract_row_id(row: &Row) -> Option<String> {
    match row.values.get("id") {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Create an empty QueryResult.
fn empty_result() -> QueryResult {
    QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        stats: QueryStats {
            elapsed: Duration::ZERO,
            entities_scanned: 0,
            mode: QueryMode::Probabilistic,
            tier: "scatter-gather".to_string(),
            cost: None,
            warnings: vec![],
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use trondb_core::result::{QueryMode, QueryStats, Row};
    use trondb_core::types::Value;

    /// Helper: create a Row with an id, name, and optional score.
    fn make_row(id: &str, name: &str, score: Option<f32>) -> Row {
        let mut values = HashMap::new();
        values.insert("id".to_string(), Value::String(id.to_string()));
        values.insert("name".to_string(), Value::String(name.to_string()));
        Row { values, score }
    }

    /// Helper: create a QueryResult from rows and entities_scanned.
    fn make_result(rows: Vec<Row>, entities_scanned: usize) -> QueryResult {
        QueryResult {
            columns: vec!["id".to_string(), "name".to_string()],
            rows,
            stats: QueryStats {
                elapsed: Duration::from_millis(10),
                entities_scanned,
                mode: QueryMode::Probabilistic,
                tier: "hot".to_string(),
                cost: None,
                warnings: vec![],
            },
        }
    }

    // -------------------------------------------------------------------
    // Test 1: merge_dense_results sorts by score and respects limit
    // -------------------------------------------------------------------

    #[test]
    fn merge_dense_results_sorts_by_score() {
        // Node 1 returns: A(0.9), B(0.7) — scanned 50
        let r1 = make_result(
            vec![make_row("A", "Alpha", Some(0.9)), make_row("B", "Bravo", Some(0.7))],
            50,
        );

        // Node 2 returns: C(0.85), D(0.6) — scanned 60
        let r2 = make_result(
            vec![make_row("C", "Charlie", Some(0.85)), make_row("D", "Delta", Some(0.6))],
            60,
        );

        let merged = merge_dense_results(vec![r1, r2], 3);

        // Expect top 3 sorted by score descending: A(0.9), C(0.85), B(0.7)
        assert_eq!(merged.rows.len(), 3);
        assert_eq!(
            extract_row_id(&merged.rows[0]),
            Some("A".to_string()),
            "first should be A (score 0.9)"
        );
        assert_eq!(
            extract_row_id(&merged.rows[1]),
            Some("C".to_string()),
            "second should be C (score 0.85)"
        );
        assert_eq!(
            extract_row_id(&merged.rows[2]),
            Some("B".to_string()),
            "third should be B (score 0.7)"
        );

        // entities_scanned should be summed
        assert_eq!(
            merged.stats.entities_scanned, 110,
            "entities_scanned should be sum of 50 + 60"
        );
    }

    // -------------------------------------------------------------------
    // Test 2: merge_hybrid_results uses RRF with k=60
    // -------------------------------------------------------------------

    #[test]
    fn merge_hybrid_results_uses_rrf() {
        // Node 1: X(rank=1, score=0.9), Y(rank=2, score=0.8) — scanned 50
        let r1 = make_result(
            vec![make_row("X", "X-ray", Some(0.9)), make_row("Y", "Yankee", Some(0.8))],
            50,
        );

        // Node 2: Y(rank=1, score=0.95), Z(rank=2, score=0.7) — scanned 60
        let r2 = make_result(
            vec![make_row("Y", "Yankee", Some(0.95)), make_row("Z", "Zulu", Some(0.7))],
            60,
        );

        let merged = merge_hybrid_results(vec![r1, r2], 10);

        // RRF scores (k=60):
        //   X: appears in Node1 at rank 1 -> 1/(60+1) = 1/61 ≈ 0.01639
        //   Y: appears in Node1 at rank 2 -> 1/(60+2) = 1/62 ≈ 0.01613
        //      appears in Node2 at rank 1 -> 1/(60+1) = 1/61 ≈ 0.01639
        //      total Y = 1/62 + 1/61 ≈ 0.03252
        //   Z: appears in Node2 at rank 2 -> 1/(60+2) = 1/62 ≈ 0.01613
        //
        // Expected order: Y (0.03252), X (0.01639), Z (0.01613)

        assert_eq!(merged.rows.len(), 3, "should have 3 unique entities");

        assert_eq!(
            extract_row_id(&merged.rows[0]),
            Some("Y".to_string()),
            "Y should be first (appears in both result sets)"
        );
        assert_eq!(
            extract_row_id(&merged.rows[1]),
            Some("X".to_string()),
            "X should be second (rank 1 in one set)"
        );
        assert_eq!(
            extract_row_id(&merged.rows[2]),
            Some("Z".to_string()),
            "Z should be third (rank 2 in one set)"
        );

        // Verify entities_scanned summed
        assert_eq!(
            merged.stats.entities_scanned, 110,
            "entities_scanned should be 50 + 60 = 110"
        );

        // Verify Y's RRF score is approximately 1/61 + 1/62
        let y_score = merged.rows[0].score.unwrap() as f64;
        let expected_y = 1.0 / 61.0 + 1.0 / 62.0;
        assert!(
            (y_score - expected_y).abs() < 1e-4,
            "Y's RRF score should be ~{expected_y}, got {y_score}"
        );
    }

    // -------------------------------------------------------------------
    // Test 3: dense merge with empty inputs
    // -------------------------------------------------------------------

    #[test]
    fn merge_dense_empty_input() {
        let merged = merge_dense_results(vec![], 10);
        assert!(merged.rows.is_empty());
        assert_eq!(merged.stats.entities_scanned, 0);
    }

    // -------------------------------------------------------------------
    // Test 4: hybrid merge with no id field falls back to unique rows
    // -------------------------------------------------------------------

    #[test]
    fn merge_hybrid_no_id_field_treats_rows_as_unique() {
        let mut values = HashMap::new();
        values.insert("name".to_string(), Value::String("no-id".to_string()));
        let row_no_id = Row {
            values,
            score: Some(0.5),
        };

        let r1 = QueryResult {
            columns: vec!["name".to_string()],
            rows: vec![row_no_id.clone()],
            stats: QueryStats {
                elapsed: Duration::from_millis(5),
                entities_scanned: 10,
                mode: QueryMode::Probabilistic,
                tier: "hot".to_string(),
                cost: None,
                warnings: vec![],
            },
        };

        let r2 = QueryResult {
            columns: vec!["name".to_string()],
            rows: vec![row_no_id],
            stats: QueryStats {
                elapsed: Duration::from_millis(5),
                entities_scanned: 15,
                mode: QueryMode::Probabilistic,
                tier: "hot".to_string(),
                cost: None,
                warnings: vec![],
            },
        };

        let merged = merge_hybrid_results(vec![r1, r2], 10);
        // Without id field, both rows should be treated as distinct
        assert_eq!(merged.rows.len(), 2);
        assert_eq!(merged.stats.entities_scanned, 25);
    }

    // -------------------------------------------------------------------
    // Test 5: dense merge respects limit smaller than total rows
    // -------------------------------------------------------------------

    #[test]
    fn merge_dense_limit_truncates() {
        let r1 = make_result(
            vec![
                make_row("A", "Alpha", Some(0.9)),
                make_row("B", "Bravo", Some(0.8)),
                make_row("C", "Charlie", Some(0.7)),
            ],
            30,
        );

        let merged = merge_dense_results(vec![r1], 2);
        assert_eq!(merged.rows.len(), 2);
        assert_eq!(extract_row_id(&merged.rows[0]), Some("A".to_string()));
        assert_eq!(extract_row_id(&merged.rows[1]), Some("B".to_string()));
    }

    // -------------------------------------------------------------------
    // Test 6: single-result merge is a pass-through (dense)
    // -------------------------------------------------------------------

    #[test]
    fn merge_dense_single_result_passthrough() {
        let r1 = make_result(
            vec![make_row("X", "X-ray", Some(0.5))],
            42,
        );
        let merged = merge_dense_results(vec![r1], 10);
        assert_eq!(merged.rows.len(), 1);
        assert_eq!(merged.stats.entities_scanned, 42);
    }

    // -------------------------------------------------------------------
    // Test 7: rows without scores in dense merge sort last
    // -------------------------------------------------------------------

    #[test]
    fn merge_dense_none_scores_sort_last() {
        let r1 = make_result(
            vec![
                make_row("A", "Alpha", None),
                make_row("B", "Bravo", Some(0.5)),
            ],
            20,
        );

        let merged = merge_dense_results(vec![r1], 10);
        assert_eq!(extract_row_id(&merged.rows[0]), Some("B".to_string()));
        assert_eq!(extract_row_id(&merged.rows[1]), Some("A".to_string()));
    }
}
