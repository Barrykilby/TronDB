//! Location Table streaming — delivers an initial snapshot followed by
//! incremental delta messages over a gRPC server-stream.
//!
//! The primary broadcasts `LocationUpdateMessage` deltas via a
//! `tokio::sync::broadcast` channel.  When a router (or any client) calls
//! `StreamLocationUpdates`, this module:
//!
//! 1. Takes a point-in-time snapshot of the location table.
//! 2. Sends it as the first message (`is_snapshot = true`).
//! 3. Subscribes to the broadcast channel and forwards every subsequent
//!    delta (`is_snapshot = false`) until the client disconnects.

use std::sync::Arc;
use tokio::sync::broadcast;
use tonic::Status;
use trondb_core::Engine;
use trondb_proto::pb;

/// Spawn a background task that sends location updates on `tx`.
///
/// * First message: full snapshot from `engine.location().snapshot(...)`.
/// * Subsequent messages: deltas received from `bcast_rx`.
///
/// The task terminates when the mpsc `tx` is closed (client disconnected)
/// or the broadcast sender is dropped.
pub fn spawn_location_stream(
    engine: Arc<Engine>,
    bcast_rx: Option<broadcast::Receiver<pb::LocationUpdateMessage>>,
    tx: tokio::sync::mpsc::Sender<Result<pb::LocationUpdateMessage, Status>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // ── Step 1: Snapshot ────────────────────────────────────────────
        let lsn = engine.wal_head_lsn();
        let payload = match engine.location().snapshot(lsn) {
            Ok(bytes) => bytes,
            Err(e) => {
                let _ = tx
                    .send(Err(Status::internal(format!(
                        "location snapshot failed: {e}"
                    ))))
                    .await;
                return;
            }
        };

        let snapshot_msg = pb::LocationUpdateMessage {
            is_snapshot: true,
            payload,
            lsn,
        };

        if tx.send(Ok(snapshot_msg)).await.is_err() {
            return; // client disconnected
        }

        // ── Step 2: Deltas ─────────────────────────────────────────────
        if let Some(mut rx) = bcast_rx {
            loop {
                match rx.recv().await {
                    Ok(delta) => {
                        if tx.send(Ok(delta)).await.is_err() {
                            break; // client disconnected
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // The client fell behind.  Log and continue — the
                        // next message it receives may have a gap, but the
                        // router can request a fresh snapshot if needed.
                        eprintln!(
                            "[location_stream] receiver lagged by {n} messages"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break; // sender dropped — stream ends
                    }
                }
            }
        }
        // If there is no broadcast channel (snapshot-only mode), we simply
        // return after sending the snapshot.  The gRPC stream will end
        // naturally once the mpsc sender is dropped.
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio_stream::StreamExt;

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
    async fn location_stream_sends_snapshot_then_deltas() {
        // 1. Start an engine and insert some data so the location table is
        //    non-empty.
        let (engine, _) = trondb_core::Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);

        use trondb_core::planner::{CreateCollectionPlan, Plan};
        let plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "loc_stream_test".into(),
            representations: vec![],
            fields: vec![],
            indexes: vec![],
        });
        engine.execute(&plan).await.unwrap();

        // 2. Create the broadcast channel that simulates delta delivery.
        let (bcast_tx, bcast_rx) = broadcast::channel::<pb::LocationUpdateMessage>(64);

        // 3. Set up the mpsc channel and spawn the stream.
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let _handle = spawn_location_stream(
            engine.clone(),
            Some(bcast_rx),
            tx,
        );

        let mut stream = tokio_stream::wrappers::ReceiverStream::new(rx);

        // 4. First message must be a snapshot.
        let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for snapshot")
            .expect("stream ended before snapshot")
            .expect("snapshot message was an error");

        assert!(first.is_snapshot, "first message should be a snapshot");
        assert!(!first.payload.is_empty(), "snapshot payload should not be empty");
        assert!(first.lsn > 0, "snapshot lsn should be > 0 after insert");

        // 5. Send two simulated deltas on the broadcast channel.
        let delta1 = pb::LocationUpdateMessage {
            is_snapshot: false,
            payload: b"delta-1".to_vec(),
            lsn: first.lsn + 1,
        };
        let delta2 = pb::LocationUpdateMessage {
            is_snapshot: false,
            payload: b"delta-2".to_vec(),
            lsn: first.lsn + 2,
        };
        bcast_tx.send(delta1.clone()).unwrap();
        bcast_tx.send(delta2.clone()).unwrap();

        // 6. Receive the deltas.
        let d1 = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for delta 1")
            .expect("stream ended before delta 1")
            .expect("delta 1 was an error");
        assert!(!d1.is_snapshot, "delta should have is_snapshot=false");
        assert_eq!(d1.payload, b"delta-1");
        assert_eq!(d1.lsn, first.lsn + 1);

        let d2 = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for delta 2")
            .expect("stream ended before delta 2")
            .expect("delta 2 was an error");
        assert!(!d2.is_snapshot, "delta should have is_snapshot=false");
        assert_eq!(d2.payload, b"delta-2");
        assert_eq!(d2.lsn, first.lsn + 2);

        // Clean up — dropping bcast_tx closes the broadcast channel which
        // will cause the spawned task to exit after the next recv.
        drop(bcast_tx);
    }

    #[tokio::test]
    async fn location_stream_snapshot_only_without_broadcast() {
        // When no broadcast sender exists (snapshot-only mode) the stream
        // should deliver the snapshot and then end.
        let (engine, _) = trondb_core::Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let handle = spawn_location_stream(engine.clone(), None, tx);

        let mut stream = tokio_stream::wrappers::ReceiverStream::new(rx);

        let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for snapshot")
            .expect("stream ended before snapshot")
            .expect("snapshot was an error");

        assert!(first.is_snapshot);

        // The task should finish shortly (no broadcast to listen on).
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("task should finish in snapshot-only mode")
            .expect("task panicked");
    }

    /// End-to-end gRPC test: connect to StreamLocationUpdates, verify
    /// snapshot + delta delivery over the wire.
    #[tokio::test]
    async fn location_stream_grpc_snapshot_then_deltas() {
        use crate::config::NodeRoleConfig;
        use crate::service::TronNodeService;
        use trondb_core::planner::{CreateCollectionPlan, Plan};

        // 1. Engine + data
        let (engine, _) = trondb_core::Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);

        let plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "grpc_loc_test".into(),
            representations: vec![],
            fields: vec![],
            indexes: vec![],
        });
        engine.execute(&plan).await.unwrap();

        // 2. Create broadcast channel and service
        let (bcast_tx, _) = broadcast::channel::<pb::LocationUpdateMessage>(64);
        let service = TronNodeService::new(engine.clone(), NodeRoleConfig::Primary)
            .with_location_broadcast(bcast_tx.clone());

        // 3. Start gRPC server
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

        // 4. Connect client
        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = pb::tron_node_client::TronNodeClient::new(channel);

        let response = client
            .stream_location_updates(tonic::Request::new(pb::Empty {}))
            .await
            .unwrap();
        let mut stream = response.into_inner();

        // 5. First message: snapshot
        let snapshot = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for snapshot")
            .expect("stream ended")
            .expect("gRPC error");
        assert!(snapshot.is_snapshot);
        assert!(!snapshot.payload.is_empty());

        // 6. Send a delta on the broadcast channel
        let delta = pb::LocationUpdateMessage {
            is_snapshot: false,
            payload: b"grpc-delta".to_vec(),
            lsn: snapshot.lsn + 1,
        };
        bcast_tx.send(delta).unwrap();

        let d = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for delta")
            .expect("stream ended")
            .expect("gRPC error");
        assert!(!d.is_snapshot);
        assert_eq!(d.payload, b"grpc-delta");

        server_task.abort();
    }
}
