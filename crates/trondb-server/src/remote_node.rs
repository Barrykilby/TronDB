use async_trait::async_trait;
use tonic::transport::Channel;
use trondb_core::error::EngineError;
use trondb_core::planner::Plan;
use trondb_core::result::QueryResult;
use trondb_proto::pb;
use trondb_proto::pb::tron_node_client::TronNodeClient;
use trondb_routing::health::HealthSignal;
use trondb_routing::node::{NodeHandle, NodeId, NodeRole};

// ---------------------------------------------------------------------------
// RemoteNode — NodeHandle backed by a gRPC channel
// ---------------------------------------------------------------------------

pub struct RemoteNode {
    node_id: NodeId,
    role: NodeRole,
    client: TronNodeClient<Channel>,
}

impl RemoteNode {
    pub async fn connect(
        node_id: NodeId,
        role: NodeRole,
        addr: String,
    ) -> Result<Self, tonic::transport::Error> {
        let channel = Channel::from_shared(addr)
            .expect("invalid URI")
            .connect()
            .await?;
        Ok(Self {
            node_id,
            role,
            client: TronNodeClient::new(channel),
        })
    }
}

#[async_trait]
impl NodeHandle for RemoteNode {
    fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    fn node_role(&self) -> NodeRole {
        self.role
    }

    async fn execute(&self, plan: &Plan) -> Result<QueryResult, EngineError> {
        let proto_plan: pb::PlanRequest = plan.into();
        let mut client = self.client.clone();
        let response = client
            .execute(tonic::Request::new(proto_plan))
            .await
            .map_err(|e| EngineError::Internal(e.to_string()))?;
        QueryResult::try_from(response.into_inner())
            .map_err(EngineError::Internal)
    }

    async fn health_snapshot(&self) -> HealthSignal {
        let mut client = self.client.clone();
        match client
            .health_snapshot(tonic::Request::new(pb::Empty {}))
            .await
        {
            Ok(response) => HealthSignal::try_from(response.into_inner())
                .unwrap_or_else(|_| HealthSignal::faulted(self.node_id.clone())),
            Err(_) => HealthSignal::faulted(self.node_id.clone()),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use trondb_core::planner::{CreateCollectionPlan, Plan};
    use trondb_core::Engine;
    use trondb_proto::pb;
    use trondb_routing::node::NodeRole;

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

    // Integration test: start a real TronNodeService on localhost,
    // connect a RemoteNode, and execute a plan through it.
    #[tokio::test]
    async fn remote_node_execute_via_loopback() {
        // Start in-process gRPC server
        let (engine, _) = Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);
        let service =
            crate::service::TronNodeService::new(engine.clone(), crate::config::NodeRoleConfig::Primary);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(pb::tron_node_server::TronNodeServer::new(service))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Connect RemoteNode
        let remote = RemoteNode::connect(
            NodeId::from_string("remote-1"),
            NodeRole::ReadReplica,
            format!("http://{addr}"),
        )
        .await
        .unwrap();

        // Execute CREATE COLLECTION through RemoteNode
        let plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "remote_test".into(),
            representations: vec![],
            fields: vec![],
            indexes: vec![],
        vectoriser_config: None,
        });
        let result = remote.execute(&plan).await.unwrap();
        assert_eq!(result.rows.len(), 1);

        server_handle.abort();
    }
}
