use trondb_core::planner::Plan;
use trondb_core::types::LogicalId;

use crate::affinity::AffinityIndex;
use crate::config::RouterConfig;
use crate::health::{HealthCache, HealthSignal};
use crate::node::{EntityId, NodeId, QueryVerb, RoutingStrategy};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AnnotatedPlan {
    pub plan: Plan,
    pub verb: QueryVerb,
    pub acu: f32,
    pub targets: Vec<EntityId>,
}

#[derive(Debug, Clone)]
pub struct RoutingDecision {
    pub node: NodeId,
    pub score: f32,
    pub all_candidates: Vec<ScoredCandidate>,
    pub strategy: RoutingStrategy,
}

#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    pub node: NodeId,
    pub score: f32,
    pub health_score: f32,
    pub verb_score: f32,
    pub affinity_score: f32,
}

// ---------------------------------------------------------------------------
// Scoring functions
// ---------------------------------------------------------------------------

/// Score how well this node's current state fits the requested verb.
pub fn verb_fit_score(verb: QueryVerb, signal: &HealthSignal) -> f32 {
    match verb {
        QueryVerb::Infer => {
            if signal.cpu_utilisation < 0.40 {
                1.0
            } else if signal.cpu_utilisation < 0.65 {
                0.5
            } else {
                0.1
            }
        }
        QueryVerb::Search => {
            let baseline = 10.0_f32;
            1.0 - (signal.hnsw_p99_ms / baseline).min(1.0)
        }
        QueryVerb::Traverse => {
            if signal.queue_capacity > 0 {
                1.0 - (signal.queue_depth as f32 / signal.queue_capacity as f32)
            } else {
                0.5
            }
        }
        QueryVerb::Fetch => 0.5,
    }
}

/// Fraction of target entities that have an affinity preference for this node.
pub fn entity_affinity_score(targets: &[EntityId], node: &NodeId, affinity: &AffinityIndex) -> f32 {
    if targets.is_empty() {
        return 0.5;
    }
    let on_node = targets
        .iter()
        .filter(|e| affinity.preferred_node(e).as_ref() == Some(node))
        .count();
    on_node as f32 / targets.len() as f32
}

/// Compute a composite routing score for one candidate node.
/// Returns `None` if the node has no entry in the health cache.
pub fn routing_score(
    node: &NodeId,
    verb: QueryVerb,
    targets: &[EntityId],
    health: &HealthCache,
    affinity: &AffinityIndex,
    config: &RouterConfig,
) -> Option<ScoredCandidate> {
    let signal = health.get(node)?;
    let hs = 1.0 - signal.load_score;
    let vs = verb_fit_score(verb, &signal);
    let af = entity_affinity_score(targets, node, affinity);
    Some(ScoredCandidate {
        node: node.clone(),
        score: (hs * config.weight_health) + (vs * config.weight_verb_fit) + (af * config.weight_affinity),
        health_score: hs,
        verb_score: vs,
        affinity_score: af,
    })
}

// ---------------------------------------------------------------------------
// ACU estimation
// ---------------------------------------------------------------------------

/// Estimate Abstract Compute Units for a plan.
pub fn estimate_acu(plan: &Plan) -> f32 {
    match plan {
        Plan::Fetch(_) => 1.0,
        Plan::Search(s) => 10.0 * s.k as f32,
        Plan::Traverse(t) => 5.0 * t.depth as f32,
        Plan::Explain(inner) => estimate_acu(inner),
        _ => 1.0,
    }
}

// ---------------------------------------------------------------------------
// Verb / targets extraction from Plan
// ---------------------------------------------------------------------------

/// Map a plan to its routing verb.
pub fn plan_verb(plan: &Plan) -> QueryVerb {
    match plan {
        Plan::Fetch(_) => QueryVerb::Fetch,
        Plan::Search(_) => QueryVerb::Search,
        Plan::Traverse(_) => QueryVerb::Traverse,
        Plan::Explain(inner) => plan_verb(inner),
        _ => QueryVerb::Fetch,
    }
}

/// Extract entity IDs that the plan references (used for affinity scoring).
pub fn plan_targets(plan: &Plan) -> Vec<EntityId> {
    match plan {
        Plan::Traverse(t) => {
            vec![LogicalId::from_string(&t.from_id)]
        }
        Plan::Explain(inner) => plan_targets(inner),
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RouterConfig;
    use crate::health::HealthSignal;
    use crate::node::{NodeId, NodeRole};
    use crate::health::NodeStatus;

    fn make_signal(node: &str, load: f32, cpu: f32, hnsw_p99: f32, queue: u32) -> HealthSignal {
        HealthSignal {
            node_id: NodeId::from_string(node),
            node_role: NodeRole::HotNode,
            signal_ts: 0,
            sequence: 0,
            cpu_utilisation: cpu,
            ram_pressure: 0.0,
            hot_entity_count: 0,
            hot_tier_capacity: 100_000,
            queue_depth: queue,
            queue_capacity: 1000,
            hnsw_p99_ms: hnsw_p99,
            hnsw_p50_ms: 0.0,
            replica_lag_ms: None,
            load_score: load,
            status: NodeStatus::Healthy,
        }
    }

    #[test]
    fn verb_fit_fetch_is_neutral() {
        let s = make_signal("n", 0.0, 0.0, 0.0, 0);
        assert!((verb_fit_score(QueryVerb::Fetch, &s) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn verb_fit_infer_prefers_low_cpu() {
        let idle = make_signal("n", 0.0, 0.2, 0.0, 0);
        let busy = make_signal("n", 0.0, 0.8, 0.0, 0);
        assert!((verb_fit_score(QueryVerb::Infer, &idle) - 1.0).abs() < 1e-6);
        assert!((verb_fit_score(QueryVerb::Infer, &busy) - 0.1).abs() < 1e-6);
    }

    #[test]
    fn verb_fit_search_prefers_low_hnsw_latency() {
        let fast = make_signal("n", 0.0, 0.0, 2.0, 0);
        let slow = make_signal("n", 0.0, 0.0, 10.0, 0);
        assert!(verb_fit_score(QueryVerb::Search, &fast) > verb_fit_score(QueryVerb::Search, &slow));
    }

    #[test]
    fn verb_fit_traverse_prefers_low_queue() {
        let empty = make_signal("n", 0.0, 0.0, 0.0, 0);
        let full = make_signal("n", 0.0, 0.0, 0.0, 1000);
        assert!((verb_fit_score(QueryVerb::Traverse, &empty) - 1.0).abs() < 1e-6);
        assert!((verb_fit_score(QueryVerb::Traverse, &full) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn entity_affinity_empty_targets_returns_neutral() {
        let affinity = AffinityIndex::new();
        let score = entity_affinity_score(&[], &NodeId::from_string("n"), &affinity);
        assert!((score - 0.5).abs() < 1e-6);
    }

    #[test]
    fn routing_score_combines_factors() {
        let cfg = RouterConfig::default();
        let cache = crate::health::HealthCache::new(200, 3);
        let affinity = AffinityIndex::new();
        let s = make_signal("node-1", 0.2, 0.1, 1.0, 50);
        cache.update(s);
        let scored = routing_score(
            &NodeId::from_string("node-1"),
            QueryVerb::Fetch,
            &[],
            &cache,
            &affinity,
            &cfg,
        )
        .unwrap();
        // health: (1.0 - 0.2) * 0.40 = 0.32
        // verb:   0.5           * 0.30 = 0.15
        // affinity: 0.5         * 0.30 = 0.15
        // total: 0.62
        assert!((scored.score - 0.62).abs() < 1e-6, "got {}", scored.score);
    }
}
