pub mod affinity;
pub mod config;
pub mod error;
pub mod eviction;
pub mod health;
pub mod node;
pub mod router;

pub use affinity::{AffinityGroup, AffinityIndex, AffinitySource};
pub use config::{ColocationConfig, HealthConfig, RouterConfig};
pub use error::RouterError;
pub use eviction::{EvictionPriority, LruTracker, eviction_priority, select_eviction_candidates};
pub use health::{compute_load_score, HealthCache, HealthSignal, NodeStatus};
pub use node::{
    AffinityGroupId, EntityId, LocalNode, NodeHandle, NodeId, NodeRole,
    QueryVerb, RoutingStrategy, SimulatedNode,
};
pub use router::{
    AnnotatedPlan, RoutingDecision, ScoredCandidate, SemanticRouter,
    estimate_acu, entity_affinity_score, plan_verb, plan_targets,
    routing_score, verb_fit_score,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn router_single_node_routes_to_only_node() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = trondb_core::EngineConfig {
            data_dir: dir.path().join("store"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
        };
        let engine = std::sync::Arc::new(
            trondb_core::Engine::open(cfg).await.unwrap()
        );
        let node = std::sync::Arc::new(
            node::LocalNode::new(engine, node::NodeId::from_string("local"))
        ) as std::sync::Arc<dyn node::NodeHandle>;

        let router = router::SemanticRouter::new(
            vec![node],
            config::RouterConfig::default(),
        );

        // Give the health poll a moment to populate cache
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Route a simple plan
        let plan = trondb_core::planner::Plan::Fetch(trondb_core::planner::FetchPlan {
            collection: "nonexistent".into(),
            fields: trondb_tql::FieldList::All,
            filter: None,
            limit: Some(10),
            strategy: trondb_core::planner::FetchStrategy::FullScan,
        });

        // This should fail with an engine error (routed correctly, but collection doesn't exist)
        let result = router.route_and_execute(&plan).await;
        assert!(matches!(result, Err(RouterError::Engine(_))));
    }
}
