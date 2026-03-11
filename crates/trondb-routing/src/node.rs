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
