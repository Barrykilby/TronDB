//! StreamHealth push RPC — background task that periodically emits HealthSignal
//! proto messages on a channel, which is forwarded to gRPC clients as a server-
//! streaming response.

use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tonic::Status;
use trondb_core::Engine;
use trondb_proto::pb;
use trondb_routing::health::{HealthSignal, NodeStatus};
use trondb_routing::node::{NodeId, NodeRole};
use crate::config::NodeRoleConfig;

/// Spawn a background task that periodically computes a [`HealthSignal`] from
/// `engine`, converts it to [`pb::HealthSignalResponse`], wraps it in
/// `Ok(...)`, and sends it on `tx`.
///
/// The task loops forever (sleeping `interval` between iterations) until it is
/// aborted or the receiver side of the channel is dropped (detected via a
/// `send` error).
///
/// # Returns
/// A [`JoinHandle`] that the caller can `.abort()` to stop the stream.
pub fn spawn_health_stream(
    engine: Arc<Engine>,
    role: NodeRoleConfig,
    tx: tokio::sync::mpsc::Sender<Result<pb::HealthSignalResponse, Status>>,
    interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut sequence: u64 = 0;
        loop {
            tokio::time::sleep(interval).await;

            let signal = compute_health(&engine, &role, sequence);
            let proto: pb::HealthSignalResponse = (&signal).into();

            if tx.send(Ok(proto)).await.is_err() {
                // Receiver dropped — client disconnected; stop the task.
                break;
            }

            sequence = sequence.wrapping_add(1);
        }
    })
}

/// Build a [`HealthSignal`] from current engine state.
fn compute_health(engine: &Engine, role: &NodeRoleConfig, sequence: u64) -> HealthSignal {
    let entity_count = engine.entity_count() as u64;
    HealthSignal {
        node_id: NodeId::from_string("local"),
        node_role: match role {
            NodeRoleConfig::Primary => NodeRole::Primary,
            NodeRoleConfig::Replica => NodeRole::ReadReplica,
            NodeRoleConfig::Router => NodeRole::Router,
        },
        signal_ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64,
        sequence,
        cpu_utilisation: 0.0,
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt;

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
    async fn stream_health_sends_periodic_signals() {
        let (engine, _) = Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let handle = spawn_health_stream(
            engine.clone(),
            NodeRoleConfig::Primary,
            tx,
            Duration::from_millis(50),
        );

        let mut stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let first = tokio::time::timeout(Duration::from_secs(1), stream.next()).await;
        assert!(first.is_ok(), "timed out waiting for first health signal");
        let msg = first.unwrap();
        assert!(msg.is_some(), "stream ended before first signal");
        let result = msg.unwrap();
        assert!(result.is_ok(), "expected Ok signal, got: {:?}", result);

        handle.abort();
    }

    #[tokio::test]
    async fn stream_health_increments_sequence() {
        let (engine, _) = Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let handle = spawn_health_stream(
            engine.clone(),
            NodeRoleConfig::Primary,
            tx,
            Duration::from_millis(20),
        );

        let mut stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let first = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let second = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        assert_eq!(first.sequence, 0);
        assert_eq!(second.sequence, 1);

        handle.abort();
    }

    #[tokio::test]
    async fn stream_health_stops_when_receiver_dropped() {
        let (engine, _) = Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let handle = spawn_health_stream(
            engine.clone(),
            NodeRoleConfig::Primary,
            tx,
            Duration::from_millis(20),
        );

        drop(rx); // drop the receiver — task should stop on next send attempt

        // Give the task time to detect the closed channel and exit.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(handle.is_finished(), "task should have stopped after receiver was dropped");
    }
}
