use std::time::{Duration, Instant};
use dashmap::DashMap;
use crate::config::HealthConfig;
use crate::node::{NodeId, NodeRole};

// ---------------------------------------------------------------------------
// NodeStatus
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    Healthy,
    Degraded,
    Overloaded,
    Faulted,
}

// ---------------------------------------------------------------------------
// HealthSignal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HealthSignal {
    pub node_id:          NodeId,
    pub node_role:        NodeRole,
    pub signal_ts:        i64,
    pub sequence:         u64,
    pub cpu_utilisation:  f32,
    pub ram_pressure:     f32,
    pub hot_entity_count: u64,
    pub hot_tier_capacity: u64,
    pub queue_depth:      u32,
    pub queue_capacity:   u32,
    pub hnsw_p99_ms:      f32,
    pub hnsw_p50_ms:      f32,
    pub replica_lag_ms:   Option<u64>,
    pub load_score:       f32,
    pub status:           NodeStatus,
}

// ---------------------------------------------------------------------------
// Load score computation
// ---------------------------------------------------------------------------

pub fn compute_load_score(s: &HealthSignal, cfg: &HealthConfig) -> f32 {
    let cpu   = s.cpu_utilisation;
    let ram   = s.ram_pressure;
    let queue = if s.queue_capacity > 0 {
        s.queue_depth as f32 / s.queue_capacity as f32
    } else {
        0.0
    };
    let hnsw  = if cfg.hnsw_baseline_p99_ms > 0.0 {
        (s.hnsw_p99_ms / cfg.hnsw_baseline_p99_ms).min(1.0)
    } else {
        0.0
    };
    let lag = s.replica_lag_ms
        .map(|l| {
            if cfg.max_replica_lag_ms > 0.0 {
                (l as f32 / cfg.max_replica_lag_ms).min(1.0)
            } else {
                0.0
            }
        })
        .unwrap_or(0.0);

    (ram * 0.35) + (queue * 0.30) + (cpu * 0.20) + (hnsw * 0.10) + (lag * 0.05)
}

// ---------------------------------------------------------------------------
// HealthCache
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct CachedSignal {
    pub signal: HealthSignal,
    pub received_at: Instant,
}

pub struct HealthCache {
    signals: DashMap<NodeId, CachedSignal>,
    stale_after: Duration,
}

impl HealthCache {
    pub fn new(push_interval_ms: u64, stale_multiplier: u32) -> Self {
        Self {
            signals: DashMap::new(),
            stale_after: Duration::from_millis(
                push_interval_ms * stale_multiplier as u64,
            ),
        }
    }

    pub fn update(&self, signal: HealthSignal) {
        self.signals.insert(
            signal.node_id.clone(),
            CachedSignal {
                signal,
                received_at: Instant::now(),
            },
        );
    }

    pub fn get(&self, node: &NodeId) -> Option<HealthSignal> {
        self.signals.get(node).map(|entry| entry.signal.clone())
    }

    pub fn is_stale(&self, node: &NodeId) -> bool {
        match self.signals.get(node) {
            Some(entry) => entry.received_at.elapsed() > self.stale_after,
            None => true,
        }
    }

    /// Returns all non-Faulted, non-stale nodes sorted by load_score ascending.
    pub fn healthy_nodes(&self) -> Vec<(NodeId, f32)> {
        let mut nodes: Vec<(NodeId, f32)> = self
            .signals
            .iter()
            .filter(|entry| entry.signal.status != NodeStatus::Faulted)
            .filter(|entry| entry.received_at.elapsed() <= self.stale_after)
            .map(|entry| (entry.key().clone(), entry.signal.load_score))
            .collect();
        nodes.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        nodes
    }

    pub fn len(&self) -> usize {
        self.signals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.signals.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HealthConfig;
    use crate::node::NodeId;

    fn test_signal(cpu: f32, ram: f32, queue: u32, hnsw_p99: f32) -> HealthSignal {
        HealthSignal {
            node_id: NodeId::from_string("test"),
            node_role: NodeRole::HotNode,
            signal_ts: 0,
            sequence: 0,
            cpu_utilisation: cpu,
            ram_pressure: ram,
            hot_entity_count: 0,
            hot_tier_capacity: 100_000,
            queue_depth: queue,
            queue_capacity: 1000,
            hnsw_p99_ms: hnsw_p99,
            hnsw_p50_ms: 0.0,
            replica_lag_ms: None,
            load_score: 0.0,
            status: NodeStatus::Healthy,
        }
    }

    #[test]
    fn load_score_idle_node() {
        let s = test_signal(0.0, 0.0, 0, 0.0);
        let cfg = HealthConfig::default();
        let score = compute_load_score(&s, &cfg);
        assert!((score - 0.0).abs() < 1e-6, "idle node should score 0.0, got {score}");
    }

    #[test]
    fn load_score_fully_loaded() {
        let s = test_signal(1.0, 1.0, 1000, 10.0);
        let cfg = HealthConfig::default();
        let score = compute_load_score(&s, &cfg);
        // ram*0.35 + queue*0.30 + cpu*0.20 + hnsw*0.10 + lag*0.05
        // = 0.35 + 0.30 + 0.20 + 0.10 + 0.0 = 0.95
        assert!((score - 0.95).abs() < 1e-6, "full load should score 0.95, got {score}");
    }

    #[test]
    fn load_score_with_replica_lag() {
        let mut s = test_signal(0.0, 0.0, 0, 0.0);
        s.replica_lag_ms = Some(500);
        let cfg = HealthConfig::default();
        let score = compute_load_score(&s, &cfg);
        // only lag contributes: 0.5 * 0.05 = 0.025
        assert!((score - 0.025).abs() < 1e-6, "got {score}");
    }

    #[test]
    fn load_score_hnsw_capped_at_one() {
        let s = test_signal(0.0, 0.0, 0, 100.0); // 10x baseline
        let cfg = HealthConfig::default();
        let score = compute_load_score(&s, &cfg);
        // hnsw normalised to 1.0 (capped), contributes 0.10
        assert!((score - 0.10).abs() < 1e-6, "got {score}");
    }

    #[test]
    fn health_cache_update_and_get() {
        let cache = HealthCache::new(200, 3);
        let mut s = test_signal(0.5, 0.3, 100, 5.0);
        s.load_score = 0.42;
        cache.update(s);
        let got = cache.get(&NodeId::from_string("test")).unwrap();
        assert!((got.load_score - 0.42).abs() < 1e-6);
    }

    #[test]
    fn health_cache_healthy_nodes_excludes_faulted() {
        let cache = HealthCache::new(200, 3);
        let mut s1 = test_signal(0.1, 0.1, 10, 1.0);
        s1.node_id = NodeId::from_string("node-1");
        s1.load_score = 0.1;
        cache.update(s1);

        let mut s2 = test_signal(0.9, 0.9, 900, 9.0);
        s2.node_id = NodeId::from_string("node-2");
        s2.status = NodeStatus::Faulted;
        s2.load_score = 0.9;
        cache.update(s2);

        let healthy = cache.healthy_nodes();
        assert_eq!(healthy.len(), 1);
        assert_eq!(healthy[0].0, NodeId::from_string("node-1"));
    }

    #[test]
    fn health_cache_sorted_by_load_score() {
        let cache = HealthCache::new(200, 3);
        for (name, score) in [("c", 0.7), ("a", 0.2), ("b", 0.5)] {
            let mut s = test_signal(0.0, 0.0, 0, 0.0);
            s.node_id = NodeId::from_string(name);
            s.load_score = score;
            cache.update(s);
        }
        let healthy = cache.healthy_nodes();
        assert_eq!(healthy[0].0, NodeId::from_string("a"));
        assert_eq!(healthy[1].0, NodeId::from_string("b"));
        assert_eq!(healthy[2].0, NodeId::from_string("c"));
    }

    #[test]
    fn health_cache_unknown_node_is_stale() {
        let cache = HealthCache::new(200, 3);
        assert!(cache.is_stale(&NodeId::from_string("unknown")));
    }
}
