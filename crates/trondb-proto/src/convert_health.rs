//! HealthSignal <-> Proto conversions.
//!
//! Implements `From<&HealthSignal> for pb::HealthSignalResponse` and
//! `TryFrom<pb::HealthSignalResponse> for HealthSignal` with helpers for
//! `NodeRole` ↔ `NodeRoleProto` and `NodeStatus` ↔ `NodeStatusProto`.

use crate::pb;
use trondb_routing::health::{HealthSignal, NodeStatus};
use trondb_routing::node::{NodeId, NodeRole};

// ---------------------------------------------------------------------------
// NodeRole helpers
// ---------------------------------------------------------------------------

fn node_role_to_proto(role: NodeRole) -> pb::NodeRoleProto {
    match role {
        NodeRole::Primary => pb::NodeRoleProto::NodeRolePrimary,
        NodeRole::HotNode => pb::NodeRoleProto::NodeRoleHotNode,
        NodeRole::WarmNode => pb::NodeRoleProto::NodeRoleWarmNode,
        NodeRole::ReadReplica => pb::NodeRoleProto::NodeRoleReadReplica,
        NodeRole::Router => pb::NodeRoleProto::NodeRoleRouter,
    }
}

fn proto_to_node_role(proto: i32) -> Result<NodeRole, String> {
    match pb::NodeRoleProto::try_from(proto) {
        Ok(pb::NodeRoleProto::NodeRolePrimary) => Ok(NodeRole::Primary),
        Ok(pb::NodeRoleProto::NodeRoleHotNode) => Ok(NodeRole::HotNode),
        Ok(pb::NodeRoleProto::NodeRoleWarmNode) => Ok(NodeRole::WarmNode),
        Ok(pb::NodeRoleProto::NodeRoleReadReplica) => Ok(NodeRole::ReadReplica),
        Ok(pb::NodeRoleProto::NodeRoleRouter) => Ok(NodeRole::Router),
        Err(_) => Err(format!("unknown NodeRoleProto value: {proto}")),
    }
}

// ---------------------------------------------------------------------------
// NodeStatus helpers
// ---------------------------------------------------------------------------

fn node_status_to_proto(status: NodeStatus) -> pb::NodeStatusProto {
    match status {
        NodeStatus::Healthy => pb::NodeStatusProto::NodeStatusHealthy,
        NodeStatus::Degraded => pb::NodeStatusProto::NodeStatusDegraded,
        NodeStatus::Overloaded => pb::NodeStatusProto::NodeStatusOverloaded,
        NodeStatus::Faulted => pb::NodeStatusProto::NodeStatusFaulted,
    }
}

fn proto_to_node_status(proto: i32) -> Result<NodeStatus, String> {
    match pb::NodeStatusProto::try_from(proto) {
        Ok(pb::NodeStatusProto::NodeStatusHealthy) => Ok(NodeStatus::Healthy),
        Ok(pb::NodeStatusProto::NodeStatusDegraded) => Ok(NodeStatus::Degraded),
        Ok(pb::NodeStatusProto::NodeStatusOverloaded) => Ok(NodeStatus::Overloaded),
        Ok(pb::NodeStatusProto::NodeStatusFaulted) => Ok(NodeStatus::Faulted),
        Err(_) => Err(format!("unknown NodeStatusProto value: {proto}")),
    }
}

// ---------------------------------------------------------------------------
// HealthSignal -> HealthSignalResponse
// ---------------------------------------------------------------------------

impl From<&HealthSignal> for pb::HealthSignalResponse {
    fn from(signal: &HealthSignal) -> Self {
        pb::HealthSignalResponse {
            node_id: signal.node_id.as_str().to_owned(),
            node_role: node_role_to_proto(signal.node_role) as i32,
            signal_ts: signal.signal_ts,
            sequence: signal.sequence,
            cpu_utilisation: signal.cpu_utilisation,
            ram_pressure: signal.ram_pressure,
            hot_entity_count: signal.hot_entity_count,
            hot_tier_capacity: signal.hot_tier_capacity,
            warm_entity_count: signal.warm_entity_count,
            archive_entity_count: signal.archive_entity_count,
            queue_depth: signal.queue_depth,
            queue_capacity: signal.queue_capacity,
            hnsw_p50_ms: signal.hnsw_p50_ms,
            hnsw_p99_ms: signal.hnsw_p99_ms,
            replica_lag_ms: signal.replica_lag_ms,
            load_score: signal.load_score,
            status: node_status_to_proto(signal.status) as i32,
        }
    }
}

// ---------------------------------------------------------------------------
// HealthSignalResponse -> HealthSignal
// ---------------------------------------------------------------------------

impl TryFrom<pb::HealthSignalResponse> for HealthSignal {
    type Error = String;

    fn try_from(proto: pb::HealthSignalResponse) -> Result<Self, Self::Error> {
        Ok(HealthSignal {
            node_id: NodeId::from_string(&proto.node_id),
            node_role: proto_to_node_role(proto.node_role)?,
            signal_ts: proto.signal_ts,
            sequence: proto.sequence,
            cpu_utilisation: proto.cpu_utilisation,
            ram_pressure: proto.ram_pressure,
            hot_entity_count: proto.hot_entity_count,
            hot_tier_capacity: proto.hot_tier_capacity,
            warm_entity_count: proto.warm_entity_count,
            archive_entity_count: proto.archive_entity_count,
            queue_depth: proto.queue_depth,
            queue_capacity: proto.queue_capacity,
            hnsw_p50_ms: proto.hnsw_p50_ms,
            hnsw_p99_ms: proto.hnsw_p99_ms,
            replica_lag_ms: proto.replica_lag_ms,
            load_score: proto.load_score,
            status: proto_to_node_status(proto.status)?,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use trondb_routing::health::{HealthSignal, NodeStatus};
    use trondb_routing::node::{NodeId, NodeRole};

    fn make_signal() -> HealthSignal {
        HealthSignal {
            node_id: NodeId::from_string("node-1"),
            node_role: NodeRole::Primary,
            signal_ts: 1000,
            sequence: 5,
            cpu_utilisation: 0.45,
            ram_pressure: 0.30,
            hot_entity_count: 5000,
            hot_tier_capacity: 100_000,
            warm_entity_count: 2000,
            archive_entity_count: 500,
            queue_depth: 10,
            queue_capacity: 1000,
            hnsw_p50_ms: 1.2,
            hnsw_p99_ms: 4.5,
            replica_lag_ms: Some(150),
            load_score: 0.35,
            status: NodeStatus::Healthy,
        }
    }

    #[test]
    fn health_signal_round_trip() {
        let signal = make_signal();
        let proto: pb::HealthSignalResponse = (&signal).into();
        let restored = HealthSignal::try_from(proto).unwrap();
        assert_eq!(restored.node_id, signal.node_id);
        assert_eq!(restored.node_role, NodeRole::Primary);
        assert_eq!(restored.replica_lag_ms, Some(150));
    }

    #[test]
    fn all_numeric_fields_preserved() {
        let signal = make_signal();
        let proto: pb::HealthSignalResponse = (&signal).into();
        let restored = HealthSignal::try_from(proto).unwrap();
        assert_eq!(restored.signal_ts, 1000);
        assert_eq!(restored.sequence, 5);
        assert_eq!(restored.hot_entity_count, 5000);
        assert_eq!(restored.hot_tier_capacity, 100_000);
        assert_eq!(restored.warm_entity_count, 2000);
        assert_eq!(restored.archive_entity_count, 500);
        assert_eq!(restored.queue_depth, 10);
        assert_eq!(restored.queue_capacity, 1000);
        assert!((restored.cpu_utilisation - 0.45).abs() < 1e-6);
        assert!((restored.ram_pressure - 0.30).abs() < 1e-6);
        assert!((restored.hnsw_p50_ms - 1.2).abs() < 1e-5);
        assert!((restored.hnsw_p99_ms - 4.5).abs() < 1e-5);
        assert!((restored.load_score - 0.35).abs() < 1e-6);
    }

    #[test]
    fn replica_lag_none_round_trip() {
        let mut signal = make_signal();
        signal.replica_lag_ms = None;
        let proto: pb::HealthSignalResponse = (&signal).into();
        let restored = HealthSignal::try_from(proto).unwrap();
        assert_eq!(restored.replica_lag_ms, None);
    }

    #[test]
    fn all_node_roles_round_trip() {
        let roles = [
            NodeRole::Primary,
            NodeRole::HotNode,
            NodeRole::WarmNode,
            NodeRole::ReadReplica,
            NodeRole::Router,
        ];
        for role in &roles {
            let proto_val = node_role_to_proto(*role) as i32;
            let restored = proto_to_node_role(proto_val).unwrap();
            assert_eq!(restored, *role, "failed for {role:?}");
        }
    }

    #[test]
    fn all_node_statuses_round_trip() {
        let statuses = [
            NodeStatus::Healthy,
            NodeStatus::Degraded,
            NodeStatus::Overloaded,
            NodeStatus::Faulted,
        ];
        for status in &statuses {
            let proto_val = node_status_to_proto(*status) as i32;
            let restored = proto_to_node_status(proto_val).unwrap();
            assert_eq!(restored, *status, "failed for {status:?}");
        }
    }

    #[test]
    fn faulted_node_round_trip() {
        let mut signal = make_signal();
        signal.node_role = NodeRole::Router;
        signal.status = NodeStatus::Faulted;
        let proto: pb::HealthSignalResponse = (&signal).into();
        let restored = HealthSignal::try_from(proto).unwrap();
        assert_eq!(restored.node_role, NodeRole::Router);
        assert_eq!(restored.status, NodeStatus::Faulted);
    }
}
