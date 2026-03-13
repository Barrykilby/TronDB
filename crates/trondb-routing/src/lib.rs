pub mod affinity;
pub mod config;
pub mod error;
pub mod eviction;
pub mod health;
pub mod migrator;
pub mod node;
pub mod router;
pub mod startup;
pub mod sweeper;

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
            hnsw_snapshot_interval_secs: 0,
        };
        let (engine, _) = trondb_core::Engine::open(cfg).await.unwrap();
        std::sync::Arc::new(engine)
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
            warm_entity_count: 0,
            archive_entity_count: 0,
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
            hnsw_snapshot_interval_secs: 0,
        };
        let (engine, _) = trondb_core::Engine::open(cfg).await.unwrap();
        let engine = std::sync::Arc::new(engine);
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
            temporal: None,
            order_by: vec![],
            limit: Some(10),
            strategy: trondb_core::planner::FetchStrategy::FullScan,
            hints: vec![],
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
            hnsw_snapshot_interval_secs: 0,
        };
        let (engine, _) = trondb_core::Engine::open(cfg).await.unwrap();
        let engine = std::sync::Arc::new(engine);
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
            temporal: None,
            order_by: vec![],
            limit: Some(10),
            strategy: trondb_core::planner::FetchStrategy::FullScan,
            hints: vec![],
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
            temporal: None,
            order_by: vec![],
            limit: Some(10),
            strategy: trondb_core::planner::FetchStrategy::FullScan,
            hints: vec![],
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
            temporal: None,
            order_by: vec![],
            limit: Some(10),
            strategy: trondb_core::planner::FetchStrategy::FullScan,
            hints: vec![],
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
            temporal: None,
            order_by: vec![],
            limit: Some(10),
            strategy: trondb_core::planner::FetchStrategy::FullScan,
            hints: vec![],
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

    // -----------------------------------------------------------------------
    // Helper: set up engine + router + migrator for tier integration tests
    // -----------------------------------------------------------------------

    async fn make_tier_test_setup(
        dir: &std::path::Path,
    ) -> (
        std::sync::Arc<trondb_core::Engine>,
        router::SemanticRouter,
    ) {
        let engine = make_engine(dir).await;

        let node = std::sync::Arc::new(
            node::LocalNode::new(engine.clone(), node::NodeId::from_string("local"))
        ) as std::sync::Arc<dyn node::NodeHandle>;

        let lru = std::sync::Arc::new(std::sync::Mutex::new(eviction::LruTracker::new()));
        let mut router = router::SemanticRouter::new(
            vec![node],
            config::RouterConfig::default(),
        );

        let migrator = std::sync::Arc::new(migrator::TierMigrator::new(
            config::TierConfig::default(),
            engine.clone(),
            lru,
            router.affinity_index().clone(),
        ));
        router.set_migrator(migrator);

        // Let health poll populate the cache
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        (engine, router)
    }

    /// Write a vector to the hot-tier partition so the migrator can read it.
    /// The regular INSERT stores the full Entity struct, but the migrator
    /// reads/writes raw vector data via write_tiered/read_tiered.
    fn seed_hot_tier_vector(
        engine: &trondb_core::Engine,
        collection: &str,
        entity_id: &str,
        vector: &[f32],
    ) {
        let id = trondb_core::types::LogicalId::from_string(entity_id);
        let data = rmp_serde::to_vec_named(&vector.to_vec()).unwrap();
        engine
            .write_tiered(collection, &id, trondb_core::location::Tier::Fjall, &data)
            .unwrap();
    }

    // -----------------------------------------------------------------------
    // Tier integration test 1: EXPLAIN TIERS shows distribution
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn explain_tiers_shows_distribution() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, router) = make_tier_test_setup(dir.path()).await;

        // Create collection and insert entities
        engine
            .execute_tql(
                "CREATE COLLECTION venues (\
                    REPRESENTATION default DIMENSIONS 4 METRIC COSINE\
                );",
            )
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'The Shard') \
                 REPRESENTATION default VECTOR [1.0, 0.0, 0.0, 0.0];",
            )
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v2', 'Big Ben') \
                 REPRESENTATION default VECTOR [0.0, 1.0, 0.0, 0.0];",
            )
            .await
            .unwrap();

        // Run EXPLAIN TIERS via the router
        let plan = engine.parse_and_plan("EXPLAIN TIERS venues;").unwrap();
        let result = router.route_and_execute(&plan).await.unwrap();

        // Should have 3 rows: Hot, Warm, Archive
        assert_eq!(result.rows.len(), 3, "expected 3 tier rows, got {}", result.rows.len());

        // Check tier names
        let tier_names: Vec<String> = result
            .rows
            .iter()
            .filter_map(|r| {
                if let Some(trondb_core::types::Value::String(s)) = r.values.get("tier") {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .collect();
        assert!(tier_names.contains(&"Hot".to_string()), "missing Hot tier");
        assert!(tier_names.contains(&"Warm".to_string()), "missing Warm tier");
        assert!(tier_names.contains(&"Archive".to_string()), "missing Archive tier");

        // All entities should be in Hot initially
        let hot_row = result.rows.iter().find(|r| {
            r.values.get("tier") == Some(&trondb_core::types::Value::String("Hot".into()))
        }).expect("Hot tier row");
        assert_eq!(
            hot_row.values.get("entity_count"),
            Some(&trondb_core::types::Value::Int(2)),
            "expected 2 entities in Hot tier"
        );

        let warm_row = result.rows.iter().find(|r| {
            r.values.get("tier") == Some(&trondb_core::types::Value::String("Warm".into()))
        }).expect("Warm tier row");
        assert_eq!(
            warm_row.values.get("entity_count"),
            Some(&trondb_core::types::Value::Int(0)),
            "expected 0 entities in Warm tier"
        );
    }

    // -----------------------------------------------------------------------
    // Tier integration test 2: DEMOTE moves entity to warm
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn demote_moves_entity_to_warm() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, router) = make_tier_test_setup(dir.path()).await;

        engine
            .execute_tql(
                "CREATE COLLECTION venues (\
                    REPRESENTATION default DIMENSIONS 4 METRIC COSINE\
                );",
            )
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'The Shard') \
                 REPRESENTATION default VECTOR [1.0, 0.5, -0.5, -1.0];",
            )
            .await
            .unwrap();

        // Seed the hot-tier partition with the raw vector for the migrator
        seed_hot_tier_vector(&engine, "venues", "v1", &[1.0, 0.5, -0.5, -1.0]);

        // DEMOTE v1 to warm
        let plan = engine.parse_and_plan("DEMOTE 'v1' FROM venues TO WARM;").unwrap();
        let result = router.route_and_execute(&plan).await.unwrap();
        assert_eq!(
            result.rows[0].values.get("status"),
            Some(&trondb_core::types::Value::String("OK".into())),
            "DEMOTE should return OK"
        );

        // Verify via EXPLAIN TIERS
        let plan = engine.parse_and_plan("EXPLAIN TIERS venues;").unwrap();
        let result = router.route_and_execute(&plan).await.unwrap();

        let hot_count = result.rows.iter().find(|r| {
            r.values.get("tier") == Some(&trondb_core::types::Value::String("Hot".into()))
        }).and_then(|r| {
            if let Some(trondb_core::types::Value::Int(n)) = r.values.get("entity_count") {
                Some(*n)
            } else {
                None
            }
        }).unwrap_or(-1);

        let warm_count = result.rows.iter().find(|r| {
            r.values.get("tier") == Some(&trondb_core::types::Value::String("Warm".into()))
        }).and_then(|r| {
            if let Some(trondb_core::types::Value::Int(n)) = r.values.get("entity_count") {
                Some(*n)
            } else {
                None
            }
        }).unwrap_or(-1);

        // The entity that was in the main store (insert) still counts as hot
        // because tier_entity_count scans the partition. After demotion the
        // migrator deletes from the hot-tier partition, but the entity written
        // by insert() is also in the same partition. So hot_count depends on
        // what delete_from_tier actually removed. The seeded vector entry was
        // written with key "entity:v1" and so was the insert(). They share the
        // same key, so delete_from_tier removes it.
        assert_eq!(hot_count, 0, "hot tier should have 0 entities after demotion");
        assert_eq!(warm_count, 1, "warm tier should have 1 entity after demotion");
    }

    // -----------------------------------------------------------------------
    // Tier integration test 3: PROMOTE moves entity from warm to hot
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn promote_moves_entity_from_warm_to_hot() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, router) = make_tier_test_setup(dir.path()).await;

        engine
            .execute_tql(
                "CREATE COLLECTION venues (\
                    REPRESENTATION default DIMENSIONS 4 METRIC COSINE\
                );",
            )
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'The Shard') \
                 REPRESENTATION default VECTOR [1.0, 0.5, -0.5, -1.0];",
            )
            .await
            .unwrap();

        // Seed hot-tier vector data for the migrator to read
        seed_hot_tier_vector(&engine, "venues", "v1", &[1.0, 0.5, -0.5, -1.0]);

        // Demote to warm first
        let plan = engine.parse_and_plan("DEMOTE 'v1' FROM venues TO WARM;").unwrap();
        router.route_and_execute(&plan).await.unwrap();

        // Promote back to hot
        let plan = engine.parse_and_plan("PROMOTE 'v1' FROM venues;").unwrap();
        let result = router.route_and_execute(&plan).await.unwrap();
        assert_eq!(
            result.rows[0].values.get("status"),
            Some(&trondb_core::types::Value::String("OK".into())),
            "PROMOTE should return OK"
        );

        // Verify via EXPLAIN TIERS
        let plan = engine.parse_and_plan("EXPLAIN TIERS venues;").unwrap();
        let result = router.route_and_execute(&plan).await.unwrap();

        let hot_count = result.rows.iter().find(|r| {
            r.values.get("tier") == Some(&trondb_core::types::Value::String("Hot".into()))
        }).and_then(|r| {
            if let Some(trondb_core::types::Value::Int(n)) = r.values.get("entity_count") {
                Some(*n)
            } else {
                None
            }
        }).unwrap_or(-1);

        let warm_count = result.rows.iter().find(|r| {
            r.values.get("tier") == Some(&trondb_core::types::Value::String("Warm".into()))
        }).and_then(|r| {
            if let Some(trondb_core::types::Value::Int(n)) = r.values.get("entity_count") {
                Some(*n)
            } else {
                None
            }
        }).unwrap_or(-1);

        assert_eq!(hot_count, 1, "hot tier should have 1 entity after promotion");
        assert_eq!(warm_count, 0, "warm tier should have 0 entities after promotion");
    }

    // -----------------------------------------------------------------------
    // Task 6: AffinityGroup WAL records appear in unhandled collection
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn affinity_group_records_returned_as_unhandled() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = trondb_core::EngineConfig {
            data_dir: dir.path().join("store"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
            hnsw_snapshot_interval_secs: 0,
        };

        // Create an engine and write an AffinityGroup WAL record
        {
            let (engine, _) = trondb_core::Engine::open(cfg.clone()).await.unwrap();
            let wal = engine.wal_writer();
            // Construct a simple payload using rmp_serde with a map (no serde_json needed)
            let mut payload_map = std::collections::HashMap::new();
            payload_map.insert("group_id", "g1");
            let payload = rmp_serde::to_vec_named(&payload_map).unwrap();
            let tx = wal.next_tx_id();
            wal.append(trondb_wal::RecordType::AffinityGroupCreate, "affinity", tx, 1, payload);
            wal.commit(tx).await.unwrap();
        }

        // Reopen — the AffinityGroupCreate record should appear in unhandled
        let (_, pending) = trondb_core::Engine::open(cfg).await.unwrap();
        let affinity_records: Vec<_> = pending.iter()
            .filter(|r| matches!(r.record_type,
                trondb_wal::RecordType::AffinityGroupCreate
                | trondb_wal::RecordType::AffinityGroupMember
                | trondb_wal::RecordType::AffinityGroupRemove
            ))
            .collect();
        assert!(!affinity_records.is_empty(), "AffinityGroupCreate should be in unhandled records");
    }

    // -----------------------------------------------------------------------
    // Tier integration test 4: quantisation roundtrip preserves vector approx.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn demote_promote_preserves_vector_approximately() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, router) = make_tier_test_setup(dir.path()).await;

        engine
            .execute_tql(
                "CREATE COLLECTION venues (\
                    REPRESENTATION default DIMENSIONS 4 METRIC COSINE\
                );",
            )
            .await
            .unwrap();

        let original_vector: Vec<f32> = vec![1.0, 0.5, -0.5, -1.0];

        engine
            .execute_tql(
                "INSERT INTO venues (id, name) VALUES ('v1', 'Test') \
                 REPRESENTATION default VECTOR [1.0, 0.5, -0.5, -1.0];",
            )
            .await
            .unwrap();

        // Seed hot-tier vector data
        seed_hot_tier_vector(&engine, "venues", "v1", &original_vector);

        // Demote to warm (Float32 -> Int8)
        let plan = engine.parse_and_plan("DEMOTE 'v1' FROM venues TO WARM;").unwrap();
        router.route_and_execute(&plan).await.unwrap();

        // Promote back to hot (Int8 -> Float32)
        let plan = engine.parse_and_plan("PROMOTE 'v1' FROM venues;").unwrap();
        router.route_and_execute(&plan).await.unwrap();

        // Read the vector back from the hot tier
        let entity_id = trondb_core::types::LogicalId::from_string("v1");
        let raw = engine
            .read_tiered("venues", &entity_id, trondb_core::location::Tier::Fjall)
            .unwrap()
            .expect("entity should be back in hot tier");

        let restored: Vec<f32> = rmp_serde::from_slice(&raw).unwrap();

        // Int8 quantisation introduces some error but values should be close
        assert_eq!(
            restored.len(),
            original_vector.len(),
            "vector dimensions should be preserved"
        );
        for (orig, rest) in original_vector.iter().zip(restored.iter()) {
            let diff = (orig - rest).abs();
            assert!(
                diff < 0.05,
                "vector component diverged too much: original={orig}, restored={rest}, diff={diff}"
            );
        }
    }
}
