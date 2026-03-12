use std::sync::Arc;
use std::time::Duration;
use dashmap::DashMap;
use tokio::sync::{mpsc, Notify};
use trondb_proto::pb;

#[derive(Debug)]
pub enum ReplicationError {
    Timeout,
    InsufficientReplicas,
}

impl std::fmt::Display for ReplicationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplicationError::Timeout => write!(f, "replication ack timed out"),
            ReplicationError::InsufficientReplicas => write!(f, "insufficient replicas"),
        }
    }
}

impl std::error::Error for ReplicationError {}

struct ReplicaState {
    confirmed_lsn: u64,
    #[allow(dead_code)]
    connected_at: std::time::Instant,
    sender: mpsc::Sender<pb::WalRecordMessage>,
}

#[derive(Clone)]
pub struct ReplicaTracker {
    replicas: Arc<DashMap<String, ReplicaState>>,
    ack_notify: Arc<Notify>,
}

impl ReplicaTracker {
    pub fn new() -> Self {
        Self {
            replicas: Arc::new(DashMap::new()),
            ack_notify: Arc::new(Notify::new()),
        }
    }

    pub fn register(&self, replica_id: String, sender: mpsc::Sender<pb::WalRecordMessage>) {
        self.replicas.insert(
            replica_id,
            ReplicaState {
                confirmed_lsn: 0,
                connected_at: std::time::Instant::now(),
                sender,
            },
        );
    }

    pub fn disconnect(&self, replica_id: &str) {
        self.replicas.remove(replica_id);
    }

    pub fn update_confirmed_lsn(&self, replica_id: &str, lsn: u64) {
        if let Some(mut state) = self.replicas.get_mut(replica_id) {
            state.confirmed_lsn = lsn;
        }
        self.ack_notify.notify_waiters();
    }

    pub fn confirmed_lsn(&self, replica_id: &str) -> Option<u64> {
        self.replicas.get(replica_id).map(|s| s.confirmed_lsn)
    }

    pub fn connected_count(&self) -> usize {
        self.replicas.len()
    }

    /// Wait until at least `min_replicas` have confirmed `lsn`, or timeout.
    pub async fn wait_for_ack(
        &self,
        lsn: u64,
        min_replicas: u32,
        timeout: Duration,
    ) -> Result<(), ReplicationError> {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            // Count how many replicas have confirmed at least `lsn`.
            let confirmed = self
                .replicas
                .iter()
                .filter(|entry| entry.value().confirmed_lsn >= lsn)
                .count();

            if confirmed >= min_replicas as usize {
                return Ok(());
            }

            // Check if we've already exceeded the deadline.
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(ReplicationError::Timeout);
            }

            // Wait for a notification or until the deadline.
            let remaining = deadline - now;
            let notified = self.ack_notify.notified();
            tokio::pin!(notified);

            if tokio::time::timeout(remaining, notified).await.is_err() {
                return Err(ReplicationError::Timeout);
            }
        }
    }

    /// Broadcast a WAL record to all connected replicas (best-effort, skip disconnected).
    pub async fn broadcast(&self, record: &pb::WalRecordMessage) {
        // Collect replica IDs to send to (avoid holding dashmap refs across await).
        let senders: Vec<mpsc::Sender<pb::WalRecordMessage>> = self
            .replicas
            .iter()
            .map(|entry| entry.value().sender.clone())
            .collect();

        for sender in senders {
            // Clone the record for each sender; ignore errors (replica may have disconnected).
            let _ = sender.try_send(record.clone());
        }
    }
}

impl Default for ReplicaTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tracker_registers_and_tracks_lsn() {
        let tracker = ReplicaTracker::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        tracker.register("replica-1".into(), tx);
        tracker.update_confirmed_lsn("replica-1", 42);
        assert_eq!(tracker.confirmed_lsn("replica-1"), Some(42));
    }

    #[tokio::test]
    async fn wait_for_ack_succeeds_when_replica_confirms() {
        let tracker = ReplicaTracker::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        tracker.register("replica-1".into(), tx);

        // Spawn task that confirms after a short delay
        let tracker_clone = tracker.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            tracker_clone.update_confirmed_lsn("replica-1", 100);
        });

        let result = tracker.wait_for_ack(100, 1, Duration::from_secs(1)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wait_for_ack_times_out() {
        let tracker = ReplicaTracker::new();
        let result = tracker.wait_for_ack(100, 1, Duration::from_millis(50)).await;
        assert!(result.is_err());
    }

    #[test]
    fn disconnect_removes_replica() {
        let tracker = ReplicaTracker::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        tracker.register("replica-1".into(), tx);
        assert_eq!(tracker.connected_count(), 1);
        tracker.disconnect("replica-1");
        assert_eq!(tracker.connected_count(), 0);
    }
}
