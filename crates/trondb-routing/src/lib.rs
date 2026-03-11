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

    // -----------------------------------------------------------------------
    // Helper: open a fresh Engine backed by a temp dir
    // -----------------------------------------------------------------------

    async fn make_engine(dir: &std::path::Path) -> std::sync::Arc<trondb_core::Engine> {
        let cfg = trondb_core::EngineConfig {
            data_dir: dir.join("store"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
        };
        std::sync::Arc::new(trondb_core::Engine::open(cfg).await.unwrap())
    }

    /// Build a HealthSignal with explicit load_score for manual cache seeding.
    fn make_health_signal(node_id: &str, load_score: f32) -> health::HealthSignal {
        health::HealthSignal {
            node_id: node::NodeId::from_string(node_id),
            node_role: node::NodeRole::HotNode,
            signal_ts: 0,
            sequence: 0,
            cpu_utilisation: 0.0,
            ram_pressure: 0.0,
            hot_entity_count: 0,
            hot_tier_capacity: 100_000,
            queue_depth: 0,
            queue_capacity: 1000,
            hnsw_p99_ms: 0.0,
            hnsw_p50_ms: 0.0,
            replica_lag_ms: None,
            load_score,
            status: health::NodeStatus::Healthy,
        }
    }

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

    #[tokio::test]
    async fn explain_includes_routing_section() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = trondb_core::EngineConfig {
            data_dir: dir.path().join("store"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
        };
        let engine = std::sync::Arc::new(trondb_core::Engine::open(cfg).await.unwrap());
        let node = std::sync::Arc::new(
            node::LocalNode::new(engine, node::NodeId::from_string("local"))
        ) as std::sync::Arc<dyn node::NodeHandle>;
        let router = router::SemanticRouter::new(vec![node], config::RouterConfig::default());

        // Wait for health polling
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Create an EXPLAIN plan wrapping a Fetch
        let inner = trondb_core::planner::Plan::Fetch(trondb_core::planner::FetchPlan {
            collection: "test".into(),
            fields: trondb_tql::FieldList::All,
            filter: None,
            limit: Some(10),
            strategy: trondb_core::planner::FetchStrategy::FullScan,
        });
        let plan = trondb_core::planner::Plan::Explain(Box::new(inner));

        let result = router.route_and_execute(&plan).await.unwrap();
        // Should have routing rows appended
        let has_routing = result.rows.iter().any(|r| {
            r.values.get("property") == Some(&trondb_core::types::Value::String("routing".into()))
        });
        assert!(has_routing, "EXPLAIN should include routing section");
        let has_node = result.rows.iter().any(|r| {
            r.values.get("property") == Some(&trondb_core::types::Value::String("selected_node".into()))
        });
        assert!(has_node, "EXPLAIN should include selected_node");
    }

    // -----------------------------------------------------------------------
    // Test 1: multi-node routing prefers the healthy (lower-load) node
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn multi_node_routing_prefers_healthy_node() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let engine1 = make_engine(dir1.path()).await;
        let engine2 = make_engine(dir2.path()).await;

        let node1 = std::sync::Arc::new(
            node::SimulatedNode::new(engine1, node::NodeId::from_string("node-1"))
        ) as std::sync::Arc<dyn node::NodeHandle>;
        let node2 = std::sync::Arc::new(
            node::SimulatedNode::new(engine2, node::NodeId::from_string("node-2"))
        ) as std::sync::Arc<dyn node::NodeHandle>;

        let router = router::SemanticRouter::new(
            vec![node1, node2],
            config::RouterConfig::default(),
        );

        // Manually seed health cache: node-1 healthy (0.2), node-2 stressed (0.7)
        router.health_cache().update(make_health_signal("node-1", 0.2));
        router.health_cache().update(make_health_signal("node-2", 0.7));

        let plan = trondb_core::planner::Plan::Fetch(trondb_core::planner::FetchPlan {
            collection: "nonexistent".into(),
            fields: trondb_tql::FieldList::All,
            filter: None,
            limit: Some(10),
            strategy: trondb_core::planner::FetchStrategy::FullScan,
        });

        // Execute to get a routing decision (will fail at engine level — that's fine)
        let _ = router.route_and_execute(&plan).await;
        let decision = router.last_routing_decision().expect("should have a routing decision");
        assert_eq!(
            decision.node,
            node::NodeId::from_string("node-1"),
            "should prefer the node with lower load score"
        );
    }

    // -----------------------------------------------------------------------
    // Test 2: load shedding avoids overloaded node
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn load_shedding_avoids_overloaded_node() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let engine1 = make_engine(dir1.path()).await;
        let engine2 = make_engine(dir2.path()).await;

        let node1 = std::sync::Arc::new(
            node::SimulatedNode::new(engine1, node::NodeId::from_string("node-1"))
        ) as std::sync::Arc<dyn node::NodeHandle>;
        let node2 = std::sync::Arc::new(
            node::SimulatedNode::new(engine2, node::NodeId::from_string("node-2"))
        ) as std::sync::Arc<dyn node::NodeHandle>;

        let router = router::SemanticRouter::new(
            vec![node1, node2],
            config::RouterConfig::default(),
        );

        // node-1 above load_shed_threshold (0.85), node-2 healthy
        router.health_cache().update(make_health_signal("node-1", 0.9));
        router.health_cache().update(make_health_signal("node-2", 0.3));

        let plan = trondb_core::planner::Plan::Fetch(trondb_core::planner::FetchPlan {
            collection: "nonexistent".into(),
            fields: trondb_tql::FieldList::All,
            filter: None,
            limit: Some(10),
            strategy: trondb_core::planner::FetchStrategy::FullScan,
        });

        let _ = router.route_and_execute(&plan).await;
        let decision = router.last_routing_decision().expect("should have a routing decision");
        assert_eq!(
            decision.node,
            node::NodeId::from_string("node-2"),
            "overloaded node-1 should be shed, routing to node-2"
        );
    }

    // -----------------------------------------------------------------------
    // Test 3: all nodes overloaded → ClusterOverloaded
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn all_nodes_overloaded_returns_cluster_overloaded() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let engine1 = make_engine(dir1.path()).await;
        let engine2 = make_engine(dir2.path()).await;

        let node1 = std::sync::Arc::new(
            node::SimulatedNode::new(engine1, node::NodeId::from_string("node-1"))
        ) as std::sync::Arc<dyn node::NodeHandle>;
        let node2 = std::sync::Arc::new(
            node::SimulatedNode::new(engine2, node::NodeId::from_string("node-2"))
        ) as std::sync::Arc<dyn node::NodeHandle>;

        let router = router::SemanticRouter::new(
            vec![node1, node2],
            config::RouterConfig::default(),
        );

        // Both nodes above load_shed_threshold
        router.health_cache().update(make_health_signal("node-1", 0.9));
        router.health_cache().update(make_health_signal("node-2", 0.9));

        let plan = trondb_core::planner::Plan::Fetch(trondb_core::planner::FetchPlan {
            collection: "nonexistent".into(),
            fields: trondb_tql::FieldList::All,
            filter: None,
            limit: Some(10),
            strategy: trondb_core::planner::FetchStrategy::FullScan,
        });

        let result = router.route_and_execute(&plan).await;
        assert!(
            matches!(result, Err(RouterError::ClusterOverloaded)),
            "expected ClusterOverloaded, got {:?}",
            result
        );
    }

    // -----------------------------------------------------------------------
    // Test 4: co-occurrence learning creates implicit affinity groups
    // -----------------------------------------------------------------------

    #[test]
    fn cooccurrence_learning_creates_implicit_groups() {
        use trondb_core::types::LogicalId;

        let idx = affinity::AffinityIndex::new();
        let cfg = config::ColocationConfig {
            learn_threshold: 0.70,
            decay_factor: 0.95,
            max_group_size: 500,
            ..config::ColocationConfig::default()
        };

        let ea = LogicalId::from_string("entity-a");
        let eb = LogicalId::from_string("entity-b");

        // Record 5 co-occurrences of (ea, eb) — each pair-of-2 gives increment 1.0
        // so after 5 calls the score is 5.0, well above learn_threshold 0.70
        for _ in 0..5 {
            idx.record_cooccurrence(&[ea.clone(), eb.clone()]);
        }

        idx.promote_and_decay(&cfg);

        let group_a = idx.group_for(&ea).expect("entity-a should be in a group");
        let group_b = idx.group_for(&eb).expect("entity-b should be in a group");
        assert_eq!(group_a, group_b, "both entities should be in the same learned group");

        let group = idx.get_group(&group_a).unwrap();
        assert_eq!(group.source, affinity::AffinitySource::Learned);
        assert!(group.members.contains(&ea));
        assert!(group.members.contains(&eb));
    }

    // -----------------------------------------------------------------------
    // Test 5: affinity group CRUD — full lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn affinity_group_crud() {
        use trondb_core::types::LogicalId;

        let idx = affinity::AffinityIndex::new();
        let gid = node::AffinityGroupId::from_string("test-group");
        let entity = LogicalId::from_string("entity-1");
        let entity2 = LogicalId::from_string("entity-2");

        // Create group
        idx.create_group(gid.clone()).unwrap();
        let group = idx.get_group(&gid).unwrap();
        assert!(group.members.is_empty(), "new group should be empty");

        // Add member
        idx.add_to_group(&entity, &gid, 10).unwrap();
        assert_eq!(idx.group_for(&entity), Some(gid.clone()));
        let group = idx.get_group(&gid).unwrap();
        assert!(group.members.contains(&entity));

        // Remove member
        idx.remove_from_group(&entity);
        assert_eq!(idx.group_for(&entity), None, "entity should be removed");
        let group = idx.get_group(&gid).unwrap();
        assert!(!group.members.contains(&entity), "group members should not contain removed entity");

        // Test max_size enforcement: add two entities to a max_size=1 group
        idx.add_to_group(&entity, &gid, 1).unwrap();
        let err = idx.add_to_group(&entity2, &gid, 1);
        assert!(
            matches!(err, Err(RouterError::AffinityGroupFull(_, 1))),
            "adding to full group should return AffinityGroupFull, got {:?}",
            err
        );
    }
}
