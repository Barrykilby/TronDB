# Phase 6: Routing Intelligence Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build TronDB's routing intelligence layer — health signals, co-location, and semantic routing — behind a `NodeHandle` trait abstraction for testability.

**Architecture:** New `trondb-routing` crate sits above `trondb-core`. Three tightly coupled subsystems (health, affinity, router) share identity types and config. All logic is real; only the transport is abstracted via `NodeHandle` trait. Single-node mode uses `LocalNode` adapter wrapping in-process `Engine`.

**Tech Stack:** Rust 2021, Tokio (async, `watch` channels, `spawn`), DashMap 6, async-trait, thiserror 2, serde/rmp-serde (MessagePack), Fjall (persistence for explicit affinity groups)

**Spec:** `docs/superpowers/specs/2026-03-11-trondb-phase6-design.md`

---

## File Structure

### New files (trondb-routing crate)
| File | Responsibility |
|------|---------------|
| `crates/trondb-routing/Cargo.toml` | Crate manifest, workspace member |
| `crates/trondb-routing/src/lib.rs` | Public API, re-exports, integration tests |
| `crates/trondb-routing/src/node.rs` | `NodeHandle` trait, `NodeId`, `NodeRole`, `LocalNode`, `SimulatedNode` |
| `crates/trondb-routing/src/health.rs` | `HealthSignal`, `HealthCache`, `compute_load_score()`, `NodeStatus` |
| `crates/trondb-routing/src/affinity.rs` | `AffinityIndex`, `AffinityGroup`, co-occurrence tracking, group CRUD |
| `crates/trondb-routing/src/router.rs` | `SemanticRouter`, candidate scoring, dispatch, scatter-gather |
| `crates/trondb-routing/src/eviction.rs` | Soft eviction policy, LRU tracking, priority rules |
| `crates/trondb-routing/src/config.rs` | `RouterConfig`, `HealthConfig`, `ColocationConfig` |
| `crates/trondb-routing/src/error.rs` | `RouterError` enum |

### Modified files
| File | Changes |
|------|---------|
| `Cargo.toml` (workspace) | Add `trondb-routing` member, add `async-trait` to workspace deps |
| `crates/trondb-wal/src/record.rs` | Add 3 WAL record types: `AffinityGroupCreate`, `AffinityGroupMember`, `AffinityGroupRemove` |
| `crates/trondb-tql/src/token.rs` | Add 7 tokens: `Collocate`, `Affinity`, `Group`, `Alter`, `Drop`, `Entity` (keyword), `CollocateWith` |
| `crates/trondb-tql/src/ast.rs` | Extend `InsertStmt`, add `CreateAffinityGroupStmt`, `AlterEntityDropAffinityStmt`, new `Statement` variants |
| `crates/trondb-tql/src/parser.rs` | Parse `COLLOCATE_WITH`, `AFFINITY GROUP`, `CREATE AFFINITY GROUP`, `ALTER ENTITY ... DROP AFFINITY GROUP` |
| `crates/trondb-core/src/planner.rs` | Add `Plan::CreateAffinityGroup`, `Plan::AlterEntityDropAffinity`, extend `InsertPlan` with colocation fields |
| `crates/trondb-core/src/executor.rs` | Handle new plan variants (delegate to routing layer), add `entity_count()`, `collection_count()` |
| `crates/trondb-core/src/lib.rs` | Expose `entity_count()`, `collection_count()` on `Engine`, add integration tests |
| `crates/trondb-cli/Cargo.toml` | Add `trondb-routing` dependency |
| `crates/trondb-cli/src/main.rs` | Wire `SemanticRouter` with `LocalNode`, route queries through router |

---

## Chunk 1: Foundation — Crate scaffold, identity types, config, error

### Task 1: Crate scaffold + identity types + config + error

Create the `trondb-routing` crate with all foundational types that other modules depend on.

**Files:**
- Create: `crates/trondb-routing/Cargo.toml`
- Create: `crates/trondb-routing/src/lib.rs`
- Create: `crates/trondb-routing/src/node.rs`
- Create: `crates/trondb-routing/src/config.rs`
- Create: `crates/trondb-routing/src/error.rs`
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Add trondb-routing to workspace and add async-trait dep**

In `Cargo.toml` (workspace root), add `"crates/trondb-routing"` to `members` array. Add `async-trait = "0.1"` to `[workspace.dependencies]`.

- [ ] **Step 2: Create `crates/trondb-routing/Cargo.toml`**

```toml
[package]
name = "trondb-routing"
version = "0.1.0"
edition = "2021"

[dependencies]
trondb-core = { path = "../trondb-core" }
trondb-tql = { path = "../trondb-tql" }
async-trait.workspace = true
tokio.workspace = true
dashmap.workspace = true
serde.workspace = true
thiserror.workspace = true

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 3: Create `crates/trondb-routing/src/error.rs`**

```rust
use trondb_core::error::EngineError;

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("no healthy nodes available for routing")]
    NoCandidates,

    #[error("all nodes overloaded, cluster at capacity")]
    ClusterOverloaded,

    #[error("node overloaded, retry after {retry_after_ms}ms")]
    Backpressure { retry_after_ms: u64 },

    #[error("affinity group '{0}' is full (max {1} entities)")]
    AffinityGroupFull(String, usize),

    #[error("affinity group '{0}' not found")]
    AffinityGroupNotFound(String),

    #[error("affinity group '{0}' already exists")]
    AffinityGroupAlreadyExists(String),

    #[error(transparent)]
    Engine(#[from] EngineError),
}
```

- [ ] **Step 4: Create `crates/trondb-routing/src/config.rs`**

```rust
#[derive(Debug, Clone)]
pub struct RouterConfig {
    pub health: HealthConfig,
    pub colocation: ColocationConfig,
    pub weight_health: f32,
    pub weight_verb_fit: f32,
    pub weight_affinity: f32,
    pub affinity_boost_max: f32,
    pub retry_after_ms: u64,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            health: HealthConfig::default(),
            colocation: ColocationConfig::default(),
            weight_health: 0.40,
            weight_verb_fit: 0.30,
            weight_affinity: 0.30,
            affinity_boost_max: 0.05,
            retry_after_ms: 500,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HealthConfig {
    pub push_interval_ms: u64,
    pub stale_multiplier: u32,
    pub pull_threshold_acu: f32,
    pub cpu_warn_threshold: f32,
    pub ram_warn_threshold: f32,
    pub queue_depth_alert: u32,
    pub hnsw_baseline_p99_ms: f32,
    pub max_replica_lag_ms: f32,
    pub load_shed_threshold: f32,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            push_interval_ms: 200,
            stale_multiplier: 3,
            pull_threshold_acu: 80.0,
            cpu_warn_threshold: 0.75,
            ram_warn_threshold: 0.85,
            queue_depth_alert: 500,
            hnsw_baseline_p99_ms: 10.0,
            max_replica_lag_ms: 1000.0,
            load_shed_threshold: 0.85,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ColocationConfig {
    pub learn_threshold: f32,
    pub learn_interval_ms: u64,
    pub decay_factor: f32,
    pub max_group_size: usize,
    pub ram_headroom: f32,
    pub soft_evict_delay_ms: u64,
}

impl Default for ColocationConfig {
    fn default() -> Self {
        Self {
            learn_threshold: 0.70,
            learn_interval_ms: 30_000,
            decay_factor: 0.95,
            max_group_size: 500,
            ram_headroom: 0.80,
            soft_evict_delay_ms: 5_000,
        }
    }
}
```

- [ ] **Step 5: Create `crates/trondb-routing/src/node.rs`**

```rust
use std::fmt;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// NodeId
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(String);

impl NodeId {
    pub fn from_string(s: &str) -> Self {
        Self(s.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// AffinityGroupId
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AffinityGroupId(String);

impl AffinityGroupId {
    pub fn from_string(s: &str) -> Self {
        Self(s.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AffinityGroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// NodeRole
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRole {
    HotNode,
    WarmNode,
    ReadReplica,
}

// ---------------------------------------------------------------------------
// QueryVerb / RoutingStrategy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryVerb {
    Fetch,
    Search,
    Traverse,
    Infer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingStrategy {
    LocationAware,
    ScatterGather,
}

// ---------------------------------------------------------------------------
// EntityId alias
// ---------------------------------------------------------------------------

pub type EntityId = trondb_core::types::LogicalId;
```

- [ ] **Step 6: Create `crates/trondb-routing/src/lib.rs`**

```rust
pub mod config;
pub mod error;
pub mod node;

pub use config::{ColocationConfig, HealthConfig, RouterConfig};
pub use error::RouterError;
pub use node::{AffinityGroupId, EntityId, NodeId, NodeRole, QueryVerb, RoutingStrategy};
```

- [ ] **Step 7: Verify it compiles**

Run: `cargo check -p trondb-routing`
Expected: compiles with no errors

- [ ] **Step 8: Write tests for identity types**

Add to `crates/trondb-routing/src/node.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_equality() {
        let a = NodeId::from_string("node-1");
        let b = NodeId::from_string("node-1");
        let c = NodeId::from_string("node-2");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn affinity_group_id_display() {
        let id = AffinityGroupId::from_string("cluster-1");
        assert_eq!(id.to_string(), "cluster-1");
        assert_eq!(id.as_str(), "cluster-1");
    }
}
```

Add to `crates/trondb-routing/src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_weights_sum_to_one() {
        let cfg = RouterConfig::default();
        let sum = cfg.weight_health + cfg.weight_verb_fit + cfg.weight_affinity;
        assert!((sum - 1.0).abs() < 1e-6);
    }

    #[test]
    fn default_health_config_thresholds() {
        let cfg = HealthConfig::default();
        assert_eq!(cfg.push_interval_ms, 200);
        assert!((cfg.load_shed_threshold - 0.85).abs() < 1e-6);
    }
}
```

- [ ] **Step 9: Run tests**

Run: `cargo test -p trondb-routing`
Expected: all tests pass

- [ ] **Step 10: Commit**

```bash
git add crates/trondb-routing/ Cargo.toml
git commit -m "feat: add trondb-routing crate scaffold — identity types, config, error"
```

---

### Task 2: HealthSignal + HealthCache + load score computation

**Files:**
- Create: `crates/trondb-routing/src/health.rs`
- Modify: `crates/trondb-routing/src/lib.rs`

- [ ] **Step 1: Write failing tests for `compute_load_score`**

Create `crates/trondb-routing/src/health.rs` with tests first:

```rust
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
}
```

- [ ] **Step 2: Implement HealthSignal, NodeStatus, compute_load_score**

Add above the tests in `health.rs`:

```rust
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

    /// Returns all non-Faulted nodes sorted by load_score ascending.
    pub fn healthy_nodes(&self) -> Vec<(NodeId, f32)> {
        let mut nodes: Vec<(NodeId, f32)> = self
            .signals
            .iter()
            .filter(|entry| entry.signal.status != NodeStatus::Faulted)
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
```

- [ ] **Step 3: Add HealthCache tests**

Append to the `mod tests` block:

```rust
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
```

- [ ] **Step 4: Update lib.rs to export health module**

Add `pub mod health;` to `crates/trondb-routing/src/lib.rs` and re-export key types:

```rust
pub use health::{compute_load_score, HealthCache, HealthSignal, NodeStatus};
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p trondb-routing`
Expected: all tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-routing/
git commit -m "feat: add HealthSignal, HealthCache, load score computation"
```

---

### Task 3: NodeHandle trait + LocalNode + SimulatedNode

**Files:**
- Modify: `crates/trondb-routing/src/node.rs`
- Modify: `crates/trondb-core/src/lib.rs` (add `entity_count`, `collection_count`)
- Modify: `crates/trondb-core/src/executor.rs` (add accessor methods)

- [ ] **Step 1: Add `entity_count` and `collection_count` to Engine**

In `crates/trondb-core/src/executor.rs`, add these methods to `impl Executor`:

```rust
pub fn entity_count(&self) -> usize {
    self.store.entity_count()
}

pub fn collection_count(&self) -> usize {
    self.schemas.len()
}
```

In `crates/trondb-core/src/store.rs`, add to `impl FjallStore`:

```rust
pub fn entity_count(&self) -> usize {
    // Count entities across all collection partitions
    let mut count = 0;
    for entry in &self.partitions {
        count += entry.value().len();
    }
    count
}
```

Note: `FjallStore.partitions` is a `DashMap<String, Partition>` — `Partition::len()` returns the key count.

In `crates/trondb-core/src/lib.rs`, add to `impl Engine`:

```rust
pub fn entity_count(&self) -> usize {
    self.executor.entity_count()
}

pub fn collection_count(&self) -> usize {
    self.executor.collection_count()
}

/// Execute a pre-planned query. Used by the routing layer.
pub async fn execute(&self, plan: &planner::Plan) -> Result<QueryResult, EngineError> {
    self.executor.execute(plan).await
}

/// Parse TQL and produce a Plan without executing.
pub fn parse_and_plan(&self, tql: &str) -> Result<planner::Plan, EngineError> {
    let stmt = trondb_tql::parse(tql)
        .map_err(|e| EngineError::InvalidQuery(e.to_string()))?;
    planner::plan(&stmt, self.executor.schemas())
}
```

- [ ] **Step 2: Add NodeHandle trait and adapters to node.rs**

Add to `crates/trondb-routing/src/node.rs`:

```rust
use std::sync::{Arc, Mutex};
use async_trait::async_trait;
use trondb_core::error::EngineError;
use trondb_core::result::QueryResult;
use trondb_core::Engine;
use crate::health::{HealthSignal, NodeStatus};

// ---------------------------------------------------------------------------
// NodeHandle trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait NodeHandle: Send + Sync {
    fn node_id(&self) -> &NodeId;
    fn node_role(&self) -> NodeRole;
    async fn execute(
        &self,
        plan: &trondb_core::planner::Plan,
    ) -> Result<QueryResult, EngineError>;
    async fn health_snapshot(&self) -> HealthSignal;
}

// ---------------------------------------------------------------------------
// LocalNode — wraps in-process Engine
// ---------------------------------------------------------------------------

const HOT_TIER_DEFAULT_CAPACITY: u64 = 100_000;

pub struct LocalNode {
    engine: Arc<Engine>,
    node_id: NodeId,
}

impl LocalNode {
    pub fn new(engine: Arc<Engine>, node_id: NodeId) -> Self {
        Self { engine, node_id }
    }
}

#[async_trait]
impl NodeHandle for LocalNode {
    fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    fn node_role(&self) -> NodeRole {
        NodeRole::HotNode
    }

    async fn execute(
        &self,
        plan: &trondb_core::planner::Plan,
    ) -> Result<QueryResult, EngineError> {
        self.engine.execute(plan).await
    }

    async fn health_snapshot(&self) -> HealthSignal {
        let entity_count = self.engine.entity_count() as u64;
        HealthSignal {
            node_id: self.node_id.clone(),
            node_role: NodeRole::HotNode,
            signal_ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64,
            sequence: 0,
            cpu_utilisation: 0.0,
            ram_pressure: entity_count as f32 / HOT_TIER_DEFAULT_CAPACITY as f32,
            hot_entity_count: entity_count,
            hot_tier_capacity: HOT_TIER_DEFAULT_CAPACITY,
            queue_depth: 0,
            queue_capacity: 1000,
            hnsw_p99_ms: 0.0,
            hnsw_p50_ms: 0.0,
            replica_lag_ms: None,
            load_score: 0.0,
            status: NodeStatus::Healthy,
        }
    }
}

// ---------------------------------------------------------------------------
// SimulatedNode — for tests, controllable health metrics
// ---------------------------------------------------------------------------

pub struct SimulatedNode {
    engine: Arc<Engine>,
    node_id: NodeId,
    health: Arc<Mutex<HealthSignal>>,
}

impl SimulatedNode {
    pub fn new(engine: Arc<Engine>, node_id: NodeId) -> Self {
        let health = HealthSignal {
            node_id: node_id.clone(),
            node_role: NodeRole::HotNode,
            signal_ts: 0,
            sequence: 0,
            cpu_utilisation: 0.0,
            ram_pressure: 0.0,
            hot_entity_count: 0,
            hot_tier_capacity: HOT_TIER_DEFAULT_CAPACITY,
            queue_depth: 0,
            queue_capacity: 1000,
            hnsw_p99_ms: 0.0,
            hnsw_p50_ms: 0.0,
            replica_lag_ms: None,
            load_score: 0.0,
            status: NodeStatus::Healthy,
        };
        Self {
            engine,
            node_id,
            health: Arc::new(Mutex::new(health)),
        }
    }

    pub fn set_cpu(&self, v: f32) {
        self.health.lock().unwrap().cpu_utilisation = v;
    }
    pub fn set_ram_pressure(&self, v: f32) {
        self.health.lock().unwrap().ram_pressure = v;
    }
    pub fn set_queue_depth(&self, v: u32) {
        self.health.lock().unwrap().queue_depth = v;
    }
    pub fn set_load_score(&self, v: f32) {
        self.health.lock().unwrap().load_score = v;
    }
    pub fn set_status(&self, s: NodeStatus) {
        self.health.lock().unwrap().status = s;
    }
}

#[async_trait]
impl NodeHandle for SimulatedNode {
    fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    fn node_role(&self) -> NodeRole {
        NodeRole::HotNode
    }

    async fn execute(
        &self,
        plan: &trondb_core::planner::Plan,
    ) -> Result<QueryResult, EngineError> {
        self.engine.execute(plan).await
    }

    async fn health_snapshot(&self) -> HealthSignal {
        self.health.lock().unwrap().clone()
    }
}
```

- [ ] **Step 3: Update lib.rs exports**

```rust
pub use node::{
    AffinityGroupId, EntityId, LocalNode, NodeHandle, NodeId, NodeRole,
    QueryVerb, RoutingStrategy, SimulatedNode,
};
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-routing`
Expected: compiles and all tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-routing/ crates/trondb-core/
git commit -m "feat: add NodeHandle trait, LocalNode, SimulatedNode adapters"
```

---

### Task 4: AffinityIndex — explicit groups, co-occurrence, canonical ordering

**Files:**
- Create: `crates/trondb-routing/src/affinity.rs`
- Modify: `crates/trondb-routing/src/lib.rs`

- [ ] **Step 1: Create affinity.rs with types and CRUD methods**

```rust
use std::collections::HashSet;
use dashmap::DashMap;
use crate::config::ColocationConfig;
use crate::error::RouterError;
use crate::node::{AffinityGroupId, EntityId, NodeId};

// ---------------------------------------------------------------------------
// AffinitySource
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AffinitySource {
    Explicit,
    Learned,
}

// ---------------------------------------------------------------------------
// AffinityGroup
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AffinityGroup {
    pub id: AffinityGroupId,
    pub members: HashSet<EntityId>,
    pub source: AffinitySource,
    pub target_node: Option<NodeId>,
    pub created_at: i64,
    pub last_seen: i64,
}

// ---------------------------------------------------------------------------
// AffinityIndex
// ---------------------------------------------------------------------------

pub struct AffinityIndex {
    explicit: DashMap<EntityId, AffinityGroupId>,
    implicit: DashMap<(EntityId, EntityId), f32>,
    groups: DashMap<AffinityGroupId, AffinityGroup>,
}

impl AffinityIndex {
    pub fn new() -> Self {
        Self {
            explicit: DashMap::new(),
            implicit: DashMap::new(),
            groups: DashMap::new(),
        }
    }

    // --- Explicit group CRUD ---

    pub fn create_group(&self, id: AffinityGroupId) -> Result<(), RouterError> {
        if self.groups.contains_key(&id) {
            return Err(RouterError::AffinityGroupAlreadyExists(id.as_str().to_owned()));
        }
        let now = now_ms();
        self.groups.insert(id.clone(), AffinityGroup {
            id,
            members: HashSet::new(),
            source: AffinitySource::Explicit,
            target_node: None,
            created_at: now,
            last_seen: now,
        });
        Ok(())
    }

    pub fn add_to_group(
        &self,
        entity: &EntityId,
        group_id: &AffinityGroupId,
        max_size: usize,
    ) -> Result<(), RouterError> {
        let mut group = self.groups.get_mut(group_id)
            .ok_or_else(|| RouterError::AffinityGroupNotFound(group_id.as_str().to_owned()))?;
        if group.members.len() >= max_size {
            return Err(RouterError::AffinityGroupFull(
                group_id.as_str().to_owned(),
                max_size,
            ));
        }
        group.members.insert(entity.clone());
        group.last_seen = now_ms();
        self.explicit.insert(entity.clone(), group_id.clone());
        Ok(())
    }

    pub fn remove_from_group(&self, entity: &EntityId) {
        if let Some((_, group_id)) = self.explicit.remove(entity) {
            if let Some(mut group) = self.groups.get_mut(&group_id) {
                group.members.remove(entity);
            }
        }
    }

    pub fn group_for(&self, entity: &EntityId) -> Option<AffinityGroupId> {
        self.explicit.get(entity).map(|r| r.clone())
    }

    pub fn get_group(&self, id: &AffinityGroupId) -> Option<AffinityGroup> {
        self.groups.get(id).map(|r| r.clone())
    }

    /// Returns the target node for the entity's group (if any).
    pub fn preferred_node(&self, entity: &EntityId) -> Option<NodeId> {
        let group_id = self.explicit.get(entity)?;
        let group = self.groups.get(&*group_id)?;
        group.target_node.clone()
    }

    // --- Co-occurrence tracking ---

    pub fn record_cooccurrence(&self, results: &[EntityId]) {
        if results.len() < 2 {
            return;
        }
        let increment = 1.0 / (results.len() - 1) as f32;
        for i in 0..results.len() {
            for j in (i + 1)..results.len() {
                let key = canonical_pair(&results[i], &results[j]);
                self.implicit
                    .entry(key)
                    .and_modify(|s| *s += increment)
                    .or_insert(increment);
            }
        }
    }

    pub fn cooccurrence_score(&self, a: &EntityId, b: &EntityId) -> f32 {
        let key = canonical_pair(a, b);
        self.implicit.get(&key).map(|r| *r).unwrap_or(0.0)
    }

    /// Run implicit affinity promotion: promote high-scoring pairs to groups,
    /// decay all scores, prune entries below 0.01.
    pub fn promote_and_decay(&self, cfg: &ColocationConfig) {
        let mut to_promote = Vec::new();
        for entry in self.implicit.iter() {
            if *entry.value() >= cfg.learn_threshold {
                to_promote.push(entry.key().clone());
            }
        }
        for (a, b) in to_promote {
            self.create_implicit_group(&a, &b, cfg.max_group_size);
        }
        // Decay all scores
        for mut entry in self.implicit.iter_mut() {
            *entry.value_mut() *= cfg.decay_factor;
        }
        // Prune near-zero
        self.implicit.retain(|_, score| *score >= 0.01);
    }

    fn create_implicit_group(
        &self,
        a: &EntityId,
        b: &EntityId,
        max_size: usize,
    ) {
        // Check if either already belongs to a group
        let a_group = self.explicit.get(a).map(|r| r.clone());
        let b_group = self.explicit.get(b).map(|r| r.clone());

        match (a_group, b_group) {
            (Some(_), Some(_)) => {
                // Both already in groups — skip
            }
            (Some(gid), None) => {
                // Add b to a's group (if not full)
                if let Some(mut g) = self.groups.get_mut(&gid) {
                    if g.members.len() < max_size {
                        g.members.insert(b.clone());
                        self.explicit.insert(b.clone(), gid);
                    }
                }
            }
            (None, Some(gid)) => {
                // Add a to b's group
                if let Some(mut g) = self.groups.get_mut(&gid) {
                    if g.members.len() < max_size {
                        g.members.insert(a.clone());
                        self.explicit.insert(a.clone(), gid);
                    }
                }
            }
            (None, None) => {
                // Create new learned group
                let gid = AffinityGroupId::from_string(
                    &format!("learned_{}_{}", a.as_str(), b.as_str()),
                );
                let now = now_ms();
                let mut members = HashSet::new();
                members.insert(a.clone());
                members.insert(b.clone());
                self.groups.insert(gid.clone(), AffinityGroup {
                    id: gid.clone(),
                    members,
                    source: AffinitySource::Learned,
                    target_node: None,
                    created_at: now,
                    last_seen: now,
                });
                self.explicit.insert(a.clone(), gid.clone());
                self.explicit.insert(b.clone(), gid);
            }
        }
    }

    pub fn implicit_count(&self) -> usize {
        self.implicit.len()
    }

    pub fn group_count(&self) -> usize {
        self.groups.len()
    }
}

impl Default for AffinityIndex {
    fn default() -> Self {
        Self::new()
    }
}

fn canonical_pair(a: &EntityId, b: &EntityId) -> (EntityId, EntityId) {
    if a.as_str() <= b.as_str() {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
```

- [ ] **Step 2: Write tests**

Add to `affinity.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use trondb_core::types::LogicalId;

    fn eid(s: &str) -> EntityId {
        LogicalId::from_string(s)
    }

    #[test]
    fn create_and_get_group() {
        let idx = AffinityIndex::new();
        let gid = AffinityGroupId::from_string("g1");
        idx.create_group(gid.clone()).unwrap();
        let group = idx.get_group(&gid).unwrap();
        assert_eq!(group.source, AffinitySource::Explicit);
        assert!(group.members.is_empty());
    }

    #[test]
    fn duplicate_group_errors() {
        let idx = AffinityIndex::new();
        let gid = AffinityGroupId::from_string("g1");
        idx.create_group(gid.clone()).unwrap();
        assert!(idx.create_group(gid).is_err());
    }

    #[test]
    fn add_and_remove_member() {
        let idx = AffinityIndex::new();
        let gid = AffinityGroupId::from_string("g1");
        idx.create_group(gid.clone()).unwrap();
        idx.add_to_group(&eid("e1"), &gid, 500).unwrap();

        assert_eq!(idx.group_for(&eid("e1")), Some(gid.clone()));
        let group = idx.get_group(&gid).unwrap();
        assert!(group.members.contains(&eid("e1")));

        idx.remove_from_group(&eid("e1"));
        assert_eq!(idx.group_for(&eid("e1")), None);
    }

    #[test]
    fn group_full_error() {
        let idx = AffinityIndex::new();
        let gid = AffinityGroupId::from_string("g1");
        idx.create_group(gid.clone()).unwrap();
        idx.add_to_group(&eid("e1"), &gid, 2).unwrap();
        idx.add_to_group(&eid("e2"), &gid, 2).unwrap();
        let err = idx.add_to_group(&eid("e3"), &gid, 2);
        assert!(matches!(err, Err(RouterError::AffinityGroupFull(_, 2))));
    }

    #[test]
    fn canonical_pair_ordering() {
        let (a, b) = canonical_pair(&eid("b"), &eid("a"));
        assert_eq!(a, eid("a"));
        assert_eq!(b, eid("b"));
    }

    #[test]
    fn cooccurrence_symmetric() {
        let idx = AffinityIndex::new();
        idx.record_cooccurrence(&[eid("a"), eid("b"), eid("c")]);
        // (a,b), (a,c), (b,c) — each gets 1.0/(3-1) = 0.5
        assert!((idx.cooccurrence_score(&eid("a"), &eid("b")) - 0.5).abs() < 1e-6);
        assert!((idx.cooccurrence_score(&eid("b"), &eid("a")) - 0.5).abs() < 1e-6);
        assert!((idx.cooccurrence_score(&eid("a"), &eid("c")) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn cooccurrence_accumulates() {
        let idx = AffinityIndex::new();
        idx.record_cooccurrence(&[eid("x"), eid("y")]);
        idx.record_cooccurrence(&[eid("x"), eid("y")]);
        // Each call: 1.0/(2-1) = 1.0; total = 2.0
        assert!((idx.cooccurrence_score(&eid("x"), &eid("y")) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn promote_and_decay_creates_learned_group() {
        let idx = AffinityIndex::new();
        let cfg = ColocationConfig {
            learn_threshold: 0.70,
            decay_factor: 0.95,
            max_group_size: 500,
            ..ColocationConfig::default()
        };
        // Push score above threshold
        for _ in 0..5 {
            idx.record_cooccurrence(&[eid("p"), eid("q")]);
        }
        // Score should be 5.0, well above 0.70
        idx.promote_and_decay(&cfg);
        // Both should now be in a learned group
        assert!(idx.group_for(&eid("p")).is_some());
        assert!(idx.group_for(&eid("q")).is_some());
        assert_eq!(idx.group_for(&eid("p")), idx.group_for(&eid("q")));
    }

    #[test]
    fn decay_prunes_low_scores() {
        let idx = AffinityIndex::new();
        let cfg = ColocationConfig {
            learn_threshold: 10.0, // unreachable
            decay_factor: 0.001,   // aggressive decay
            ..ColocationConfig::default()
        };
        idx.record_cooccurrence(&[eid("a"), eid("b")]);
        assert_eq!(idx.implicit_count(), 1);
        idx.promote_and_decay(&cfg);
        // After aggressive decay, score < 0.01, should be pruned
        assert_eq!(idx.implicit_count(), 0);
    }

    #[test]
    fn preferred_node_returns_group_target() {
        let idx = AffinityIndex::new();
        let gid = AffinityGroupId::from_string("g1");
        idx.create_group(gid.clone()).unwrap();
        idx.add_to_group(&eid("e1"), &gid, 500).unwrap();
        // No target node set
        assert_eq!(idx.preferred_node(&eid("e1")), None);
        // Set target node
        idx.groups.get_mut(&gid).unwrap().target_node =
            Some(crate::node::NodeId::from_string("node-1"));
        assert_eq!(
            idx.preferred_node(&eid("e1")),
            Some(crate::node::NodeId::from_string("node-1"))
        );
    }
}
```

- [ ] **Step 3: Update lib.rs**

Add `pub mod affinity;` and re-export:

```rust
pub use affinity::{AffinityGroup, AffinityIndex, AffinitySource};
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-routing`
Expected: all tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-routing/
git commit -m "feat: add AffinityIndex — explicit groups, co-occurrence tracking, implicit promotion"
```

---

### Task 5: Soft eviction policy

**Files:**
- Create: `crates/trondb-routing/src/eviction.rs`
- Modify: `crates/trondb-routing/src/lib.rs`

- [ ] **Step 1: Create eviction.rs with EvictionPolicy**

```rust
use std::collections::HashMap;
use std::time::Instant;
use crate::affinity::{AffinityIndex, AffinitySource};
use crate::node::EntityId;

/// Tracks last-access time for entities in the hot tier.
pub struct LruTracker {
    access_times: HashMap<EntityId, Instant>,
}

impl LruTracker {
    pub fn new() -> Self {
        Self {
            access_times: HashMap::new(),
        }
    }

    pub fn touch(&mut self, entity: &EntityId) {
        self.access_times.insert(entity.clone(), Instant::now());
    }

    pub fn last_access(&self, entity: &EntityId) -> Option<Instant> {
        self.access_times.get(entity).copied()
    }
}

impl Default for LruTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Priority level for eviction (lower number = evict first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EvictionPriority {
    Ungrouped = 1,
    ImplicitGroup = 2,
    ExplicitGroup = 3,
}

/// Determine eviction priority for an entity.
pub fn eviction_priority(
    entity: &EntityId,
    affinity: &AffinityIndex,
) -> EvictionPriority {
    match affinity.group_for(entity) {
        None => EvictionPriority::Ungrouped,
        Some(gid) => {
            match affinity.get_group(&gid) {
                Some(group) if group.source == AffinitySource::Learned => {
                    EvictionPriority::ImplicitGroup
                }
                _ => EvictionPriority::ExplicitGroup,
            }
        }
    }
}

/// Select entities to soft-evict to make room for `needed` slots.
/// Returns entity IDs in eviction order (evict first → last).
pub fn select_eviction_candidates(
    hot_entities: &[EntityId],
    needed: usize,
    affinity: &AffinityIndex,
    lru: &LruTracker,
) -> Vec<EntityId> {
    if needed == 0 {
        return vec![];
    }

    // Classify each entity
    let mut candidates: Vec<(EntityId, EvictionPriority, Instant)> = hot_entities
        .iter()
        .map(|e| {
            let prio = eviction_priority(e, affinity);
            let last = lru.last_access(e).unwrap_or(Instant::now());
            (e.clone(), prio, last)
        })
        .collect();

    // Sort by priority (evict lower first), then by access time (older first)
    candidates.sort_by(|a, b| {
        a.1.cmp(&b.1).then(a.2.cmp(&b.2))
    });

    candidates.into_iter().take(needed).map(|(e, _, _)| e).collect()
}
```

- [ ] **Step 2: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::affinity::AffinityIndex;
    use crate::node::AffinityGroupId;
    use trondb_core::types::LogicalId;

    fn eid(s: &str) -> EntityId {
        LogicalId::from_string(s)
    }

    #[test]
    fn ungrouped_evicted_first() {
        let affinity = AffinityIndex::new();
        let gid = AffinityGroupId::from_string("g1");
        affinity.create_group(gid.clone()).unwrap();
        affinity.add_to_group(&eid("grouped"), &gid, 500).unwrap();

        let lru = LruTracker::new();
        let hot = vec![eid("ungrouped"), eid("grouped")];
        let evict = select_eviction_candidates(&hot, 1, &affinity, &lru);
        assert_eq!(evict, vec![eid("ungrouped")]);
    }

    #[test]
    fn lru_ordering_within_priority() {
        let affinity = AffinityIndex::new();
        let mut lru = LruTracker::new();

        // Touch "newer" after "older"
        lru.touch(&eid("older"));
        std::thread::sleep(std::time::Duration::from_millis(10));
        lru.touch(&eid("newer"));

        let hot = vec![eid("newer"), eid("older")];
        let evict = select_eviction_candidates(&hot, 1, &affinity, &lru);
        assert_eq!(evict, vec![eid("older")]);
    }

    #[test]
    fn eviction_priority_ordering() {
        assert!(EvictionPriority::Ungrouped < EvictionPriority::ImplicitGroup);
        assert!(EvictionPriority::ImplicitGroup < EvictionPriority::ExplicitGroup);
    }

    #[test]
    fn select_zero_needed_returns_empty() {
        let affinity = AffinityIndex::new();
        let lru = LruTracker::new();
        let evict = select_eviction_candidates(&[eid("a")], 0, &affinity, &lru);
        assert!(evict.is_empty());
    }
}
```

- [ ] **Step 3: Update lib.rs**

Add `pub mod eviction;` and re-export:

```rust
pub use eviction::{EvictionPriority, LruTracker, eviction_priority, select_eviction_candidates};
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-routing`
Expected: all tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-routing/
git commit -m "feat: add soft eviction policy — LRU tracking, priority-based candidate selection"
```

---

## Chunk 2: Semantic Router + scoring + TQL extensions

### Task 6: SemanticRouter — candidate scoring, verb fit, entity affinity

**Files:**
- Create: `crates/trondb-routing/src/router.rs`
- Modify: `crates/trondb-routing/src/lib.rs`

- [ ] **Step 1: Create router.rs with scoring functions**

```rust
use std::sync::Arc;
use dashmap::DashMap;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use trondb_core::planner::Plan;
use trondb_core::result::QueryResult;

use crate::affinity::AffinityIndex;
use crate::config::RouterConfig;
use crate::error::RouterError;
use crate::health::{compute_load_score, HealthCache, HealthSignal, NodeStatus};
use crate::node::{EntityId, NodeHandle, NodeId, QueryVerb, RoutingStrategy};

// ---------------------------------------------------------------------------
// AnnotatedPlan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AnnotatedPlan {
    pub plan: Plan,
    pub verb: QueryVerb,
    pub acu: f32,
    pub targets: Vec<EntityId>,
}

// ---------------------------------------------------------------------------
// RoutingDecision
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RoutingDecision {
    pub node: NodeId,
    pub score: f32,
    pub all_candidates: Vec<ScoredCandidate>,
    pub strategy: RoutingStrategy,
}

#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    pub node: NodeId,
    pub score: f32,
    pub health_score: f32,
    pub verb_score: f32,
    pub affinity_score: f32,
}

// ---------------------------------------------------------------------------
// Scoring functions
// ---------------------------------------------------------------------------

pub fn verb_fit_score(verb: QueryVerb, signal: &HealthSignal) -> f32 {
    match verb {
        QueryVerb::Infer => {
            if signal.cpu_utilisation < 0.40 {
                1.0
            } else if signal.cpu_utilisation < 0.65 {
                0.5
            } else {
                0.1
            }
        }
        QueryVerb::Search => {
            let baseline = 10.0_f32; // matches default config
            1.0 - (signal.hnsw_p99_ms / baseline).min(1.0)
        }
        QueryVerb::Traverse => {
            if signal.queue_capacity > 0 {
                1.0 - (signal.queue_depth as f32 / signal.queue_capacity as f32)
            } else {
                0.5
            }
        }
        QueryVerb::Fetch => 0.5,
    }
}

pub fn entity_affinity_score(
    targets: &[EntityId],
    node: &NodeId,
    affinity: &AffinityIndex,
) -> f32 {
    if targets.is_empty() {
        return 0.5; // SEARCH: neutral
    }
    let on_node = targets
        .iter()
        .filter(|e| affinity.preferred_node(e).as_ref() == Some(node))
        .count();
    on_node as f32 / targets.len() as f32
}

pub fn routing_score(
    node: &NodeId,
    verb: QueryVerb,
    targets: &[EntityId],
    health: &HealthCache,
    affinity: &AffinityIndex,
    config: &RouterConfig,
) -> Option<ScoredCandidate> {
    let signal = health.get(node)?;
    let hs = 1.0 - signal.load_score;
    let vs = verb_fit_score(verb, &signal);
    let af = entity_affinity_score(targets, node, affinity);

    Some(ScoredCandidate {
        node: node.clone(),
        score: (hs * config.weight_health)
            + (vs * config.weight_verb_fit)
            + (af * config.weight_affinity),
        health_score: hs,
        verb_score: vs,
        affinity_score: af,
    })
}

/// Estimate ACU for a plan.
pub fn estimate_acu(plan: &Plan) -> f32 {
    match plan {
        Plan::Fetch(_) => 1.0,
        Plan::Search(s) => 10.0 * s.k as f32,
        Plan::Traverse(t) => 5.0 * t.depth as f32,
        Plan::Explain(inner) => estimate_acu(inner),
        _ => 1.0, // DDL operations are cheap
    }
}

/// Determine query verb from plan variant.
pub fn plan_verb(plan: &Plan) -> QueryVerb {
    match plan {
        Plan::Fetch(_) => QueryVerb::Fetch,
        Plan::Search(_) => QueryVerb::Search,
        Plan::Traverse(_) => QueryVerb::Traverse,
        Plan::Explain(inner) => plan_verb(inner),
        _ => QueryVerb::Fetch, // DDL treated as cheap fetch-like
    }
}

/// Extract target entity IDs from a plan (for affinity scoring).
pub fn plan_targets(plan: &Plan) -> Vec<EntityId> {
    match plan {
        Plan::Traverse(t) => {
            vec![trondb_core::types::LogicalId::from_string(&t.from_id)]
        }
        Plan::Explain(inner) => plan_targets(inner),
        _ => vec![], // FETCH/SEARCH don't have known targets pre-execution
    }
}
```

- [ ] **Step 2: Write scoring tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RouterConfig;
    use crate::health::HealthSignal;
    use crate::node::{NodeId, NodeRole};
    use trondb_core::types::LogicalId;

    fn make_signal(node: &str, load: f32, cpu: f32, hnsw_p99: f32, queue: u32) -> HealthSignal {
        HealthSignal {
            node_id: NodeId::from_string(node),
            node_role: NodeRole::HotNode,
            signal_ts: 0,
            sequence: 0,
            cpu_utilisation: cpu,
            ram_pressure: 0.0,
            hot_entity_count: 0,
            hot_tier_capacity: 100_000,
            queue_depth: queue,
            queue_capacity: 1000,
            hnsw_p99_ms: hnsw_p99,
            hnsw_p50_ms: 0.0,
            replica_lag_ms: None,
            load_score: load,
            status: NodeStatus::Healthy,
        }
    }

    #[test]
    fn verb_fit_fetch_is_neutral() {
        let s = make_signal("n", 0.0, 0.0, 0.0, 0);
        assert!((verb_fit_score(QueryVerb::Fetch, &s) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn verb_fit_infer_prefers_low_cpu() {
        let idle = make_signal("n", 0.0, 0.2, 0.0, 0);
        let busy = make_signal("n", 0.0, 0.8, 0.0, 0);
        assert!((verb_fit_score(QueryVerb::Infer, &idle) - 1.0).abs() < 1e-6);
        assert!((verb_fit_score(QueryVerb::Infer, &busy) - 0.1).abs() < 1e-6);
    }

    #[test]
    fn verb_fit_search_prefers_low_hnsw_latency() {
        let fast = make_signal("n", 0.0, 0.0, 2.0, 0);
        let slow = make_signal("n", 0.0, 0.0, 10.0, 0);
        assert!(verb_fit_score(QueryVerb::Search, &fast) > verb_fit_score(QueryVerb::Search, &slow));
    }

    #[test]
    fn verb_fit_traverse_prefers_low_queue() {
        let empty = make_signal("n", 0.0, 0.0, 0.0, 0);
        let full = make_signal("n", 0.0, 0.0, 0.0, 1000);
        assert!((verb_fit_score(QueryVerb::Traverse, &empty) - 1.0).abs() < 1e-6);
        assert!((verb_fit_score(QueryVerb::Traverse, &full) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn entity_affinity_empty_targets_returns_neutral() {
        let affinity = AffinityIndex::new();
        let score = entity_affinity_score(&[], &NodeId::from_string("n"), &affinity);
        assert!((score - 0.5).abs() < 1e-6);
    }

    #[test]
    fn routing_score_combines_factors() {
        let cfg = RouterConfig::default();
        let cache = HealthCache::new(200, 3);
        let affinity = AffinityIndex::new();

        let mut s = make_signal("node-1", 0.2, 0.1, 1.0, 50);
        cache.update(s);

        let scored = routing_score(
            &NodeId::from_string("node-1"),
            QueryVerb::Fetch,
            &[],
            &cache,
            &affinity,
            &cfg,
        )
        .unwrap();

        // health: (1.0 - 0.2) * 0.40 = 0.32
        // verb: 0.5 * 0.30 = 0.15
        // affinity: 0.5 * 0.30 = 0.15
        // total: 0.62
        assert!((scored.score - 0.62).abs() < 1e-6, "got {}", scored.score);
    }

    #[test]
    fn estimate_acu_by_verb() {
        use trondb_core::planner::*;
        use trondb_tql::FieldList;

        let fetch_plan = Plan::Fetch(FetchPlan {
            collection: "c".into(),
            fields: FieldList::All,
            filter: None,
            limit: None,
            strategy: FetchStrategy::FullScan,
        });
        assert!((estimate_acu(&fetch_plan) - 1.0).abs() < 1e-6);

        let search_plan = Plan::Search(SearchPlan {
            collection: "c".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0]),
            sparse_vector: None,
            filter: None,
            pre_filter: None,
            k: 10,
            confidence_threshold: 0.0,
            strategy: SearchStrategy::Hnsw,
        });
        assert!((estimate_acu(&search_plan) - 100.0).abs() < 1e-6);
    }
}
```

- [ ] **Step 3: Update lib.rs**

Add `pub mod router;` and re-exports:

```rust
pub use router::{
    AnnotatedPlan, RoutingDecision, ScoredCandidate,
    estimate_acu, entity_affinity_score, plan_verb, plan_targets,
    routing_score, verb_fit_score,
};
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-routing`
Expected: all tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-routing/
git commit -m "feat: add candidate scoring — verb fit, entity affinity, routing score, ACU estimation"
```

---

### Task 7: SemanticRouter — route pipeline, dispatch, health polling

**Files:**
- Modify: `crates/trondb-routing/src/router.rs`
- Modify: `crates/trondb-routing/src/lib.rs`

- [ ] **Step 1: Add SemanticRouter struct and route_and_execute**

Add to `router.rs`:

```rust
// ---------------------------------------------------------------------------
// SemanticRouter
// ---------------------------------------------------------------------------

pub struct SemanticRouter {
    health: Arc<HealthCache>,
    affinity: Arc<AffinityIndex>,
    nodes: DashMap<NodeId, Arc<dyn NodeHandle>>,
    config: RouterConfig,
    shutdown_tx: watch::Sender<bool>,
    _poll_handle: JoinHandle<()>,
}

impl SemanticRouter {
    pub fn new(
        nodes: Vec<Arc<dyn NodeHandle>>,
        config: RouterConfig,
    ) -> Self {
        let health = Arc::new(HealthCache::new(
            config.health.push_interval_ms,
            config.health.stale_multiplier,
        ));
        let affinity = Arc::new(AffinityIndex::new());
        let node_map: DashMap<NodeId, Arc<dyn NodeHandle>> = DashMap::new();
        for node in &nodes {
            node_map.insert(node.node_id().clone(), Arc::clone(node));
        }

        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        // Clone for background task
        let poll_nodes = node_map.clone();
        let poll_health = Arc::clone(&health);
        let poll_config = config.health.clone();

        let poll_handle = tokio::spawn(async move {
            health_poll_loop(poll_nodes, poll_health, poll_config, shutdown_rx).await;
        });

        Self {
            health,
            affinity,
            nodes: node_map,
            config,
            shutdown_tx,
            _poll_handle: poll_handle,
        }
    }

    pub fn health_cache(&self) -> &Arc<HealthCache> {
        &self.health
    }

    pub fn affinity_index(&self) -> &Arc<AffinityIndex> {
        &self.affinity
    }

    /// Route and execute a plan. Main entry point.
    pub async fn route_and_execute(
        &self,
        plan: &Plan,
    ) -> Result<QueryResult, RouterError> {
        let annotated = self.annotate(plan);
        let decision = self.route(&annotated)?;
        let node = self.nodes.get(&decision.node)
            .ok_or(RouterError::NoCandidates)?;
        let result = node.execute(plan).await?;
        Ok(result)
    }

    fn annotate(&self, plan: &Plan) -> AnnotatedPlan {
        AnnotatedPlan {
            plan: plan.clone(),
            verb: plan_verb(plan),
            acu: estimate_acu(plan),
            targets: plan_targets(plan),
        }
    }

    fn route(&self, annotated: &AnnotatedPlan) -> Result<RoutingDecision, RouterError> {
        let strategy = match annotated.verb {
            QueryVerb::Search => RoutingStrategy::ScatterGather,
            _ => RoutingStrategy::LocationAware,
        };

        // Get all healthy, non-overloaded candidates
        let candidates: Vec<NodeId> = self.health.healthy_nodes()
            .into_iter()
            .filter(|(_, load)| *load < self.config.health.load_shed_threshold)
            .map(|(id, _)| id)
            .collect();

        if candidates.is_empty() {
            // Check if all are overloaded vs no nodes at all
            let all = self.health.healthy_nodes();
            if all.is_empty() {
                return Err(RouterError::NoCandidates);
            } else {
                return Err(RouterError::ClusterOverloaded);
            }
        }

        // Score each candidate
        let mut scored: Vec<ScoredCandidate> = candidates
            .iter()
            .filter_map(|id| {
                routing_score(
                    id,
                    annotated.verb,
                    &annotated.targets,
                    &self.health,
                    &self.affinity,
                    &self.config,
                )
            })
            .collect();

        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        let best = scored.first().ok_or(RouterError::NoCandidates)?;

        Ok(RoutingDecision {
            node: best.node.clone(),
            score: best.score,
            all_candidates: scored,
            strategy,
        })
    }
}

impl Drop for SemanticRouter {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

// ---------------------------------------------------------------------------
// Health poll loop
// ---------------------------------------------------------------------------

async fn health_poll_loop(
    nodes: DashMap<NodeId, Arc<dyn NodeHandle>>,
    health: Arc<HealthCache>,
    config: crate::config::HealthConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let interval = std::time::Duration::from_millis(config.push_interval_ms);
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await; // skip immediate first tick

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                for entry in nodes.iter() {
                    let mut signal = entry.value().health_snapshot().await;
                    signal.load_score = compute_load_score(&signal, &config);
                    // Determine status from thresholds
                    if signal.load_score >= config.load_shed_threshold {
                        signal.status = NodeStatus::Overloaded;
                    } else if signal.cpu_utilisation >= config.cpu_warn_threshold
                        || signal.ram_pressure >= config.ram_warn_threshold
                        || signal.queue_depth >= config.queue_depth_alert
                        || signal.hnsw_p99_ms >= config.hnsw_baseline_p99_ms * 2.0
                    {
                        signal.status = NodeStatus::Degraded;
                    }
                    health.update(signal);
                }
            }
            _ = shutdown.changed() => {
                break;
            }
        }
    }
}
```

- [ ] **Step 2: Write integration test for SemanticRouter**

Add to `crates/trondb-routing/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

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
        };
        let engine = std::sync::Arc::new(
            trondb_core::Engine::open(cfg).await.unwrap()
        );
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
            limit: Some(10),
            strategy: trondb_core::planner::FetchStrategy::FullScan,
        });

        // This should fail with CollectionNotFound (routed correctly, but collection doesn't exist)
        let result = router.route_and_execute(&plan).await;
        assert!(matches!(result, Err(RouterError::Engine(_))));
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p trondb-routing`
Expected: all tests pass

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-routing/
git commit -m "feat: add SemanticRouter — routing pipeline, health polling, dispatch"
```

---

### Task 8: TQL extensions — new tokens, AST nodes, parser rules

**Files:**
- Modify: `crates/trondb-tql/src/token.rs`
- Modify: `crates/trondb-tql/src/ast.rs`
- Modify: `crates/trondb-tql/src/parser.rs`

- [ ] **Step 1: Add new tokens**

In `crates/trondb-tql/src/token.rs`, add these tokens to the `Token` enum (following existing pattern with priority 10 and `ignore(ascii_case)`):

```rust
    #[token("COLLOCATE", priority = 10, ignore(ascii_case))]
    Collocate,

    #[token("AFFINITY", priority = 10, ignore(ascii_case))]
    Affinity,

    #[token("GROUP", priority = 10, ignore(ascii_case))]
    Group,

    #[token("ALTER", priority = 10, ignore(ascii_case))]
    Alter,

    #[token("DROP", priority = 10, ignore(ascii_case))]
    Drop,

    #[token("ENTITY", priority = 10, ignore(ascii_case))]
    Entity,
```

Note: `With` token already exists. `COLLOCATE WITH` is parsed as two tokens: `Collocate` then `With`.

- [ ] **Step 2: Add new AST nodes**

In `crates/trondb-tql/src/ast.rs`:

Extend `InsertStmt`:
```rust
pub struct InsertStmt {
    pub collection: String,
    pub fields: Vec<String>,
    pub values: Vec<Literal>,
    pub vectors: Vec<(String, VectorLiteral)>,
    pub collocate_with: Option<Vec<String>>,  // NEW
    pub affinity_group: Option<String>,       // NEW
}
```

Add new statement types:
```rust
#[derive(Debug, Clone, PartialEq)]
pub struct CreateAffinityGroupStmt {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlterEntityDropAffinityStmt {
    pub entity_id: String,
}
```

Add new variants to `Statement`:
```rust
pub enum Statement {
    // ... existing ...
    CreateAffinityGroup(CreateAffinityGroupStmt),
    AlterEntityDropAffinity(AlterEntityDropAffinityStmt),
}
```

- [ ] **Step 3: Update parser for INSERT colocation clauses**

In the INSERT parsing function in `parser.rs`, after parsing representation clauses and before expecting semicolon, add:

```rust
// Parse optional colocation clause
let mut collocate_with = None;
let mut affinity_group = None;
if self.peek() == Some(&Token::Collocate) {
    self.advance(); // COLLOCATE
    self.expect(&Token::With)?; // WITH
    self.expect(&Token::LParen)?;
    let mut ids = vec![];
    loop {
        let id = self.expect_string_lit()?;
        ids.push(id);
        if self.peek() != Some(&Token::Comma) {
            break;
        }
        self.advance(); // comma
    }
    self.expect(&Token::RParen)?;
    collocate_with = Some(ids);
} else if self.peek() == Some(&Token::Affinity) {
    self.advance(); // AFFINITY
    self.expect(&Token::Group)?; // GROUP
    let name = self.expect_string_lit()?;
    affinity_group = Some(name);
}
```

- [ ] **Step 4: Add parser for CREATE AFFINITY GROUP**

In the `CREATE` parsing branch, after checking for `Collection` and `Edge`, add:

```rust
Token::Affinity => {
    self.advance(); // AFFINITY
    self.expect(&Token::Group)?;
    let name = self.expect_string_lit()?;
    self.expect(&Token::Semicolon)?;
    Ok(Statement::CreateAffinityGroup(CreateAffinityGroupStmt { name }))
}
```

- [ ] **Step 5: Add parser for ALTER ENTITY ... DROP AFFINITY GROUP**

Add a new branch at the top-level parse dispatch for `Token::Alter`:

```rust
Some(Token::Alter) => {
    self.advance(); // ALTER
    self.expect(&Token::Entity)?;
    let entity_id = self.expect_string_lit()?;
    self.expect(&Token::Drop)?;
    self.expect(&Token::Affinity)?;
    self.expect(&Token::Group)?;
    self.expect(&Token::Semicolon)?;
    Ok(Statement::AlterEntityDropAffinity(AlterEntityDropAffinityStmt { entity_id }))
}
```

- [ ] **Step 6: Fix all existing InsertStmt constructions**

All existing code constructing `InsertStmt` needs the two new fields. Add `collocate_with: None, affinity_group: None` to all existing `InsertStmt` constructions in tests and parser code.

- [ ] **Step 7: Write parser tests**

```rust
#[test]
fn parse_create_affinity_group() {
    let stmt = parse("CREATE AFFINITY GROUP 'festival_cluster';").unwrap();
    match stmt {
        Statement::CreateAffinityGroup(s) => {
            assert_eq!(s.name, "festival_cluster");
        }
        _ => panic!("expected CreateAffinityGroup"),
    }
}

#[test]
fn parse_insert_with_collocate_with() {
    let stmt = parse("INSERT INTO venues (id, name) VALUES ('v1', 'Glastonbury') COLLOCATE WITH ('v2', 'v3');").unwrap();
    match stmt {
        Statement::Insert(s) => {
            assert_eq!(s.collocate_with, Some(vec!["v2".into(), "v3".into()]));
            assert_eq!(s.affinity_group, None);
        }
        _ => panic!("expected Insert"),
    }
}

#[test]
fn parse_insert_with_affinity_group() {
    let stmt = parse("INSERT INTO venues (id, name) VALUES ('v1', 'Glastonbury') AFFINITY GROUP 'festival_cluster';").unwrap();
    match stmt {
        Statement::Insert(s) => {
            assert_eq!(s.collocate_with, None);
            assert_eq!(s.affinity_group, Some("festival_cluster".into()));
        }
        _ => panic!("expected Insert"),
    }
}

#[test]
fn parse_alter_entity_drop_affinity() {
    let stmt = parse("ALTER ENTITY 'v1' DROP AFFINITY GROUP;").unwrap();
    match stmt {
        Statement::AlterEntityDropAffinity(s) => {
            assert_eq!(s.entity_id, "v1");
        }
        _ => panic!("expected AlterEntityDropAffinity"),
    }
}
```

- [ ] **Step 8: Run all TQL tests**

Run: `cargo test -p trondb-tql`
Expected: all tests pass (existing + new)

- [ ] **Step 9: Commit**

```bash
git add crates/trondb-tql/
git commit -m "feat(tql): add COLLOCATE WITH, AFFINITY GROUP, ALTER ENTITY DROP AFFINITY GROUP syntax"
```

---

### Task 9: Planner + executor updates for new plan variants

**Files:**
- Modify: `crates/trondb-core/src/planner.rs`
- Modify: `crates/trondb-core/src/executor.rs`

- [ ] **Step 1: Add new Plan variants**

In `crates/trondb-core/src/planner.rs`:

```rust
#[derive(Debug, Clone)]
pub struct CreateAffinityGroupPlan {
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct AlterEntityDropAffinityPlan {
    pub entity_id: String,
}
```

Add to `Plan` enum:
```rust
CreateAffinityGroup(CreateAffinityGroupPlan),
AlterEntityDropAffinity(AlterEntityDropAffinityPlan),
```

Add `collocate_with: Option<Vec<String>>` and `affinity_group: Option<String>` to `InsertPlan`.

- [ ] **Step 2: Update planner `plan()` function**

In the `Statement::Insert` match arm, propagate the new fields:
```rust
Statement::Insert(s) => Ok(Plan::Insert(InsertPlan {
    collection: s.collection.clone(),
    fields: s.fields.clone(),
    values: s.values.clone(),
    vectors: s.vectors.clone(),
    collocate_with: s.collocate_with.clone(),
    affinity_group: s.affinity_group.clone(),
})),
```

Add new match arms:
```rust
Statement::CreateAffinityGroup(s) => Ok(Plan::CreateAffinityGroup(
    CreateAffinityGroupPlan { name: s.name.clone() }
)),
Statement::AlterEntityDropAffinity(s) => Ok(Plan::AlterEntityDropAffinity(
    AlterEntityDropAffinityPlan { entity_id: s.entity_id.clone() }
)),
```

- [ ] **Step 3: Update executor to handle new plan variants**

In `executor.rs`, in the `execute()` match:

```rust
Plan::CreateAffinityGroup(_) | Plan::AlterEntityDropAffinity(_) => {
    // These are handled by the routing layer, not the core executor.
    // Return a simple acknowledgment.
    use std::collections::HashMap;
    Ok(QueryResult {
        columns: vec!["status".into()],
        rows: vec![Row {
            values: HashMap::from([
                ("status".to_owned(), crate::types::Value::String("OK".into())),
            ]),
            score: None,
        }],
        stats: QueryStats {
            elapsed: start.elapsed(),
            entities_scanned: 0,
            mode: QueryMode::Deterministic,
            tier: "Routing".into(),
        },
    })
}
```

- [ ] **Step 4: Fix all existing InsertPlan constructions**

Add `collocate_with: None, affinity_group: None` to all existing `InsertPlan` constructions in planner.rs tests and executor.rs.

- [ ] **Step 5: Run all core tests**

Run: `cargo test -p trondb-core`
Expected: all tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/
git commit -m "feat(core): add CreateAffinityGroup, AlterEntityDropAffinity plan variants + InsertPlan colocation fields"
```

---

### Task 10: WAL record types for affinity groups

**Files:**
- Modify: `crates/trondb-wal/src/record.rs`

- [ ] **Step 1: Add new WAL record types**

In `crates/trondb-wal/src/record.rs`, add to `RecordType` enum:

```rust
    AffinityGroupCreate = 0x60,
    AffinityGroupMember = 0x61,
    AffinityGroupRemove = 0x62,
```

- [ ] **Step 2: Update discriminant test**

Add to the `record_type_discriminants` test:
```rust
    assert_eq!(RecordType::AffinityGroupCreate as u8, 0x60);
    assert_eq!(RecordType::AffinityGroupMember as u8, 0x61);
    assert_eq!(RecordType::AffinityGroupRemove as u8, 0x62);
```

- [ ] **Step 3: Run WAL tests**

Run: `cargo test -p trondb-wal`
Expected: all tests pass

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-wal/
git commit -m "feat(wal): add AffinityGroupCreate/Member/Remove record types"
```

---

### Task 10b: Affinity group WAL logging + Fjall persistence

The routing layer must persist explicit affinity groups to WAL and Fjall so they survive restart. Implicit co-occurrence state is RAM-only (by spec) and does not need persistence.

**Files:**
- Modify: `crates/trondb-routing/src/affinity.rs`
- Modify: `crates/trondb-routing/src/router.rs`
- Modify: `crates/trondb-routing/Cargo.toml`

- [ ] **Step 1: Add WAL + Fjall integration to AffinityIndex**

Add to `crates/trondb-routing/src/affinity.rs`:

```rust
use trondb_wal::{RecordType, WalWriter};
use serde::{Serialize, Deserialize};

/// Payload for AffinityGroupCreate WAL record.
#[derive(Serialize, Deserialize)]
pub struct AffinityGroupCreatePayload {
    pub group_id: String,
}

/// Payload for AffinityGroupMember WAL record.
#[derive(Serialize, Deserialize)]
pub struct AffinityGroupMemberPayload {
    pub group_id: String,
    pub entity_id: String,
}

/// Payload for AffinityGroupRemove WAL record.
#[derive(Serialize, Deserialize)]
pub struct AffinityGroupRemovePayload {
    pub entity_id: String,
    pub group_id: String,
}
```

- [ ] **Step 2: Add WAL-logging variants of CRUD methods**

Add to `AffinityIndex`:

```rust
/// Create group with WAL logging.
pub async fn create_group_logged(
    &self,
    id: AffinityGroupId,
    wal: &WalWriter,
) -> Result<(), RouterError> {
    self.create_group(id.clone())?;
    let payload = rmp_serde::to_vec_named(&AffinityGroupCreatePayload {
        group_id: id.as_str().to_owned(),
    }).unwrap();
    let tx = wal.next_tx_id();
    wal.append(RecordType::TxBegin, "affinity", tx, 1, vec![]);
    wal.append(RecordType::AffinityGroupCreate, "affinity", tx, 1, payload);
    wal.commit(tx).await.map_err(|e| RouterError::Engine(e.into()))?;
    Ok(())
}

/// Add entity to group with WAL logging.
pub async fn add_to_group_logged(
    &self,
    entity: &EntityId,
    group_id: &AffinityGroupId,
    max_size: usize,
    wal: &WalWriter,
) -> Result<(), RouterError> {
    self.add_to_group(entity, group_id, max_size)?;
    let payload = rmp_serde::to_vec_named(&AffinityGroupMemberPayload {
        group_id: group_id.as_str().to_owned(),
        entity_id: entity.as_str().to_owned(),
    }).unwrap();
    let tx = wal.next_tx_id();
    wal.append(RecordType::TxBegin, "affinity", tx, 1, vec![]);
    wal.append(RecordType::AffinityGroupMember, "affinity", tx, 1, payload);
    wal.commit(tx).await.map_err(|e| RouterError::Engine(e.into()))?;
    Ok(())
}

/// Remove entity from group with WAL logging.
pub async fn remove_from_group_logged(
    &self,
    entity: &EntityId,
    wal: &WalWriter,
) -> Result<(), RouterError> {
    let group_id = self.group_for(entity);
    self.remove_from_group(entity);
    if let Some(gid) = group_id {
        let payload = rmp_serde::to_vec_named(&AffinityGroupRemovePayload {
            entity_id: entity.as_str().to_owned(),
            group_id: gid.as_str().to_owned(),
        }).unwrap();
        let tx = wal.next_tx_id();
        wal.append(RecordType::TxBegin, "affinity", tx, 1, vec![]);
        wal.append(RecordType::AffinityGroupRemove, "affinity", tx, 1, payload);
        wal.commit(tx).await.map_err(|e| RouterError::Engine(e.into()))?;
    }
    Ok(())
}

/// Restore affinity state from WAL records. Called during startup.
pub fn replay_affinity_record(
    &self,
    record_type: RecordType,
    payload: &[u8],
    max_group_size: usize,
) {
    match record_type {
        RecordType::AffinityGroupCreate => {
            if let Ok(p) = rmp_serde::from_slice::<AffinityGroupCreatePayload>(payload) {
                let _ = self.create_group(AffinityGroupId::from_string(&p.group_id));
            }
        }
        RecordType::AffinityGroupMember => {
            if let Ok(p) = rmp_serde::from_slice::<AffinityGroupMemberPayload>(payload) {
                let gid = AffinityGroupId::from_string(&p.group_id);
                let eid = EntityId::from_string(&p.entity_id);
                let _ = self.add_to_group(&eid, &gid, max_group_size);
            }
        }
        RecordType::AffinityGroupRemove => {
            if let Ok(p) = rmp_serde::from_slice::<AffinityGroupRemovePayload>(payload) {
                let eid = EntityId::from_string(&p.entity_id);
                self.remove_from_group(&eid);
            }
        }
        _ => {}
    }
}
```

- [ ] **Step 3: Add trondb-wal + rmp-serde deps to trondb-routing**

In `crates/trondb-routing/Cargo.toml`, add:
```toml
trondb-wal = { path = "../trondb-wal" }
rmp-serde = "1"
```

- [ ] **Step 4: Write WAL replay test**

```rust
#[test]
fn affinity_group_wal_replay() {
    let idx = AffinityIndex::new();
    let gid = AffinityGroupId::from_string("g1");

    // Simulate WAL replay: create group
    let create_payload = rmp_serde::to_vec_named(
        &AffinityGroupCreatePayload { group_id: "g1".into() }
    ).unwrap();
    idx.replay_affinity_record(RecordType::AffinityGroupCreate, &create_payload, 500);
    assert!(idx.get_group(&gid).is_some());

    // Simulate WAL replay: add member
    let member_payload = rmp_serde::to_vec_named(
        &AffinityGroupMemberPayload { group_id: "g1".into(), entity_id: "e1".into() }
    ).unwrap();
    idx.replay_affinity_record(RecordType::AffinityGroupMember, &member_payload, 500);
    assert_eq!(idx.group_for(&eid("e1")), Some(gid.clone()));

    // Simulate WAL replay: remove member
    let remove_payload = rmp_serde::to_vec_named(
        &AffinityGroupRemovePayload { entity_id: "e1".into(), group_id: "g1".into() }
    ).unwrap();
    idx.replay_affinity_record(RecordType::AffinityGroupRemove, &remove_payload, 500);
    assert_eq!(idx.group_for(&eid("e1")), None);
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p trondb-routing`
Expected: all tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-routing/
git commit -m "feat: add WAL logging + replay for explicit affinity groups"
```

---

## Chunk 3: CLI integration, EXPLAIN routing, integration tests

### Task 11: Wire CLI through SemanticRouter

**Files:**
- Modify: `crates/trondb-cli/Cargo.toml`
- Modify: `crates/trondb-cli/src/main.rs`

- [ ] **Step 1: Add trondb-routing dependency**

In `crates/trondb-cli/Cargo.toml`, add:
```toml
trondb-routing = { path = "../trondb-routing" }
```

- [ ] **Step 2: Update main.rs to use router**

Replace direct `engine.execute_tql()` with router-based flow. The `Engine::execute()` and `Engine::parse_and_plan()` methods were already added in Task 3.

```rust
use trondb_routing::{RouterConfig, node::{LocalNode, NodeId}};
use trondb_routing::router::SemanticRouter;

// After Engine::open — wrap in Arc for sharing:
let engine = std::sync::Arc::new(engine);
let local_node = std::sync::Arc::new(
    LocalNode::new(engine.clone(), NodeId::from_string("local"))
) as std::sync::Arc<dyn trondb_routing::NodeHandle>;
let router = SemanticRouter::new(vec![local_node], RouterConfig::default());

// In the REPL loop, replace engine.execute_tql(&input) with:
match engine.parse_and_plan(&input) {
    Ok(plan) => {
        match router.route_and_execute(&plan).await {
            Ok(result) => println!("{}", display::format_result(&result)),
            Err(e) => eprintln!("Error: {e}"),
        }
    }
    Err(e) => eprintln!("Error: {e}"),
}
```

- [ ] **Step 3: Run CLI manually to verify**

Run: `cargo run -p trondb-cli`
Expected: REPL starts, basic commands work (CREATE COLLECTION, INSERT, FETCH, SEARCH)

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-cli/ crates/trondb-core/
git commit -m "feat(cli): wire REPL through SemanticRouter with LocalNode"
```

---

### Task 12: EXPLAIN routing section

**Files:**
- Modify: `crates/trondb-routing/src/router.rs`
- Modify: `crates/trondb-core/src/executor.rs`

- [ ] **Step 1: Add routing decision to EXPLAIN output**

The current EXPLAIN handler in `executor.rs` builds a string description of the plan. We need to extend this to include routing info. Since the router calls `executor.execute()` and EXPLAIN is handled by the executor, the cleanest approach is:

In `router.rs`, add a method to `SemanticRouter`:
```rust
pub fn last_routing_decision(&self) -> Option<RoutingDecision> {
    // Store last decision in an Arc<Mutex<Option<RoutingDecision>>>
    // Updated in route_and_execute
}
```

Add a field to SemanticRouter:
```rust
last_decision: Arc<std::sync::Mutex<Option<RoutingDecision>>>,
```

Update `route_and_execute` to store the decision before dispatch.

For EXPLAIN plans specifically, the router should include routing info in the result. Modify `route_and_execute` to check if the plan is `Plan::Explain` and append routing info to the result rows.

- [ ] **Step 2: Format routing decision as result rows**

```rust
fn routing_rows(decision: &RoutingDecision) -> Vec<trondb_core::result::Row> {
    use std::collections::HashMap;
    use trondb_core::types::Value;
    use trondb_core::result::Row;

    let mut rows = vec![];
    let make_row = |prop: &str, val: &str| Row {
        values: HashMap::from([
            ("property".to_owned(), Value::String(prop.to_owned())),
            ("value".to_owned(), Value::String(val.to_owned())),
        ]),
        score: None,
    };
    rows.push(make_row("routing", &format!("{:?}", decision.strategy)));
    rows.push(make_row("selected_node", decision.node.as_str()));
    rows.push(make_row("routing_score", &format!("{:.3}", decision.score)));
    for c in &decision.all_candidates {
        rows.push(make_row(
            &format!("candidate:{}", c.node.as_str()),
            &format!(
                "score={:.3} health={:.3} verb={:.3} affinity={:.3}",
                c.score, c.health_score, c.verb_score, c.affinity_score
            ),
        ));
    }
    rows
}
```

- [ ] **Step 3: Write test**

```rust
#[tokio::test]
async fn explain_includes_routing_section() {
    // Setup engine + router + create collection
    // Run EXPLAIN FETCH * FROM collection;
    // Assert result rows contain "Routing" and "SelectedNode"
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-routing`
Expected: all tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-routing/ crates/trondb-core/
git commit -m "feat: add routing section to EXPLAIN output"
```

---

### Task 13: Integration tests — multi-node, load shedding, affinity

**Files:**
- Modify: `crates/trondb-routing/src/lib.rs` (integration tests)

- [ ] **Step 1: Multi-node routing test**

```rust
#[tokio::test]
async fn multi_node_routing_prefers_healthy_node() {
    // Create 3 SimulatedNodes with different load scores
    // Node 1: load 0.2 (best)
    // Node 2: load 0.5
    // Node 3: load 0.8
    // Route a FETCH → should pick node 1
}
```

- [ ] **Step 2: Load shedding test**

```rust
#[tokio::test]
async fn load_shedding_avoids_overloaded_node() {
    // Create 2 nodes, set one to load_score > 0.85 (overloaded)
    // Route → should pick the other node
}

#[tokio::test]
async fn all_nodes_overloaded_returns_cluster_overloaded() {
    // Create 2 nodes, both overloaded
    // Route → should return ClusterOverloaded error
}
```

- [ ] **Step 3: Affinity scoring test**

```rust
#[tokio::test]
async fn routing_prefers_node_with_entity_affinity() {
    // Create 2 nodes, put entity in affinity group with target_node = node-2
    // Route a TRAVERSE from that entity → should prefer node-2
}
```

- [ ] **Step 4: Co-occurrence learning test**

```rust
#[tokio::test]
async fn cooccurrence_learning_creates_implicit_groups() {
    // Record multiple co-occurrences for a pair
    // Call promote_and_decay
    // Verify implicit group was created
}
```

- [ ] **Step 5: Affinity group persistence test**

```rust
#[tokio::test]
async fn affinity_group_crud() {
    // Create group, add member, verify membership
    // Remove member, verify removal
    // Attempt full group, verify error
}
```

- [ ] **Step 6: Run all tests**

Run: `cargo test --workspace`
Expected: all tests pass (existing + new)

- [ ] **Step 7: Commit**

```bash
git add crates/trondb-routing/
git commit -m "test: add integration tests — multi-node routing, load shedding, affinity learning"
```

---

### Task 14: Update CLAUDE.md + final clippy pass

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update CLAUDE.md header and routing section**

Change header from "Phase 5a" to "Phase 6: Routing Intelligence". Add routing section:

```markdown
- Routing Intelligence: SemanticRouter with health signals, co-location, semantic routing
  - NodeHandle trait: LocalNode (in-process), SimulatedNode (tests), RemoteNode (Phase 6b)
  - HealthSignal + HealthCache: load score computation (RAM 0.35, queue 0.30, CPU 0.20, HNSW 0.10, lag 0.05)
  - AffinityIndex: explicit groups (WAL-logged, Fjall-persisted) + implicit co-occurrence (RAM-only)
  - Candidate scoring: health (40%) + verb fit (30%) + entity affinity (30%)
  - Soft eviction: ungrouped LRU → implicit groups → explicit groups
  - Background tasks: health polling (200ms), implicit affinity promotion (30s)
  - TQL: COLLOCATE WITH, AFFINITY GROUP, CREATE AFFINITY GROUP, ALTER ENTITY DROP AFFINITY GROUP
  - EXPLAIN shows routing section (selected node, score breakdown, candidates)
```

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: no warnings

- [ ] **Step 3: Run full test suite**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: update CLAUDE.md for Phase 6 — routing intelligence"
```

---

## Summary

| Task | Description | Files | Estimated Complexity |
|------|-------------|-------|---------------------|
| 1 | Crate scaffold + identity types + config + error | 6 new, 1 modified | Simple |
| 2 | HealthSignal + HealthCache + load score | 1 new | Simple |
| 3 | NodeHandle trait + LocalNode + SimulatedNode + Engine::execute/parse_and_plan | 3 modified | Medium |
| 4 | AffinityIndex — groups, co-occurrence | 1 new | Medium |
| 5 | Soft eviction policy | 1 new | Simple |
| 6 | Candidate scoring — verb fit, affinity, ACU | 1 new | Medium |
| 7 | SemanticRouter — route pipeline, dispatch | 1 modified | Complex |
| 8 | TQL extensions — tokens, AST, parser | 3 modified | Medium |
| 9 | Planner + executor updates | 2 modified | Medium |
| 10 | WAL record types | 1 modified | Simple |
| 10b | Affinity group WAL logging + Fjall persistence | 2 modified | Medium |
| 11 | Wire CLI through router | 2 modified | Medium |
| 12 | EXPLAIN routing section | 2 modified | Medium |
| 13 | Integration tests | 1 modified | Medium |
| 14 | CLAUDE.md + clippy | 1 modified | Simple |
