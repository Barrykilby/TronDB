pub mod affinity;
pub mod config;
pub mod error;
pub mod eviction;
pub mod health;
pub mod node;

pub use affinity::{AffinityGroup, AffinityIndex, AffinitySource};
pub use config::{ColocationConfig, HealthConfig, RouterConfig};
pub use error::RouterError;
pub use eviction::{EvictionPriority, LruTracker, eviction_priority, select_eviction_candidates};
pub use health::{compute_load_score, HealthCache, HealthSignal, NodeStatus};
pub use node::{
    AffinityGroupId, EntityId, LocalNode, NodeHandle, NodeId, NodeRole,
    QueryVerb, RoutingStrategy, SimulatedNode,
};
