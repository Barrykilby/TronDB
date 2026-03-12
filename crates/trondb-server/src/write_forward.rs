/// Write-forwarding integration tests.
///
/// These tests verify that a replica (or router) node correctly proxies write
/// plans to the primary over gRPC, and that the primary executes them.
#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tonic::transport::Channel;
    use trondb_core::planner::{CreateCollectionPlan, Plan};
    use trondb_core::result::QueryResult;
    use trondb_core::Engine;
    use trondb_proto::pb;

    use crate::config::NodeRoleConfig;
    use crate::service::TronNodeService;

    /// Create a temporary EngineConfig for testing.
    fn test_config() -> trondb_core::EngineConfig {
        let dir = tempfile::tempdir().unwrap().keep();
        trondb_core::EngineConfig {
            data_dir: dir.join("store"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 60,
            hnsw_snapshot_interval_secs: 300,
        }
    }

    /// Start a gRPC server for the given service on a random port.
    /// Returns (address, server join handle).
    async fn start_grpc_server(
        service: TronNodeService,
    ) -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
            tonic::transport::Server::builder()
                .add_service(pb::tron_node_server::TronNodeServer::new(service))
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });
        // Give the server a moment to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;
        (addr, handle)
    }

    /// Connect a gRPC channel to the given address.
    async fn connect_channel(addr: std::net::SocketAddr) -> Channel {
        Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn forward_create_collection_from_replica_to_primary() {
        // 1. Start the primary with its own engine.
        let (primary_engine, _) = Engine::open(test_config()).await.unwrap();
        let primary_engine = Arc::new(primary_engine);
        let primary_service =
            TronNodeService::new(primary_engine.clone(), NodeRoleConfig::Primary);
        let (primary_addr, primary_handle) = start_grpc_server(primary_service).await;

        // 2. Create a replica service pointing to the primary.
        let primary_channel = connect_channel(primary_addr).await;
        let (replica_engine, _) = Engine::open(test_config()).await.unwrap();
        let replica_engine = Arc::new(replica_engine);
        let replica_service = TronNodeService::new(replica_engine.clone(), NodeRoleConfig::Replica)
            .with_primary(primary_channel);
        let (replica_addr, replica_handle) = start_grpc_server(replica_service).await;

        // 3. Send a CreateCollection write to the replica via gRPC.
        let replica_channel = connect_channel(replica_addr).await;
        let mut client = pb::tron_node_client::TronNodeClient::new(replica_channel);

        let plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "forwarded_coll".into(),
            representations: vec![],
            fields: vec![],
            indexes: vec![],
        });
        let proto_plan: pb::PlanRequest = (&plan).into();
        let response = client
            .execute(tonic::Request::new(proto_plan))
            .await
            .expect("replica should forward the write successfully");

        // 4. The response should contain a valid QueryResult.
        let result = QueryResult::try_from(response.into_inner()).unwrap();
        assert_eq!(result.rows.len(), 1, "expected one status row from CreateCollection");

        // 5. Verify the collection now exists on the *primary* engine.
        let collections = primary_engine.collections();
        assert!(
            collections.contains(&"forwarded_coll".to_string()),
            "expected 'forwarded_coll' on primary engine, found: {collections:?}"
        );

        // 6. The replica engine should NOT have the collection (it was forwarded).
        let replica_collections = replica_engine.collections();
        assert!(
            !replica_collections.contains(&"forwarded_coll".to_string()),
            "replica should not have the collection locally — it forwarded the write"
        );

        // Cleanup
        primary_handle.abort();
        replica_handle.abort();
    }

    #[tokio::test]
    async fn forward_from_router_to_primary() {
        // Router nodes should also forward writes to primary.
        let (primary_engine, _) = Engine::open(test_config()).await.unwrap();
        let primary_engine = Arc::new(primary_engine);
        let primary_service =
            TronNodeService::new(primary_engine.clone(), NodeRoleConfig::Primary);
        let (primary_addr, primary_handle) = start_grpc_server(primary_service).await;

        let primary_channel = connect_channel(primary_addr).await;
        let (router_engine, _) = Engine::open(test_config()).await.unwrap();
        let router_engine = Arc::new(router_engine);
        let router_service = TronNodeService::new(router_engine.clone(), NodeRoleConfig::Router)
            .with_primary(primary_channel);
        let (router_addr, router_handle) = start_grpc_server(router_service).await;

        let router_channel = connect_channel(router_addr).await;
        let mut client = pb::tron_node_client::TronNodeClient::new(router_channel);

        let plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "router_forwarded".into(),
            representations: vec![],
            fields: vec![],
            indexes: vec![],
        });
        let proto_plan: pb::PlanRequest = (&plan).into();
        let response = client
            .execute(tonic::Request::new(proto_plan))
            .await
            .expect("router should forward the write successfully");

        let result = QueryResult::try_from(response.into_inner()).unwrap();
        assert_eq!(result.rows.len(), 1);

        assert!(
            primary_engine.collections().contains(&"router_forwarded".to_string()),
            "expected 'router_forwarded' on primary engine"
        );

        primary_handle.abort();
        router_handle.abort();
    }

    #[tokio::test]
    async fn forward_without_primary_channel_returns_unavailable() {
        // A replica with no primary channel configured should return UNAVAILABLE.
        let (engine, _) = Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);
        // No `.with_primary(...)` — intentionally missing.
        let service = TronNodeService::new(engine.clone(), NodeRoleConfig::Replica);
        let (addr, server_handle) = start_grpc_server(service).await;

        let channel = connect_channel(addr).await;
        let mut client = pb::tron_node_client::TronNodeClient::new(channel);

        let plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "should_fail".into(),
            representations: vec![],
            fields: vec![],
            indexes: vec![],
        });
        let proto_plan: pb::PlanRequest = (&plan).into();
        let result = client.execute(tonic::Request::new(proto_plan)).await;

        assert!(result.is_err(), "expected error when no primary channel is configured");
        let status = result.unwrap_err();
        assert_eq!(
            status.code(),
            tonic::Code::Unavailable,
            "expected UNAVAILABLE, got {:?}",
            status.code()
        );

        server_handle.abort();
    }

    #[tokio::test]
    async fn primary_executes_writes_locally() {
        // Writes sent directly to a primary should execute locally (no forwarding).
        let (engine, _) = Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);
        let service = TronNodeService::new(engine.clone(), NodeRoleConfig::Primary);
        let (addr, server_handle) = start_grpc_server(service).await;

        let channel = connect_channel(addr).await;
        let mut client = pb::tron_node_client::TronNodeClient::new(channel);

        let plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "primary_local".into(),
            representations: vec![],
            fields: vec![],
            indexes: vec![],
        });
        let proto_plan: pb::PlanRequest = (&plan).into();
        let response = client
            .execute(tonic::Request::new(proto_plan))
            .await
            .expect("primary should handle writes locally");

        let result = QueryResult::try_from(response.into_inner()).unwrap();
        assert_eq!(result.rows.len(), 1);

        assert!(
            engine.collections().contains(&"primary_local".to_string()),
            "primary should have the collection locally"
        );

        server_handle.abort();
    }
}
