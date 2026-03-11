use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use trondb_core::planner::Plan;
use trondb_core::result::QueryResult;
use trondb_core::types::LogicalId;
use trondb_wal::WalWriter;

use crate::affinity::AffinityIndex;
use crate::config::RouterConfig;
use crate::error::RouterError;
use crate::health::{compute_load_score, HealthCache, HealthSignal, NodeStatus};
use crate::node::{AffinityGroupId, EntityId, NodeHandle, NodeId, QueryVerb, RoutingStrategy};

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
// SemanticRouter
// ---------------------------------------------------------------------------

pub struct SemanticRouter {
    health: Arc<HealthCache>,
    affinity: Arc<AffinityIndex>,
    nodes: Arc<DashMap<NodeId, Arc<dyn NodeHandle>>>,
    config: RouterConfig,
    wal: Option<Arc<WalWriter>>,
    shutdown_tx: watch::Sender<bool>,
    _poll_handle: JoinHandle<()>,
    last_decision: Arc<std::sync::Mutex<Option<RoutingDecision>>>,
}

impl SemanticRouter {
    pub fn new(
        nodes: Vec<Arc<dyn NodeHandle>>,
        config: RouterConfig,
    ) -> Self {
        Self::with_wal(nodes, config, None)
    }

    pub fn with_wal(
        nodes: Vec<Arc<dyn NodeHandle>>,
        config: RouterConfig,
        wal: Option<Arc<WalWriter>>,
    ) -> Self {
        let health = Arc::new(HealthCache::new(
            config.health.push_interval_ms,
            config.health.stale_multiplier,
        ));
        let affinity = Arc::new(AffinityIndex::new());
        let node_map: Arc<DashMap<NodeId, Arc<dyn NodeHandle>>> = Arc::new(DashMap::new());
        for node in &nodes {
            node_map.insert(node.node_id().clone(), Arc::clone(node));
        }

        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let poll_nodes = Arc::clone(&node_map);
        let poll_health = Arc::clone(&health);
        let poll_config = config.health.clone();

        let poll_handle = tokio::spawn(async move {
            health_poll_loop(poll_nodes, poll_health, poll_config, shutdown_rx).await;
        });

        Self {
            health,
            affinity,
            nodes: node_map,
            config,
            wal,
            shutdown_tx,
            _poll_handle: poll_handle,
            last_decision: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn health_cache(&self) -> &Arc<HealthCache> {
        &self.health
    }

    pub fn affinity_index(&self) -> &Arc<AffinityIndex> {
        &self.affinity
    }

    pub fn last_routing_decision(&self) -> Option<RoutingDecision> {
        self.last_decision.lock().unwrap().clone()
    }

    pub async fn route_and_execute(
        &self,
        plan: &Plan,
    ) -> Result<QueryResult, RouterError> {
        // For EXPLAIN plans, route the inner plan then append routing rows to the result.
        if let Plan::Explain(inner) = plan {
            let annotated = self.annotate(inner);
            let decision = self.route(&annotated)?;
            *self.last_decision.lock().unwrap() = Some(decision.clone());
            let node = self.nodes.get(&decision.node)
                .ok_or(RouterError::NoCandidates)?;
            let mut result = node.execute(plan).await?;
            // Append routing rows to the explain output
            let mut extra = routing_rows(&decision);
            result.rows.append(&mut extra);
            // Ensure columns include property/value if not already present
            if !result.columns.contains(&"property".to_owned()) {
                result.columns.push("property".to_owned());
            }
            if !result.columns.contains(&"value".to_owned()) {
                result.columns.push("value".to_owned());
            }
            return Ok(result);
        }

        // Intercept routing-layer operations before general dispatch.
        match plan {
            Plan::CreateAffinityGroup(ref p) => {
                if let Some(ref wal) = self.wal {
                    self.affinity
                        .create_group_logged(AffinityGroupId::from_string(&p.name), wal)
                        .await?;
                } else {
                    self.affinity
                        .create_group(AffinityGroupId::from_string(&p.name))?;
                }
                return Ok(ok_result("Affinity group created"));
            }
            Plan::AlterEntityDropAffinity(ref p) => {
                let entity_id = EntityId::from_string(&p.entity_id);
                if let Some(ref wal) = self.wal {
                    self.affinity
                        .remove_from_group_logged(&entity_id, wal)
                        .await?;
                } else {
                    self.affinity.remove_from_group(&entity_id);
                }
                return Ok(ok_result("Affinity dropped"));
            }
            _ => {}
        }

        let annotated = self.annotate(plan);
        let decision = self.route(&annotated)?;
        *self.last_decision.lock().unwrap() = Some(decision.clone());
        let node = self.nodes.get(&decision.node)
            .ok_or(RouterError::NoCandidates)?;
        let result = node.execute(plan).await?;

        // After successful INSERT, handle affinity group assignment.
        if let Plan::Insert(ref p) = plan {
            if let Some(ref group_name) = p.affinity_group {
                let entity_id = find_entity_id_from_insert(p);
                let group_id = AffinityGroupId::from_string(group_name);
                let max_size = self.config.colocation.max_group_size;
                if let Some(ref wal) = self.wal {
                    self.affinity
                        .add_to_group_logged(&entity_id, &group_id, max_size, wal)
                        .await?;
                } else {
                    self.affinity
                        .add_to_group(&entity_id, &group_id, max_size)?;
                }
            }
            if let Some(ref collocate_ids) = p.collocate_with {
                let entity_id = find_entity_id_from_insert(p);
                let collocate_entities: Vec<EntityId> = collocate_ids
                    .iter()
                    .map(|id| EntityId::from_string(id))
                    .collect();
                let mut all = vec![entity_id];
                all.extend(collocate_entities);
                self.affinity.record_cooccurrence(&all);
            }
        }

        Ok(result)
    }

    fn annotate(&self, plan: &Plan) -> AnnotatedPlan {
        AnnotatedPlan {
            plan: plan.clone(),
            verb: plan_verb(plan),
            acu: estimate_acu(plan),
            targets: plan_targets(plan),
        }
    }

    fn route(&self, annotated: &AnnotatedPlan) -> Result<RoutingDecision, RouterError> {
        let strategy = match annotated.verb {
            QueryVerb::Search => RoutingStrategy::ScatterGather,
            _ => RoutingStrategy::LocationAware,
        };

        let candidates: Vec<NodeId> = self.health.healthy_nodes()
            .into_iter()
            .filter(|(_, load)| *load < self.config.health.load_shed_threshold)
            .map(|(id, _)| id)
            .collect();

        if candidates.is_empty() {
            let all = self.health.healthy_nodes();
            if all.is_empty() {
                return Err(RouterError::NoCandidates);
            } else {
                return Err(RouterError::ClusterOverloaded);
            }
        }

        let mut scored: Vec<ScoredCandidate> = candidates
            .iter()
            .filter_map(|id| {
                routing_score(id, annotated.verb, &annotated.targets, &self.health, &self.affinity, &self.config)
            })
            .collect();

        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        let best = scored.first().ok_or(RouterError::NoCandidates)?;

        Ok(RoutingDecision {
            node: best.node.clone(),
            score: best.score,
            all_candidates: scored,
            strategy,
        })
    }
}

impl Drop for SemanticRouter {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a simple success QueryResult with a status message.
fn ok_result(msg: &str) -> QueryResult {
    use trondb_core::result::{QueryMode, QueryStats, Row};
    use trondb_core::types::Value;

    QueryResult {
        columns: vec!["status".into()],
        rows: vec![Row {
            values: HashMap::from([("status".into(), Value::String(msg.to_owned()))]),
            score: None,
        }],
        stats: QueryStats {
            elapsed: Duration::ZERO,
            entities_scanned: 0,
            mode: QueryMode::Deterministic,
            tier: "Routing".into(),
        },
    }
}

/// Extract the entity ID from an INSERT plan by looking for an "id" field.
fn find_entity_id_from_insert(p: &trondb_core::planner::InsertPlan) -> EntityId {
    use trondb_tql::Literal;

    for (i, field) in p.fields.iter().enumerate() {
        if field.eq_ignore_ascii_case("id") {
            if let Some(Literal::String(ref s)) = p.values.get(i) {
                return LogicalId::from_string(s);
            }
        }
    }
    // Fallback: generate a deterministic ID from collection + first value
    LogicalId::default()
}

// ---------------------------------------------------------------------------
// Routing rows helper
// ---------------------------------------------------------------------------

fn routing_rows(decision: &RoutingDecision) -> Vec<trondb_core::result::Row> {
    use trondb_core::types::Value;
    use trondb_core::result::Row;

    let mut rows = vec![];
    let make_row = |prop: &str, val: &str| Row {
        values: HashMap::from([
            ("property".to_owned(), Value::String(prop.to_owned())),
            ("value".to_owned(), Value::String(val.to_owned())),
        ]),
        score: None,
    };
    rows.push(make_row("routing", &format!("{:?}", decision.strategy)));
    rows.push(make_row("selected_node", decision.node.as_str()));
    rows.push(make_row("routing_score", &format!("{:.3}", decision.score)));
    for c in &decision.all_candidates {
        rows.push(make_row(
            &format!("candidate:{}", c.node.as_str()),
            &format!(
                "score={:.3} health={:.3} verb={:.3} affinity={:.3}",
                c.score, c.health_score, c.verb_score, c.affinity_score
            ),
        ));
    }
    rows
}

async fn health_poll_loop(
    nodes: Arc<DashMap<NodeId, Arc<dyn NodeHandle>>>,
    health: Arc<HealthCache>,
    config: crate::config::HealthConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let interval = std::time::Duration::from_millis(config.push_interval_ms);
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // Collect node handles into a Vec to avoid holding DashMap
                // refs across an await point (avoids lifetime issues with
                // dyn Trait + DashMap).
                let snapshot: Vec<Arc<dyn NodeHandle>> = nodes
                    .iter()
                    .map(|entry| Arc::clone(entry.value()))
                    .collect();

                for node in &snapshot {
                    let node_id = node.node_id().clone();
                    match tokio::time::timeout(
                        Duration::from_secs(5),
                        node.health_snapshot(),
                    )
                    .await
                    {
                        Ok(mut signal) => {
                            signal.load_score = compute_load_score(&signal, &config);
                            if signal.load_score >= config.load_shed_threshold {
                                signal.status = NodeStatus::Overloaded;
                            } else if signal.cpu_utilisation >= config.cpu_warn_threshold
                                || signal.ram_pressure >= config.ram_warn_threshold
                                || signal.queue_depth >= config.queue_depth_alert
                                || signal.hnsw_p99_ms >= config.hnsw_baseline_p99_ms * 2.0
                            {
                                signal.status = NodeStatus::Degraded;
                            }
                            health.update(signal);
                        }
                        Err(_) => {
                            // Node is unresponsive — mark as faulted
                            let signal = HealthSignal {
                                node_id,
                                node_role: crate::node::NodeRole::HotNode,
                                signal_ts: 0,
                                sequence: 0,
                                cpu_utilisation: 0.0,
                                ram_pressure: 0.0,
                                hot_entity_count: 0,
                                hot_tier_capacity: 0,
                                queue_depth: 0,
                                queue_capacity: 0,
                                hnsw_p99_ms: 0.0,
                                hnsw_p50_ms: 0.0,
                                replica_lag_ms: None,
                                load_score: 1.0,
                                status: NodeStatus::Faulted,
                                warm_entity_count: 0,
                                archive_entity_count: 0,
                            };
                            health.update(signal);
                        }
                    }
                }
            }
            _ = shutdown.changed() => {
                break;
            }
        }
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
            warm_entity_count: 0,
            archive_entity_count: 0,
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
