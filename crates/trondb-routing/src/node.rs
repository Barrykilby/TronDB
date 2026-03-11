use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use trondb_core::error::EngineError;
use trondb_core::result::QueryResult;
use trondb_core::Engine;

use crate::health::{HealthSignal, NodeStatus};

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
            warm_entity_count: 0,
            archive_entity_count: 0,
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
            warm_entity_count: 0,
            archive_entity_count: 0,
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
