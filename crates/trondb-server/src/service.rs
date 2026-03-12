use std::sync::Arc;
use std::time::Duration;
use tonic::{Request, Response, Status};
use trondb_core::Engine;
use trondb_core::planner::Plan;
use trondb_core::result::QueryResult;
use trondb_proto::pb;
use trondb_proto::pb::tron_node_server::TronNode;
use trondb_routing::health::{HealthSignal, NodeStatus};
use trondb_routing::node::{NodeId, NodeRole};
use crate::config::NodeRoleConfig;
use crate::stream_health::spawn_health_stream;

pub struct TronNodeService {
    engine: Arc<Engine>,
    role: NodeRoleConfig,
    primary_channel: Option<tonic::transport::Channel>,  // for write forwarding
}

impl TronNodeService {
    pub fn new(engine: Arc<Engine>, role: NodeRoleConfig) -> Self {
        Self { engine, role, primary_channel: None }
    }

    pub fn with_primary(mut self, channel: tonic::transport::Channel) -> Self {
        self.primary_channel = Some(channel);
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

        // Write forwarding: non-primary nodes forward writes to primary
        if self.role != NodeRoleConfig::Primary && Self::is_write_plan(&plan) {
            return self.forward_to_primary(&plan).await;
        }

        let result = self.engine.execute(&plan).await
            .map_err(|e| Status::internal(e.to_string()))?;
        let proto_result: pb::QueryResponse = (&result).into();
        Ok(Response::new(proto_result))
    }

    async fn health_snapshot(
        &self,
        _request: Request<pb::Empty>,
    ) -> Result<Response<pb::HealthSignalResponse>, Status> {
        let signal = self.compute_local_health();
        let proto: pb::HealthSignalResponse = (&signal).into();
        Ok(Response::new(proto))
    }

    // StreamHealth — real implementation (Task 8)
    // StreamWal, StreamLocationUpdates — stub implementations (Tasks 12, 15)

    type StreamHealthStream =
        tokio_stream::wrappers::ReceiverStream<Result<pb::HealthSignalResponse, Status>>;

    async fn stream_health(
        &self,
        _: Request<pb::Empty>,
    ) -> Result<Response<Self::StreamHealthStream>, Status> {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        spawn_health_stream(
            self.engine.clone(),
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
        _: Request<tonic::Streaming<pb::WalAck>>,
    ) -> Result<Response<Self::StreamWalStream>, Status> {
        Err(Status::unimplemented("implemented in Task 11"))
    }

    type StreamLocationUpdatesStream =
        tokio_stream::wrappers::ReceiverStream<Result<pb::LocationUpdateMessage, Status>>;

    async fn stream_location_updates(
        &self,
        _: Request<pb::Empty>,
    ) -> Result<Response<Self::StreamLocationUpdatesStream>, Status> {
        Err(Status::unimplemented("implemented in Task 14"))
    }
}

impl TronNodeService {
    /// Compute a HealthSignal from the local engine state.
    /// Uses estimated values until Task 17 (real metrics) replaces them.
    fn compute_local_health(&self) -> HealthSignal {
        let entity_count = self.engine.entity_count() as u64;
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
            cpu_utilisation: 0.0, // placeholder until Task 17
            ram_pressure: entity_count as f32 / 100_000.0,
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
}
