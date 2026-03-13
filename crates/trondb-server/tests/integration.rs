use std::sync::Arc;
use std::time::Duration;

use trondb_core::planner::*;
use trondb_core::{Engine, EngineConfig};
use trondb_proto::pb;
use trondb_routing::node::{NodeHandle, NodeId, NodeRole};
use trondb_server::config::NodeRoleConfig;
use trondb_server::remote_node::RemoteNode;
use trondb_server::service::TronNodeService;
use trondb_tql::{FieldDecl, FieldList, FieldType, Literal, Metric, RepresentationDecl, VectorLiteral};

/// Create a temporary EngineConfig for testing.
fn test_config() -> EngineConfig {
    let dir = tempfile::tempdir().unwrap().keep();
    EngineConfig {
        data_dir: dir.join("store"),
        wal: trondb_wal::WalConfig {
            wal_dir: dir.join("wal"),
            ..Default::default()
        },
        snapshot_interval_secs: 60,
        hnsw_snapshot_interval_secs: 300,
    }
}

/// Start a gRPC server on a random port, returning the address and server task handle.
async fn start_grpc_server(
    service: TronNodeService,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(pb::tron_node_server::TronNodeServer::new(service))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    // Give the server a moment to start accepting connections.
    tokio::time::sleep(Duration::from_millis(100)).await;

    (addr, handle)
}

/// Connect a RemoteNode to the given address.
async fn connect_remote(addr: std::net::SocketAddr) -> RemoteNode {
    RemoteNode::connect(
        NodeId::from_string("test-remote"),
        NodeRole::ReadReplica,
        format!("http://{addr}"),
    )
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// Test 1: loopback_create_insert_fetch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loopback_create_insert_fetch() {
    // 1. Start Engine and TronNodeService (Primary)
    let (engine, _) = Engine::open(test_config()).await.unwrap();
    let engine = Arc::new(engine);
    let service = TronNodeService::new(engine.clone(), NodeRoleConfig::Primary);

    // 2. Start gRPC server on random port
    let (addr, server_handle) = start_grpc_server(service).await;

    // 3. Connect a RemoteNode
    let remote = connect_remote(addr).await;

    // 4. CREATE COLLECTION with a 3-dim representation
    let create_plan = Plan::CreateCollection(CreateCollectionPlan {
        name: "loopback_coll".into(),
        representations: vec![RepresentationDecl {
            name: "default".into(),
            model: None,
            dimensions: Some(3),
            metric: Metric::Cosine,
            sparse: false,
            fields: vec![],
        }],
        fields: vec![FieldDecl {
            name: "title".into(),
            field_type: FieldType::Text,
        }],
        indexes: vec![],
        vectoriser_config: None,
    });
    let create_result = remote.execute(&create_plan).await.unwrap();
    assert!(
        !create_result.rows.is_empty(),
        "CREATE COLLECTION should return at least one status row"
    );

    // 5. INSERT with a dense vector
    let insert_plan = Plan::Insert(InsertPlan {
        collection: "loopback_coll".into(),
        fields: vec!["title".into()],
        values: vec![Literal::String("hello world".into())],
        vectors: vec![(
            "default".into(),
            VectorLiteral::Dense(vec![1.0, 0.0, 0.0]),
        )],
        collocate_with: None,
        affinity_group: None,
    });
    let insert_result = remote.execute(&insert_plan).await.unwrap();
    assert!(
        !insert_result.rows.is_empty(),
        "INSERT should return at least one status row"
    );

    // 6. FETCH (verify rows returned)
    let fetch_plan = Plan::Fetch(FetchPlan {
        collection: "loopback_coll".into(),
        fields: FieldList::All,
        filter: None,
        order_by: vec![],
        limit: Some(10),
        strategy: FetchStrategy::FullScan,
        hints: vec![],
    });
    let fetch_result = remote.execute(&fetch_plan).await.unwrap();
    assert!(
        !fetch_result.rows.is_empty(),
        "FETCH should return the inserted row"
    );

    // Clean up
    server_handle.abort();
}

// ---------------------------------------------------------------------------
// Test 2: loopback_health_snapshot
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loopback_health_snapshot() {
    // 1. Start Engine and TronNodeService
    let (engine, _) = Engine::open(test_config()).await.unwrap();
    let engine = Arc::new(engine);
    let service = TronNodeService::new(engine.clone(), NodeRoleConfig::Primary);

    // 2. Start gRPC server on random port
    let (addr, server_handle) = start_grpc_server(service).await;

    // 3. Connect a RemoteNode
    let remote = connect_remote(addr).await;

    // 4. Call health_snapshot()
    let health = remote.health_snapshot().await;

    // 5. Verify cpu_utilisation and ram_pressure are in [0.0, 1.0]
    assert!(
        (0.0..=1.0).contains(&health.cpu_utilisation),
        "cpu_utilisation should be in [0.0, 1.0], got {}",
        health.cpu_utilisation,
    );
    assert!(
        (0.0..=1.0).contains(&health.ram_pressure),
        "ram_pressure should be in [0.0, 1.0], got {}",
        health.ram_pressure,
    );

    // Clean up
    server_handle.abort();
}
