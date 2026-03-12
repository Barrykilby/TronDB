use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};
use trondb_core::Engine;
use trondb_core::planner::Plan;
use trondb_core::result::QueryResult;
use trondb_proto::pb;
use trondb_proto::pb::tron_node_server::TronNode;
use trondb_routing::health::{HealthSignal, NodeStatus};
use trondb_routing::node::{NodeId, NodeRole};
use crate::config::{NodeRoleConfig, ReplicationConfig};
use crate::location_stream::spawn_location_stream;
use crate::metrics::SystemMetrics;
use crate::replication::ReplicaTracker;
use crate::stream_health::spawn_health_stream;

pub struct TronNodeService {
    engine: Option<Arc<Engine>>,
    role: NodeRoleConfig,
    primary_channel: Option<tonic::transport::Channel>,  // for write forwarding
    tracker: Option<Arc<ReplicaTracker>>,
    replication_config: ReplicationConfig,
    location_broadcast: Option<broadcast::Sender<pb::LocationUpdateMessage>>,
    system_metrics: Arc<SystemMetrics>,
}

impl TronNodeService {
    pub fn new(engine: Arc<Engine>, role: NodeRoleConfig) -> Self {
        Self {
            engine: Some(engine),
            role,
            primary_channel: None,
            tracker: None,
            replication_config: ReplicationConfig::default(),
            location_broadcast: None,
            system_metrics: Arc::new(SystemMetrics::new()),
        }
    }

    pub fn new_with_tracker(
        engine: Arc<Engine>,
        role: NodeRoleConfig,
        tracker: Arc<ReplicaTracker>,
    ) -> Self {
        Self {
            engine: Some(engine),
            role,
            primary_channel: None,
            tracker: Some(tracker),
            replication_config: ReplicationConfig::default(),
            location_broadcast: None,
            system_metrics: Arc::new(SystemMetrics::new()),
        }
    }

    pub fn new_with_tracker_and_config(
        engine: Arc<Engine>,
        role: NodeRoleConfig,
        tracker: Arc<ReplicaTracker>,
        replication_config: ReplicationConfig,
    ) -> Self {
        Self {
            engine: Some(engine),
            role,
            primary_channel: None,
            tracker: Some(tracker),
            replication_config,
            location_broadcast: None,
            system_metrics: Arc::new(SystemMetrics::new()),
        }
    }

    /// Create a router service with no local engine.
    ///
    /// The router forwards all Execute calls to the primary and returns
    /// minimal health information. It does not support WAL streaming or
    /// location update streaming.
    pub fn new_router(primary_channel: tonic::transport::Channel) -> Self {
        Self {
            engine: None,
            role: NodeRoleConfig::Router,
            primary_channel: Some(primary_channel),
            tracker: None,
            replication_config: ReplicationConfig::default(),
            location_broadcast: None,
            system_metrics: Arc::new(SystemMetrics::new()),
        }
    }

    pub fn with_primary(mut self, channel: tonic::transport::Channel) -> Self {
        self.primary_channel = Some(channel);
        self
    }

    pub fn with_location_broadcast(
        mut self,
        sender: broadcast::Sender<pb::LocationUpdateMessage>,
    ) -> Self {
        self.location_broadcast = Some(sender);
        self
    }

    fn is_write_plan(plan: &Plan) -> bool {
        matches!(plan,
            Plan::Insert(_) | Plan::UpdateEntity(_) | Plan::DeleteEntity(_) |
            Plan::CreateCollection(_) | Plan::CreateEdgeType(_) | Plan::InsertEdge(_) |
            Plan::DeleteEdge(_) | Plan::CreateAffinityGroup(_) |
            Plan::AlterEntityDropAffinity(_) | Plan::Demote(_) | Plan::Promote(_)
        )
    }

    /// Get a reference to the engine, or return a gRPC error if this is a
    /// router node (no engine).
    fn engine_ref(&self) -> Result<&Arc<Engine>, Status> {
        self.engine.as_ref().ok_or_else(|| {
            Status::failed_precondition("no local engine (router node)")
        })
    }
}

#[tonic::async_trait]
impl TronNode for TronNodeService {
    async fn execute(
        &self,
        request: Request<pb::PlanRequest>,
    ) -> Result<Response<pb::QueryResponse>, Status> {
        let proto_plan = request.into_inner();
        let plan = Plan::try_from(proto_plan)
            .map_err(|e| Status::invalid_argument(e))?;

        // Router mode: forward ALL calls to the primary (no local engine)
        if self.engine.is_none() {
            return self.forward_to_primary(&plan).await;
        }
        let engine = self.engine_ref()?;

        // Write forwarding: non-primary nodes forward writes to primary
        if self.role != NodeRoleConfig::Primary && Self::is_write_plan(&plan) {
            return self.forward_to_primary(&plan).await;
        }

        // Semi-synchronous write path (primary only for write plans)
        if self.role == NodeRoleConfig::Primary && Self::is_write_plan(&plan) {
            if let Some(tracker) = &self.tracker {
                let cfg = &self.replication_config;

                // Check require_replica: fail fast if no replicas connected
                if cfg.require_replica && tracker.connected_count() == 0 {
                    return Err(Status::unavailable(
                        "no replicas connected and require_replica is true",
                    ));
                }

                // Capture LSN before execution
                let lsn_before = engine.wal_head_lsn();

                // Execute locally
                let result = engine.execute(&plan).await
                    .map_err(|e| Status::internal(e.to_string()))?;

                // Get LSN after execution; if unchanged, no WAL was written — skip broadcast
                let lsn_after = engine.wal_head_lsn();

                if lsn_after > lsn_before && tracker.connected_count() > 0 {
                    // Broadcast new WAL records to all replicas
                    if let Ok(records) = engine.wal_records_since(lsn_before).await {
                        for record in &records {
                            let proto: pb::WalRecordMessage = record.into();
                            tracker.broadcast(&proto).await;
                        }

                        // Feed LocationUpdate records into the location broadcast channel
                        if let Some(ref loc_tx) = self.location_broadcast {
                            for record in &records {
                                if matches!(record.record_type, trondb_wal::RecordType::LocationUpdate) {
                                    let msg = pb::LocationUpdateMessage {
                                        is_snapshot: false,
                                        payload: record.payload.clone(),
                                        lsn: record.lsn,
                                    };
                                    let _ = loc_tx.send(msg);
                                }
                            }
                        }
                    }

                    // Wait for ack (semi-sync: timeout is non-fatal)
                    let timeout = Duration::from_millis(cfg.ack_timeout_ms);
                    match tracker.wait_for_ack(lsn_after, cfg.min_ack_replicas, timeout).await {
                        Ok(()) => {} // replicas confirmed
                        Err(e) => {
                            // Log warning but still return success (semi-sync, not strict sync)
                            eprintln!("[semi-sync] replication wait timed out: {e}");
                        }
                    }
                }

                let proto_result: pb::QueryResponse = (&result).into();
                return Ok(Response::new(proto_result));
            }
        }

        // Capture LSN before execution for location broadcast
        let lsn_before = engine.wal_head_lsn();

        let result = engine.execute(&plan).await
            .map_err(|e| Status::internal(e.to_string()))?;

        // Feed LocationUpdate WAL records into the location broadcast channel
        if let Some(ref loc_tx) = self.location_broadcast {
            let lsn_after = engine.wal_head_lsn();
            if lsn_after > lsn_before {
                if let Ok(records) = engine.wal_records_since(lsn_before).await {
                    for record in &records {
                        if matches!(record.record_type, trondb_wal::RecordType::LocationUpdate) {
                            let msg = pb::LocationUpdateMessage {
                                is_snapshot: false,
                                payload: record.payload.clone(),
                                lsn: record.lsn,
                            };
                            let _ = loc_tx.send(msg);
                        }
                    }
                }
            }
        }

        let proto_result: pb::QueryResponse = (&result).into();
        Ok(Response::new(proto_result))
    }

    async fn health_snapshot(
        &self,
        _request: Request<pb::Empty>,
    ) -> Result<Response<pb::HealthSignalResponse>, Status> {
        // Router nodes without an engine return a minimal health signal
        if self.engine.is_none() {
            let signal = self.compute_router_health();
            let proto: pb::HealthSignalResponse = (&signal).into();
            return Ok(Response::new(proto));
        }
        let signal = self.compute_local_health();
        let proto: pb::HealthSignalResponse = (&signal).into();
        Ok(Response::new(proto))
    }

    // StreamHealth (Task 8), StreamWal (Task 12), StreamLocationUpdates (Task 15)

    type StreamHealthStream =
        tokio_stream::wrappers::ReceiverStream<Result<pb::HealthSignalResponse, Status>>;

    async fn stream_health(
        &self,
        _: Request<pb::Empty>,
    ) -> Result<Response<Self::StreamHealthStream>, Status> {
        let engine = self.engine_ref()?;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        spawn_health_stream(
            engine.clone(),
            self.role.clone(),
            tx,
            Duration::from_millis(200),
        );
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    type StreamWalStream =
        tokio_stream::wrappers::ReceiverStream<Result<pb::WalRecordMessage, Status>>;

    async fn stream_wal(
        &self,
        request: Request<tonic::Streaming<pb::WalAck>>,
    ) -> Result<Response<Self::StreamWalStream>, Status> {
        let tracker = self.tracker.as_ref().ok_or_else(|| {
            Status::failed_precondition("WAL streaming not available on this node")
        })?;

        let mut client_stream = request.into_inner();

        // Step 1: Read the initial WalAck to get the replica's last confirmed LSN
        let initial_ack = client_stream
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("no initial WalAck received"))?
            .map_err(|e| Status::internal(format!("error reading initial ack: {e}")))?;
        let last_confirmed_lsn = initial_ack.confirmed_lsn;

        // Generate a unique replica ID
        let replica_id = format!("replica-{}", uuid_v4_simple());

        // Step 2: Create channel and register replica in tracker
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<pb::WalRecordMessage, Status>>(256);
        let wal_tx = {
            // Create a sender that wraps records in Ok for the gRPC stream
            let (wal_sender, mut wal_receiver) =
                tokio::sync::mpsc::channel::<pb::WalRecordMessage>(256);

            // Spawn forwarder: wal_receiver -> tx (wrapping in Ok)
            let tx_clone = tx.clone();
            tokio::spawn(async move {
                while let Some(record) = wal_receiver.recv().await {
                    if tx_clone.send(Ok(record)).await.is_err() {
                        break;
                    }
                }
            });

            wal_sender
        };

        tracker.register(replica_id.clone(), wal_tx);

        // Step 3: Catch-up — read WAL records after last_confirmed_lsn
        let engine = self.engine_ref()?.clone();
        let tx_catchup = tx.clone();
        let catchup_lsn = last_confirmed_lsn;

        tokio::spawn(async move {
            match engine.wal_records_since(catchup_lsn).await {
                Ok(records) => {
                    for record in &records {
                        let proto: pb::WalRecordMessage = record.into();
                        if tx_catchup.send(Ok(proto)).await.is_err() {
                            return; // client disconnected
                        }
                    }
                }
                Err(e) => {
                    let _ = tx_catchup
                        .send(Err(Status::internal(format!("WAL catch-up failed: {e}"))))
                        .await;
                }
            }
        });

        // Step 4: Background task to read WalAck messages from client stream
        let tracker_clone = tracker.clone();
        let replica_id_clone = replica_id.clone();
        let tracker_for_cleanup = tracker.clone();
        let replica_id_for_cleanup = replica_id.clone();

        tokio::spawn(async move {
            while let Some(result) = client_stream.next().await {
                match result {
                    Ok(ack) => {
                        tracker_clone.update_confirmed_lsn(
                            &replica_id_clone,
                            ack.confirmed_lsn,
                        );
                    }
                    Err(_) => break, // client error — stop processing acks
                }
            }
            // Client disconnected — clean up
            tracker_for_cleanup.disconnect(&replica_id_for_cleanup);
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    type StreamLocationUpdatesStream =
        tokio_stream::wrappers::ReceiverStream<Result<pb::LocationUpdateMessage, Status>>;

    async fn stream_location_updates(
        &self,
        _: Request<pb::Empty>,
    ) -> Result<Response<Self::StreamLocationUpdatesStream>, Status> {
        let engine = self.engine_ref()?;
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        // Subscribe to the broadcast channel (if available) before taking
        // the snapshot so that we don't miss any deltas written between
        // the snapshot and the first recv.
        let bcast_rx = self.location_broadcast.as_ref().map(|s| s.subscribe());

        spawn_location_stream(engine.clone(), bcast_rx, tx);

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

impl TronNodeService {
    /// Compute a HealthSignal from the local engine state, using real
    /// CPU/RAM metrics from `sysinfo`.
    fn compute_local_health(&self) -> HealthSignal {
        let entity_count = self.engine.as_ref()
            .map(|e| e.entity_count() as u64)
            .unwrap_or(0);
        let cpu = self.system_metrics.cpu_utilisation();
        let ram = self.system_metrics.ram_pressure();
        HealthSignal {
            node_id: NodeId::from_string("local"),
            node_role: match self.role {
                NodeRoleConfig::Primary => NodeRole::Primary,
                NodeRoleConfig::Replica => NodeRole::ReadReplica,
                NodeRoleConfig::Router => NodeRole::Router,
            },
            signal_ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64,
            sequence: 0,
            cpu_utilisation: cpu,
            ram_pressure: ram,
            hot_entity_count: entity_count,
            hot_tier_capacity: 100_000,
            warm_entity_count: 0,
            archive_entity_count: 0,
            queue_depth: 0,
            queue_capacity: 1000,
            hnsw_p50_ms: 0.0,
            hnsw_p99_ms: 0.0,
            replica_lag_ms: None,
            load_score: 0.0,
            status: NodeStatus::Healthy,
        }
    }

    /// Compute a minimal HealthSignal for router nodes (no local engine).
    fn compute_router_health(&self) -> HealthSignal {
        let cpu = self.system_metrics.cpu_utilisation();
        let ram = self.system_metrics.ram_pressure();
        HealthSignal {
            node_id: NodeId::from_string("local"),
            node_role: NodeRole::Router,
            signal_ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64,
            sequence: 0,
            cpu_utilisation: cpu,
            ram_pressure: ram,
            hot_entity_count: 0,
            hot_tier_capacity: 0,
            warm_entity_count: 0,
            archive_entity_count: 0,
            queue_depth: 0,
            queue_capacity: 1000,
            hnsw_p50_ms: 0.0,
            hnsw_p99_ms: 0.0,
            replica_lag_ms: None,
            load_score: 0.0,
            status: NodeStatus::Healthy,
        }
    }

    /// Forward a write plan to the primary. Returns Err if no primary channel configured.
    async fn forward_to_primary(
        &self,
        plan: &Plan,
    ) -> Result<Response<pb::QueryResponse>, Status> {
        let channel = self.primary_channel.as_ref().ok_or_else(|| {
            Status::unavailable("no primary configured for write forwarding")
        })?;
        let mut client = pb::tron_node_client::TronNodeClient::new(channel.clone());
        let proto_plan: pb::PlanRequest = plan.into();
        client.execute(tonic::Request::new(proto_plan)).await
    }
}

/// Generate a simple unique ID without pulling in the uuid crate.
fn uuid_v4_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tid = std::thread::current().id();
    format!("{ts:x}-{tid:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use trondb_core::planner::*;

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

    #[tokio::test]
    async fn execute_create_collection_via_grpc() {
        // Spin up in-process service
        let (engine, _) = Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);
        let service = TronNodeService::new(engine.clone(), NodeRoleConfig::Primary);

        let plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "test_coll".into(),
            representations: vec![],
            fields: vec![],
            indexes: vec![],
        });
        let request = tonic::Request::new((&plan).into());
        let response = service.execute(request).await.unwrap();
        let result = QueryResult::try_from(response.into_inner()).unwrap();
        assert_eq!(result.rows.len(), 1); // status row
    }

    #[tokio::test]
    async fn wal_stream_catches_up_replica() {
        use tokio_stream::StreamExt;

        // Step 1: Create engine and insert data to generate WAL records
        let (engine, _) = Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);

        // Execute a CREATE COLLECTION to generate WAL records
        let plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "wal_test".into(),
            representations: vec![],
            fields: vec![],
            indexes: vec![],
        });
        engine.execute(&plan).await.unwrap();

        // Verify the engine has WAL records
        let head_lsn = engine.wal_head_lsn();
        assert!(head_lsn > 0, "expected WAL records after insert, head_lsn={head_lsn}");

        // Step 2: Create service with tracker
        let tracker = Arc::new(ReplicaTracker::new());
        let service = TronNodeService::new_with_tracker(
            engine.clone(),
            NodeRoleConfig::Primary,
            tracker.clone(),
        );

        // Step 3: Start gRPC server on a random port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
            tonic::transport::Server::builder()
                .add_service(pb::tron_node_server::TronNodeServer::new(service))
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });

        // Give the server a moment to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Step 4: Connect as a replica client
        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = pb::tron_node_client::TronNodeClient::new(channel);

        // Send initial WalAck with LSN=0 (request all records)
        let (ack_tx, ack_rx) = tokio::sync::mpsc::channel::<pb::WalAck>(16);
        ack_tx
            .send(pb::WalAck { confirmed_lsn: 0 })
            .await
            .unwrap();

        let ack_stream = tokio_stream::wrappers::ReceiverStream::new(ack_rx);
        let response = client.stream_wal(ack_stream).await.unwrap();
        let mut wal_stream = response.into_inner();

        // Step 5: Receive catch-up WAL records
        let mut received = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(500), wal_stream.next()).await {
                Ok(Some(Ok(record))) => {
                    received.push(record);
                }
                Ok(Some(Err(e))) => {
                    panic!("error receiving WAL record: {e}");
                }
                Ok(None) => break,             // stream ended
                Err(_) => break,               // timeout — no more records coming
            }
        }

        // Step 6: Verify records have LSN > 0
        assert!(
            !received.is_empty(),
            "expected catch-up WAL records but received none"
        );
        for record in &received {
            assert!(record.lsn > 0, "expected LSN > 0, got {}", record.lsn);
        }

        // Verify at least one record has the max LSN (head_lsn)
        let max_received_lsn = received.iter().map(|r| r.lsn).max().unwrap();
        assert!(
            max_received_lsn >= head_lsn,
            "expected max received LSN >= head_lsn ({head_lsn}), got {max_received_lsn}"
        );

        // Clean up
        server_task.abort();
    }

    #[tokio::test]
    async fn wal_stream_without_tracker_returns_error() {
        // Service without a tracker should return a gRPC error
        let (engine, _) = Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);
        let service = TronNodeService::new(engine.clone(), NodeRoleConfig::Primary);

        // Start a gRPC server
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
            tonic::transport::Server::builder()
                .add_service(pb::tron_node_server::TronNodeServer::new(service))
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = pb::tron_node_client::TronNodeClient::new(channel);

        // Send initial WalAck
        let (ack_tx, ack_rx) = tokio::sync::mpsc::channel::<pb::WalAck>(1);
        ack_tx.send(pb::WalAck { confirmed_lsn: 0 }).await.unwrap();
        let ack_stream = tokio_stream::wrappers::ReceiverStream::new(ack_rx);

        let result = client.stream_wal(ack_stream).await;
        assert!(result.is_err(), "expected error when tracker is absent");
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);

        server_task.abort();
    }

    #[tokio::test]
    async fn semi_sync_write_waits_for_replica_ack() {
        // Setup engine and tracker
        let (engine, _) = Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);
        let tracker = Arc::new(ReplicaTracker::new());

        // Use a short ack_timeout so the test doesn't take forever on failure
        let replication_config = crate::config::ReplicationConfig {
            min_ack_replicas: 1,
            ack_timeout_ms: 2000, // 2 second timeout
            require_replica: false,
        };

        // Register a fake replica channel: we'll simulate confirmation after 50ms
        let (replica_tx, _replica_rx) = tokio::sync::mpsc::channel::<pb::WalRecordMessage>(64);
        tracker.register("fake-replica".into(), replica_tx);

        // Clone tracker for the background confirmer task
        let tracker_for_ack = tracker.clone();

        // First create a collection so we have a schema for INSERT
        let create_plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "semi_sync_test".into(),
            representations: vec![],
            fields: vec![],
            indexes: vec![],
        });
        engine.execute(&create_plan).await.unwrap();

        // Build the service with tracker and replication config
        let service = TronNodeService::new_with_tracker_and_config(
            engine.clone(),
            NodeRoleConfig::Primary,
            tracker.clone(),
            replication_config,
        );

        // Spawn a task that confirms the replica ack after 50ms delay.
        // It will watch the WAL head LSN and acknowledge it.
        let engine_for_ack = engine.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            // Acknowledge the current WAL head LSN
            let lsn = engine_for_ack.wal_head_lsn();
            // Give it a bit more time so the write plan's LSN is covered
            let confirm_lsn = lsn + 10; // generously above any new records
            tracker_for_ack.update_confirmed_lsn("fake-replica", confirm_lsn);
        });

        // Measure the time the write takes — it should block until the replica confirms
        let plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "another_collection".into(),
            representations: vec![],
            fields: vec![],
            indexes: vec![],
        });
        let request = tonic::Request::new((&plan).into());

        let start = std::time::Instant::now();
        let response = service.execute(request).await.unwrap();
        let elapsed = start.elapsed();

        // Verify the response is valid
        let result = QueryResult::try_from(response.into_inner()).unwrap();
        assert_eq!(result.rows.len(), 1, "expected status row from CreateCollection");

        // The write should have waited for the replica ack (~50ms delay)
        assert!(
            elapsed >= Duration::from_millis(40),
            "expected execute to wait for replica ack (>=40ms), but it took only {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn semi_sync_require_replica_fails_when_no_replicas() {
        let (engine, _) = Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);
        let tracker = Arc::new(ReplicaTracker::new()); // no replicas registered

        let replication_config = crate::config::ReplicationConfig {
            min_ack_replicas: 1,
            ack_timeout_ms: 1000,
            require_replica: true, // strict: must have at least one replica
        };

        let service = TronNodeService::new_with_tracker_and_config(
            engine.clone(),
            NodeRoleConfig::Primary,
            tracker.clone(),
            replication_config,
        );

        let plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "strict_coll".into(),
            representations: vec![],
            fields: vec![],
            indexes: vec![],
        });
        let request = tonic::Request::new((&plan).into());
        let result = service.execute(request).await;

        assert!(result.is_err(), "expected error when require_replica=true and no replicas");
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unavailable);
    }

    #[tokio::test]
    async fn semi_sync_no_tracker_succeeds_immediately() {
        // Without a tracker, writes should succeed immediately (single-node mode)
        let (engine, _) = Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);
        let service = TronNodeService::new(engine.clone(), NodeRoleConfig::Primary);

        let plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "no_tracker_coll".into(),
            representations: vec![],
            fields: vec![],
            indexes: vec![],
        });
        let request = tonic::Request::new((&plan).into());

        let start = std::time::Instant::now();
        let response = service.execute(request).await.unwrap();
        let elapsed = start.elapsed();

        let result = QueryResult::try_from(response.into_inner()).unwrap();
        assert_eq!(result.rows.len(), 1);

        // Should complete quickly without any replication wait
        assert!(
            elapsed < Duration::from_millis(500),
            "expected fast execution without tracker, took {:?}",
            elapsed
        );
    }
}
