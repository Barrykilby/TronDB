# Phase 9: Multi-Node Distribution — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Transform TronDB from a single-process database into a distributed cluster with write primary, read replicas, stateless router nodes, and gRPC transport.

**Architecture:** A single `trondb-server` binary runs as primary, replica, or router based on config. Primary streams WAL records to replicas via gRPC bidirectional streams and broadcasts Location Table updates to routers. Replicas apply the WAL stream to their local Engine. Routers hold a replicated Location Table + HealthCache and dispatch queries via SemanticRouter. All writes flow through the primary (semi-synchronous replication with configurable ack quorum).

**Tech Stack:** tonic (gRPC), prost (protobuf), sysinfo (CPU/RAM metrics), toml (config parsing), tracing + tracing-subscriber (structured logging), tonic-health (container probes), clap (CLI)

**Spec:** `docs/superpowers/specs/2026-03-11-trondb-phase9-design.md`

---

## File Structure

### New crates

**`crates/trondb-proto/`** — Protobuf definitions + tonic-generated code + Rust ↔ proto conversions

| File | Purpose |
|------|---------|
| `Cargo.toml` | Dependencies: tonic, prost, tonic-build, trondb-core, trondb-tql, trondb-wal, trondb-routing |
| `build.rs` | tonic-build code generation from .proto |
| `proto/trondb.proto` | Service + message definitions |
| `src/lib.rs` | Re-export generated code, module declarations |
| `src/convert_plan.rs` | `Plan` ↔ proto conversion (all 16 variants + nested TQL types) |
| `src/convert_result.rs` | `QueryResult` ↔ proto conversion |
| `src/convert_wal.rs` | `WalRecord` ↔ proto conversion |
| `src/convert_health.rs` | `HealthSignal` ↔ proto conversion |

**`crates/trondb-server/`** — gRPC server binary

| File | Purpose |
|------|---------|
| `Cargo.toml` | Dependencies: all crates + tonic + tokio + clap + tracing + sysinfo |
| `src/main.rs` | Entry point: config loading, role-based startup, graceful shutdown |
| `src/config.rs` | `ClusterConfig` struct, TOML parsing, env var overrides |
| `src/service.rs` | `TronNodeService` — Execute + HealthSnapshot RPCs |
| `src/stream_health.rs` | StreamHealth push RPC implementation |
| `src/replication.rs` | `ReplicaTracker` + StreamWal bidirectional stream |
| `src/location_stream.rs` | StreamLocationUpdates — primary → router |
| `src/write_forward.rs` | Write forwarding — replica/router → primary |
| `src/scatter.rs` | Scatter-gather SEARCH fan-out + merge |
| `src/metrics.rs` | Real CPU/RAM metrics via sysinfo, rolling percentile |

### Modified files

| File | Change |
|------|--------|
| `Cargo.toml` (workspace) | Add `trondb-proto` and `trondb-server` to members + new workspace deps |
| `crates/trondb-routing/src/node.rs` | Add `Primary`, `Router` to `NodeRole`; add `Serialize`/`Deserialize` derives |
| `crates/trondb-routing/src/lib.rs` | Export new `startup` module |
| `crates/trondb-routing/src/startup.rs` (new) | `process_pending_wal_records()` — TierMigration + AffinityGroup wiring |
| `crates/trondb-core/src/lib.rs` | Add `hnsw_snapshot_interval_secs` to `EngineConfig`; add atomic query counter |
| `crates/trondb-routing/src/router.rs` | ScatterGather conditional on multi-node collections |

### New top-level files

| File | Purpose |
|------|--------|
| `Dockerfile` | Multi-stage build for trondb-server |
| `docker-compose.yml` | 3-node cluster (primary + replica + router) |

---

## Chunk 1: Foundation

### Task 1: Workspace Setup + Crate Scaffolds

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Create: `crates/trondb-proto/Cargo.toml`
- Create: `crates/trondb-proto/build.rs`
- Create: `crates/trondb-proto/src/lib.rs`
- Create: `crates/trondb-server/Cargo.toml`
- Create: `crates/trondb-server/src/main.rs`

- [ ] **Step 1: Add workspace dependencies**

Add to `Cargo.toml` workspace dependencies:

```toml
tonic = "0.13"
prost = "0.13"
prost-types = "0.13"
tonic-build = "0.13"
tonic-health = "0.13"
clap = { version = "4", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }
sysinfo = "0.34"
toml = "0.8"
```

Add new members:

```toml
members = [
    "crates/trondb-core",
    "crates/trondb-tql",
    "crates/trondb-cli",
    "crates/trondb-wal",
    "crates/trondb-routing",
    "crates/trondb-proto",
    "crates/trondb-server",
]
```

- [ ] **Step 2: Create trondb-proto crate**

`crates/trondb-proto/Cargo.toml`:

```toml
[package]
name = "trondb-proto"
version = "0.1.0"
edition = "2021"

[dependencies]
tonic = { workspace = true }
prost = { workspace = true }
prost-types = { workspace = true }
trondb-core = { path = "../trondb-core" }
trondb-tql = { path = "../trondb-tql" }
trondb-wal = { path = "../trondb-wal" }
trondb-routing = { path = "../trondb-routing" }

[build-dependencies]
tonic-build = { workspace = true }
```

`crates/trondb-proto/build.rs`:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("proto/trondb.proto")?;
    Ok(())
}
```

`crates/trondb-proto/src/lib.rs`:

```rust
pub mod pb {
    tonic::include_proto!("trondb");
}
```

- [ ] **Step 3: Create trondb-server crate**

`crates/trondb-server/Cargo.toml`:

```toml
[package]
name = "trondb-server"
version = "0.1.0"
edition = "2021"

[dependencies]
trondb-core = { path = "../trondb-core" }
trondb-tql = { path = "../trondb-tql" }
trondb-wal = { path = "../trondb-wal" }
trondb-routing = { path = "../trondb-routing" }
trondb-proto = { path = "../trondb-proto" }
tonic = { workspace = true }
tonic-health = { workspace = true }
prost = { workspace = true }
tokio = { workspace = true }
clap = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
sysinfo = { workspace = true }
toml = { workspace = true }
serde = { workspace = true }
```

`crates/trondb-server/src/main.rs`:

```rust
fn main() {
    println!("trondb-server placeholder");
}
```

- [ ] **Step 4: Create empty proto file and verify workspace compiles**

Create `crates/trondb-proto/proto/trondb.proto`:

```protobuf
syntax = "proto3";
package trondb;
```

Run: `cargo check --workspace`
Expected: All crates compile

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-proto/ crates/trondb-server/ Cargo.toml
git commit -m "feat(phase9): scaffold trondb-proto and trondb-server crates"
```

---

### Task 2: Extend NodeRole Enum

**Files:**
- Modify: `crates/trondb-routing/src/node.rs:62-67`

- [ ] **Step 1: Write test for new variants**

Add to the existing `tests` module in `crates/trondb-routing/src/node.rs`:

```rust
#[test]
fn node_role_primary_and_router_variants() {
    let p = NodeRole::Primary;
    let r = NodeRole::Router;
    assert_ne!(p, r);
    assert_eq!(p, NodeRole::Primary);
    assert_eq!(r, NodeRole::Router);
}

#[test]
fn node_role_display() {
    assert_eq!(format!("{}", NodeRole::Primary), "Primary");
    assert_eq!(format!("{}", NodeRole::Router), "Router");
    assert_eq!(format!("{}", NodeRole::HotNode), "HotNode");
    assert_eq!(format!("{}", NodeRole::ReadReplica), "ReadReplica");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p trondb-routing node_role_primary`
Expected: FAIL — `Primary` variant doesn't exist

- [ ] **Step 3: Add Primary and Router variants**

In `crates/trondb-routing/src/node.rs`, modify `NodeRole`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeRole {
    Primary,
    HotNode,
    WarmNode,
    ReadReplica,
    Router,
}

impl fmt::Display for NodeRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeRole::Primary => write!(f, "Primary"),
            NodeRole::HotNode => write!(f, "HotNode"),
            NodeRole::WarmNode => write!(f, "WarmNode"),
            NodeRole::ReadReplica => write!(f, "ReadReplica"),
            NodeRole::Router => write!(f, "Router"),
        }
    }
}
```

Note: `Serialize` and `Deserialize` are already imported at the top of `node.rs` (used by `NodeId` and `AffinityGroupId`), but NOT yet derived on `NodeRole`. The `#[derive(...)]` line above adds them.

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-routing`
Expected: All pass (existing tests + new tests)

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-routing/src/node.rs
git commit -m "feat(routing): add Primary and Router variants to NodeRole"
```

---

### Task 3: Protobuf Service + Message Definitions

**Files:**
- Modify: `crates/trondb-proto/proto/trondb.proto`

This task writes the complete protobuf schema. It's a single file with all messages and the service definition. No Rust code changes — just the .proto file.

- [ ] **Step 1: Write the complete .proto file**

Replace `crates/trondb-proto/proto/trondb.proto` with the full service and message definitions:

```protobuf
syntax = "proto3";
package trondb;

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

service TronNode {
    // Execute a plan and return results
    rpc Execute(PlanRequest) returns (QueryResponse);

    // Pull health snapshot on demand
    rpc HealthSnapshot(Empty) returns (HealthSignalResponse);

    // Push health updates as a stream
    rpc StreamHealth(Empty) returns (stream HealthSignalResponse);

    // WAL replication: primary streams records, replica sends acks
    rpc StreamWal(stream WalAck) returns (stream WalRecordMessage);

    // Location Table: primary streams updates to routers
    rpc StreamLocationUpdates(Empty) returns (stream LocationUpdateMessage);
}

message Empty {}

// ---------------------------------------------------------------------------
// Plan types (maps to trondb_core::planner::Plan, 16 variants)
// ---------------------------------------------------------------------------

message PlanRequest {
    oneof plan {
        CreateCollectionPlan create_collection = 1;
        InsertPlan insert = 2;
        FetchPlan fetch = 3;
        SearchPlan search = 4;
        ExplainPlan explain = 5;
        CreateEdgeTypePlan create_edge_type = 6;
        InsertEdgePlan insert_edge = 7;
        DeleteEntityPlan delete_entity = 8;
        DeleteEdgePlan delete_edge = 9;
        TraversePlan traverse = 10;
        CreateAffinityGroupPlan create_affinity_group = 11;
        AlterEntityDropAffinityPlan alter_entity_drop_affinity = 12;
        DemotePlan demote = 13;
        PromotePlan promote = 14;
        ExplainTiersPlan explain_tiers = 15;
        UpdateEntityPlan update_entity = 16;
    }
}

message CreateCollectionPlan {
    string name = 1;
    repeated RepresentationDecl representations = 2;
    repeated FieldDecl fields = 3;
    repeated IndexDecl indexes = 4;
}

message InsertPlan {
    string collection = 1;
    repeated string fields = 2;
    repeated LiteralValue values = 3;
    repeated NamedVector vectors = 4;
    repeated string collocate_with = 5;    // empty if None
    optional string affinity_group = 6;
}

message FetchPlan {
    string collection = 1;
    FieldListProto fields = 2;
    optional WhereClauseProto filter = 3;
    optional uint64 limit = 4;
    FetchStrategyProto strategy = 5;
    string strategy_index_name = 6;    // carries index name for FieldIndexLookup/Range
}

message SearchPlan {
    string collection = 1;
    FieldListProto fields = 2;
    repeated double dense_vector = 3;      // empty if None
    repeated SparseEntry sparse_vector = 4; // empty if None
    optional WhereClauseProto filter = 5;
    optional PreFilterProto pre_filter = 6;
    uint64 k = 7;
    double confidence_threshold = 8;
    SearchStrategyProto strategy = 9;
    bool has_dense = 10;                   // disambiguate empty vec from None
    bool has_sparse = 11;
}

message ExplainPlan {
    PlanRequest inner = 1;                 // recursive
}

message CreateEdgeTypePlan {
    string name = 1;
    string from_collection = 2;
    string to_collection = 3;
    optional DecayConfigProto decay_config = 4;
}

message InsertEdgePlan {
    string edge_type = 1;
    string from_id = 2;
    string to_id = 3;
    repeated FieldAssignment metadata = 4;
}

message DeleteEntityPlan {
    string entity_id = 1;
    string collection = 2;
}

message DeleteEdgePlan {
    string edge_type = 1;
    string from_id = 2;
    string to_id = 3;
}

message TraversePlan {
    string edge_type = 1;
    string from_id = 2;
    uint64 depth = 3;
    optional uint64 limit = 4;
}

message CreateAffinityGroupPlan {
    string name = 1;
}

message AlterEntityDropAffinityPlan {
    string entity_id = 1;
}

message DemotePlan {
    string entity_id = 1;
    string collection = 2;
    TierTargetProto target_tier = 3;
}

message PromotePlan {
    string entity_id = 1;
    string collection = 2;
}

message ExplainTiersPlan {
    string collection = 1;
}

message UpdateEntityPlan {
    string entity_id = 1;
    string collection = 2;
    repeated FieldAssignment assignments = 3;
}

// ---------------------------------------------------------------------------
// Nested TQL types
// ---------------------------------------------------------------------------

message RepresentationDecl {
    string name = 1;
    optional string model = 2;
    optional uint64 dimensions = 3;
    MetricProto metric = 4;
    bool sparse = 5;
}

message FieldDecl {
    string name = 1;
    FieldTypeProto field_type = 2;
    optional string entity_ref_collection = 3;  // only set when field_type = ENTITY_REF
}

message IndexDecl {
    string name = 1;
    repeated string fields = 2;
    optional WhereClauseProto partial_condition = 3;
}

message NamedVector {
    string name = 1;
    VectorLiteralProto vector = 2;
}

message VectorLiteralProto {
    oneof vector {
        DenseVector dense = 1;
        SparseVector sparse = 2;
    }
}

message DenseVector {
    repeated double values = 1;
}

message SparseVector {
    repeated SparseEntry entries = 1;
}

message SparseEntry {
    uint32 index = 1;
    float value = 2;
}

message LiteralValue {
    oneof value {
        string string_val = 1;
        int64 int_val = 2;
        double float_val = 3;
        bool bool_val = 4;
        bool null_val = 5;             // if true, value is null
    }
}

message FieldAssignment {
    string field = 1;
    LiteralValue value = 2;
}

message WhereClauseProto {
    oneof clause {
        ComparisonClause eq = 1;
        ComparisonClause gt = 2;
        ComparisonClause lt = 3;
        ComparisonClause gte = 4;
        ComparisonClause lte = 5;
        BinaryClause and = 6;
        BinaryClause or = 7;
    }
}

message ComparisonClause {
    string field = 1;
    LiteralValue value = 2;
}

message BinaryClause {
    WhereClauseProto left = 1;
    WhereClauseProto right = 2;
}

message FieldListProto {
    bool all = 1;                      // if true, select all fields
    repeated string names = 2;         // used when all=false
}

message PreFilterProto {
    string index_name = 1;
    WhereClauseProto clause = 2;
}

// ---------------------------------------------------------------------------
// Enum types
// ---------------------------------------------------------------------------

enum MetricProto {
    METRIC_COSINE = 0;
    METRIC_INNER_PRODUCT = 1;
}

enum FieldTypeProto {
    FIELD_TYPE_TEXT = 0;
    FIELD_TYPE_DATETIME = 1;
    FIELD_TYPE_BOOL = 2;
    FIELD_TYPE_INT = 3;
    FIELD_TYPE_FLOAT = 4;
    FIELD_TYPE_ENTITY_REF = 5;
}

enum SearchStrategyProto {
    SEARCH_STRATEGY_HNSW = 0;
    SEARCH_STRATEGY_SPARSE = 1;
    SEARCH_STRATEGY_HYBRID = 2;
}

enum FetchStrategyProto {
    FETCH_STRATEGY_FULL_SCAN = 0;
    FETCH_STRATEGY_FIELD_INDEX_LOOKUP = 1;
    FETCH_STRATEGY_FIELD_INDEX_RANGE = 2;
}

enum TierTargetProto {
    TIER_TARGET_WARM = 0;
    TIER_TARGET_ARCHIVE = 1;
}

message DecayConfigProto {
    optional DecayFnProto decay_fn = 1;
    optional double decay_rate = 2;
    optional double floor = 3;
    optional double promote_threshold = 4;
    optional double prune_threshold = 5;
}

enum DecayFnProto {
    DECAY_FN_EXPONENTIAL = 0;
    DECAY_FN_LINEAR = 1;
    DECAY_FN_STEP = 2;
}

// ---------------------------------------------------------------------------
// Query response
// ---------------------------------------------------------------------------

message QueryResponse {
    repeated string columns = 1;
    repeated RowMessage rows = 2;
    QueryStatsMessage stats = 3;
}

message RowMessage {
    map<string, ValueMessage> values = 1;
    optional float score = 2;
}

message ValueMessage {
    oneof value {
        string string_val = 1;
        int64 int_val = 2;
        double float_val = 3;
        bool bool_val = 4;
        bool null_val = 5;
    }
}

message QueryStatsMessage {
    uint64 elapsed_nanos = 1;
    uint64 entities_scanned = 2;
    QueryModeProto mode = 3;
    string tier = 4;
}

enum QueryModeProto {
    QUERY_MODE_DETERMINISTIC = 0;
    QUERY_MODE_PROBABILISTIC = 1;
}

// ---------------------------------------------------------------------------
// WAL replication
// ---------------------------------------------------------------------------

message WalRecordMessage {
    uint64 lsn = 1;
    int64 timestamp = 2;               // maps to WalRecord.ts
    uint64 tx_id = 3;
    RecordTypeProto record_type = 4;
    uint32 schema_ver = 5;
    string collection = 6;
    bytes payload = 7;                 // opaque MessagePack bytes
}

message WalAck {
    uint64 confirmed_lsn = 1;
}

enum RecordTypeProto {
    RECORD_TYPE_TX_BEGIN = 0;
    RECORD_TYPE_TX_COMMIT = 1;
    RECORD_TYPE_TX_ABORT = 2;
    RECORD_TYPE_ENTITY_WRITE = 3;
    RECORD_TYPE_ENTITY_DELETE = 4;
    RECORD_TYPE_REPR_WRITE = 5;
    RECORD_TYPE_REPR_DIRTY = 6;
    RECORD_TYPE_EDGE_WRITE = 7;
    RECORD_TYPE_EDGE_INFERRED = 8;
    RECORD_TYPE_EDGE_CONFIDENCE_UPDATE = 9;
    RECORD_TYPE_EDGE_CONFIRM = 10;
    RECORD_TYPE_EDGE_DELETE = 11;
    RECORD_TYPE_LOCATION_UPDATE = 12;
    RECORD_TYPE_SCHEMA_CREATE_COLL = 13;
    RECORD_TYPE_SCHEMA_CREATE_EDGE_TYPE = 14;
    RECORD_TYPE_AFFINITY_GROUP_CREATE = 15;
    RECORD_TYPE_AFFINITY_GROUP_MEMBER = 16;
    RECORD_TYPE_AFFINITY_GROUP_REMOVE = 17;
    RECORD_TYPE_TIER_MIGRATION = 18;
    RECORD_TYPE_CHECKPOINT = 19;
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

message HealthSignalResponse {
    string node_id = 1;
    NodeRoleProto node_role = 2;
    int64 signal_ts = 3;
    uint64 sequence = 4;
    float cpu_utilisation = 5;
    float ram_pressure = 6;
    uint64 hot_entity_count = 7;
    uint64 hot_tier_capacity = 8;
    uint64 warm_entity_count = 9;
    uint64 archive_entity_count = 10;
    uint32 queue_depth = 11;
    uint32 queue_capacity = 12;
    float hnsw_p50_ms = 13;
    float hnsw_p99_ms = 14;
    optional uint64 replica_lag_ms = 15;
    float load_score = 16;
    NodeStatusProto status = 17;
}

enum NodeRoleProto {
    NODE_ROLE_PRIMARY = 0;
    NODE_ROLE_HOT_NODE = 1;
    NODE_ROLE_WARM_NODE = 2;
    NODE_ROLE_READ_REPLICA = 3;
    NODE_ROLE_ROUTER = 4;
}

enum NodeStatusProto {
    NODE_STATUS_HEALTHY = 0;
    NODE_STATUS_DEGRADED = 1;
    NODE_STATUS_OVERLOADED = 2;
    NODE_STATUS_FAULTED = 3;
}

// ---------------------------------------------------------------------------
// Location Table
// ---------------------------------------------------------------------------

message LocationUpdateMessage {
    bool is_snapshot = 1;              // true for initial full snapshot
    bytes payload = 2;                 // LocationTable::snapshot() binary for snapshot,
                                       // or single LocationUpdate WAL record for delta
    uint64 lsn = 3;                   // WAL LSN at time of this update
}
```

- [ ] **Step 2: Verify proto compiles**

Run: `cargo check -p trondb-proto`
Expected: Compiles (generated code builds)

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-proto/proto/trondb.proto
git commit -m "feat(proto): complete protobuf service and message definitions"
```

---

### Task 4: Proto Conversions — Plan ↔ Proto

**Files:**
- Create: `crates/trondb-proto/src/convert_plan.rs`
- Modify: `crates/trondb-proto/src/lib.rs`

This is the largest single task — converting between all 16 Plan variants and their proto representations, plus all nested TQL types. The approach: implement `From<Plan> for PlanRequest` and `TryFrom<PlanRequest> for Plan` with all sub-conversions as helper functions.

- [ ] **Step 1: Write round-trip tests**

Add to `crates/trondb-proto/src/convert_plan.rs` (at the bottom):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use trondb_core::planner::*;
    use trondb_tql::{FieldList, Literal, WhereClause, VectorLiteral};

    fn round_trip(plan: Plan) -> Plan {
        let proto: pb::PlanRequest = (&plan).into();
        Plan::try_from(proto).unwrap()
    }

    #[test]
    fn round_trip_fetch_all() {
        let plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            limit: Some(10),
            strategy: FetchStrategy::FullScan,
        });
        let restored = round_trip(plan.clone());
        match restored {
            Plan::Fetch(fp) => {
                assert_eq!(fp.collection, "venues");
                assert_eq!(fp.fields, FieldList::All);
                assert_eq!(fp.limit, Some(10));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn round_trip_search_hybrid() {
        let plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::Named(vec!["name".into()]),
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: Some(vec![(1, 0.5), (42, 0.8)]),
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            pre_filter: Some(PreFilter {
                index_name: "idx_city".into(),
                clause: WhereClause::Eq("city".into(), Literal::String("London".into())),
            }),
            k: 5,
            confidence_threshold: 0.8,
            strategy: SearchStrategy::Hybrid,
        });
        let restored = round_trip(plan);
        match restored {
            Plan::Search(sp) => {
                assert_eq!(sp.collection, "venues");
                assert_eq!(sp.k, 5);
                assert_eq!(sp.strategy, SearchStrategy::Hybrid);
                assert!(sp.dense_vector.is_some());
                assert!(sp.sparse_vector.is_some());
                assert!(sp.filter.is_some());
                assert!(sp.pre_filter.is_some());
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn round_trip_insert() {
        let plan = Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["name".into(), "active".into()],
            values: vec![Literal::String("Gym".into()), Literal::Bool(true)],
            vectors: vec![("default".into(), VectorLiteral::Dense(vec![1.0, 2.0]))],
            collocate_with: Some(vec!["v2".into()]),
            affinity_group: Some("group-1".into()),
        });
        let restored = round_trip(plan);
        match restored {
            Plan::Insert(ip) => {
                assert_eq!(ip.collection, "venues");
                assert_eq!(ip.fields.len(), 2);
                assert_eq!(ip.values.len(), 2);
                assert_eq!(ip.vectors.len(), 1);
                assert!(ip.collocate_with.is_some());
                assert_eq!(ip.affinity_group, Some("group-1".into()));
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn round_trip_explain_wraps_inner() {
        let inner = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            limit: None,
            strategy: FetchStrategy::FullScan,
        });
        let plan = Plan::Explain(Box::new(inner));
        let restored = round_trip(plan);
        match restored {
            Plan::Explain(inner) => match *inner {
                Plan::Fetch(fp) => assert_eq!(fp.collection, "venues"),
                _ => panic!("expected Fetch inside Explain"),
            },
            _ => panic!("expected Explain"),
        }
    }

    #[test]
    fn round_trip_update() {
        let plan = Plan::UpdateEntity(UpdateEntityPlan {
            entity_id: "v1".into(),
            collection: "venues".into(),
            assignments: vec![
                ("name".into(), Literal::String("New".into())),
                ("score".into(), Literal::Int(42)),
            ],
        });
        let restored = round_trip(plan);
        match restored {
            Plan::UpdateEntity(up) => {
                assert_eq!(up.entity_id, "v1");
                assert_eq!(up.assignments.len(), 2);
            }
            _ => panic!("expected UpdateEntity"),
        }
    }

    #[test]
    fn round_trip_where_clause_and() {
        let plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::And(
                Box::new(WhereClause::Gte("score".into(), Literal::Int(50))),
                Box::new(WhereClause::Lt("score".into(), Literal::Int(100))),
            )),
            limit: None,
            strategy: FetchStrategy::FieldIndexRange("idx_score".into()),
        });
        let restored = round_trip(plan);
        match restored {
            Plan::Fetch(fp) => {
                assert!(matches!(fp.filter, Some(WhereClause::And(_, _))));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn round_trip_all_simple_plans() {
        // DeleteEntity
        let plan = Plan::DeleteEntity(DeleteEntityPlan {
            entity_id: "v1".into(), collection: "venues".into(),
        });
        assert!(matches!(round_trip(plan), Plan::DeleteEntity(_)));

        // CreateAffinityGroup
        let plan = Plan::CreateAffinityGroup(CreateAffinityGroupPlan {
            name: "g1".into(),
        });
        assert!(matches!(round_trip(plan), Plan::CreateAffinityGroup(_)));

        // Demote
        let plan = Plan::Demote(DemotePlan {
            entity_id: "v1".into(),
            collection: "venues".into(),
            target_tier: trondb_tql::ast::TierTarget::Warm,
        });
        assert!(matches!(round_trip(plan), Plan::Demote(_)));

        // Promote
        let plan = Plan::Promote(PromotePlan {
            entity_id: "v1".into(), collection: "venues".into(),
        });
        assert!(matches!(round_trip(plan), Plan::Promote(_)));

        // ExplainTiers
        let plan = Plan::ExplainTiers(ExplainTiersPlan {
            collection: "venues".into(),
        });
        assert!(matches!(round_trip(plan), Plan::ExplainTiers(_)));
    }
}
```

- [ ] **Step 2: Implement Plan → proto conversions**

Create `crates/trondb-proto/src/convert_plan.rs` with:

**A. Leaf type helpers (implement these first — all other conversions depend on them):**

```rust
fn literal_to_proto(lit: &Literal) -> pb::LiteralValue {
    use pb::literal_value::Value;
    pb::LiteralValue {
        value: Some(match lit {
            Literal::String(s) => Value::StringVal(s.clone()),
            Literal::Int(n) => Value::IntVal(*n),
            Literal::Float(f) => Value::FloatVal(*f),
            Literal::Bool(b) => Value::BoolVal(*b),
            Literal::Null => Value::NullVal(true),
        }),
    }
}

fn proto_to_literal(proto: &pb::LiteralValue) -> Result<Literal, String> {
    use pb::literal_value::Value;
    match &proto.value {
        Some(Value::StringVal(s)) => Ok(Literal::String(s.clone())),
        Some(Value::IntVal(n)) => Ok(Literal::Int(*n)),
        Some(Value::FloatVal(f)) => Ok(Literal::Float(*f)),
        Some(Value::BoolVal(b)) => Ok(Literal::Bool(*b)),
        Some(Value::NullVal(_)) => Ok(Literal::Null),
        None => Err("missing literal value".into()),
    }
}
```

Apply the same pattern for:
- `where_clause_to_proto` / `proto_to_where_clause` — recursive: `WhereClause::And`/`Or` boxes become `pb::BinaryClause` with nested `WhereClauseProto`; comparison variants become `pb::ComparisonClause`
- `field_list_to_proto` / `proto_to_field_list` — `FieldList::All` → `pb::FieldListProto { all: true, names: vec![] }`; `FieldList::Named(v)` → `{ all: false, names: v }`
- `vector_literal_to_proto` / `proto_to_vector_literal` — `VectorLiteral::Dense(v)` → `pb::DenseVector`; `VectorLiteral::Sparse(v)` → `pb::SparseVector`

**B. Strategy + config helpers:**

- `fetch_strategy_to_proto(fs: &FetchStrategy) -> (i32, String)` — returns `(enum value, index_name)`. `FieldIndexLookup(name)` → `(FIELD_INDEX_LOOKUP, name)`; `FieldIndexRange(name)` → `(FIELD_INDEX_RANGE, name)`; `FullScan` → `(FULL_SCAN, "")`
- `proto_to_fetch_strategy(proto_enum: i32, index_name: &str) -> FetchStrategy` — reverse
- `search_strategy_to_proto` / `proto_to_search_strategy` — direct enum mapping
- `repr_decl_to_proto` / `proto_to_repr_decl`
- `field_decl_to_proto` / `proto_to_field_decl` — `FieldType::EntityRef(coll)` sets `entity_ref_collection = Some(coll)` on `pb::FieldDecl`
- `index_decl_to_proto` / `proto_to_index_decl`
- `decay_config_to_proto` / `proto_to_decay_config`
- `tier_target_to_proto` / `proto_to_tier_target`

**C. Plan conversion (uses all helpers above):**

```rust
impl From<&Plan> for pb::PlanRequest {
    fn from(plan: &Plan) -> Self {
        use pb::plan_request::Plan as PP;
        pb::PlanRequest {
            plan: Some(match plan {
                Plan::Fetch(fp) => PP::Fetch(pb::FetchPlan {
                    collection: fp.collection.clone(),
                    fields: Some(field_list_to_proto(&fp.fields)),
                    filter: fp.filter.as_ref().map(where_clause_to_proto),
                    limit: fp.limit.map(|l| l as u64),
                    strategy: fetch_strategy_to_proto(&fp.strategy).0,
                    strategy_index_name: fetch_strategy_to_proto(&fp.strategy).1,
                }),
                Plan::Explain(inner) => PP::Explain(pb::ExplainPlan {
                    inner: Some(Box::new(pb::PlanRequest::from(inner.as_ref()))),
                }),
                // ... remaining 14 variants follow the same match-and-construct pattern
            }),
        }
    }
}
```

**D. Reverse conversion:**

```rust
impl TryFrom<pb::PlanRequest> for Plan {
    type Error = String;
    fn try_from(proto: pb::PlanRequest) -> Result<Self, String> {
        use pb::plan_request::Plan as PP;
        match proto.plan.ok_or("missing plan")? {
            PP::Fetch(fp) => Ok(Plan::Fetch(FetchPlan {
                collection: fp.collection,
                fields: proto_to_field_list(&fp.fields.ok_or("missing fields")?),
                filter: fp.filter.map(|f| proto_to_where_clause(&f)).transpose()?,
                limit: fp.limit.map(|l| l as usize),
                strategy: proto_to_fetch_strategy(fp.strategy, &fp.strategy_index_name),
            })),
            PP::Explain(ep) => {
                let inner = Plan::try_from(*ep.inner.ok_or("missing inner plan")?)?;
                Ok(Plan::Explain(Box::new(inner)))
            }
            // ... remaining 14 variants
        }
    }
}
```

Each of the 16 variants follows this pattern: extract fields from proto, convert nested types using helpers, construct the Rust struct. The tricky cases are:
- **Explain**: recursive — proto `inner` is a boxed `PlanRequest`
- **FetchStrategy**: enum variant carries data (`FieldIndexLookup(String)`) — use `strategy_index_name` field
- **SearchPlan**: `has_dense`/`has_sparse` booleans disambiguate empty vec from `None`
- **InsertPlan.collocate_with**: empty vec in proto maps to `None` in Rust

- [ ] **Step 3: Update lib.rs**

```rust
pub mod pb {
    tonic::include_proto!("trondb");
}

pub mod convert_plan;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-proto`
Expected: All round-trip tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-proto/
git commit -m "feat(proto): Plan ↔ proto conversions with round-trip tests"
```

---

### Task 5: Proto Conversions — QueryResult, WalRecord, HealthSignal

**Files:**
- Create: `crates/trondb-proto/src/convert_result.rs`
- Create: `crates/trondb-proto/src/convert_wal.rs`
- Create: `crates/trondb-proto/src/convert_health.rs`
- Modify: `crates/trondb-proto/src/lib.rs`

- [ ] **Step 1: Write tests for QueryResult conversion**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;
    use trondb_core::result::*;
    use trondb_core::types::Value;

    #[test]
    fn query_result_round_trip() {
        let result = QueryResult {
            columns: vec!["name".into(), "score".into()],
            rows: vec![
                Row {
                    values: HashMap::from([
                        ("name".into(), Value::String("Alice".into())),
                        ("score".into(), Value::Int(42)),
                    ]),
                    score: Some(0.95),
                },
            ],
            stats: QueryStats {
                elapsed: Duration::from_millis(150),
                entities_scanned: 1000,
                mode: QueryMode::Probabilistic,
                tier: "hot".into(),
            },
        };
        let proto: pb::QueryResponse = (&result).into();
        let restored = QueryResult::try_from(proto).unwrap();
        assert_eq!(restored.columns, result.columns);
        assert_eq!(restored.rows.len(), 1);
        assert_eq!(restored.rows[0].score, Some(0.95));
        assert_eq!(restored.stats.entities_scanned, 1000);
        assert_eq!(restored.stats.mode, QueryMode::Probabilistic);
    }
}
```

- [ ] **Step 2: Implement QueryResult conversions**

`convert_result.rs`:
- `impl From<&QueryResult> for pb::QueryResponse`
- `impl TryFrom<pb::QueryResponse> for QueryResult`
- Value ↔ ValueMessage helpers (same shape as LiteralValue but for `trondb_core::types::Value`)

- [ ] **Step 3: Write tests for WalRecord conversion**

```rust
#[test]
fn wal_record_round_trip() {
    let record = WalRecord {
        lsn: 42,
        ts: 1741612800000,
        tx_id: 100,
        record_type: RecordType::EntityWrite,
        schema_ver: 1,
        collection: "venues".into(),
        payload: vec![1, 2, 3, 4],
    };
    let proto: pb::WalRecordMessage = (&record).into();
    let restored = WalRecord::try_from(proto).unwrap();
    assert_eq!(restored.lsn, 42);
    assert_eq!(restored.ts, 1741612800000);
    assert_eq!(restored.record_type, RecordType::EntityWrite);
    assert_eq!(restored.payload, vec![1, 2, 3, 4]);
}
```

- [ ] **Step 4: Implement WalRecord conversions**

`convert_wal.rs`:
- `impl From<&WalRecord> for pb::WalRecordMessage` — map `ts` to `timestamp`, `RecordType` to `RecordTypeProto`
- `impl TryFrom<pb::WalRecordMessage> for WalRecord` — reverse

- [ ] **Step 5: Write tests for HealthSignal conversion**

```rust
#[test]
fn health_signal_round_trip() {
    let signal = HealthSignal {
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
    };
    let proto: pb::HealthSignalResponse = (&signal).into();
    let restored = HealthSignal::try_from(proto).unwrap();
    assert_eq!(restored.node_id, signal.node_id);
    assert_eq!(restored.node_role, NodeRole::Primary);
    assert_eq!(restored.replica_lag_ms, Some(150));
}
```

- [ ] **Step 6: Implement HealthSignal conversions**

`convert_health.rs`:
- `impl From<&HealthSignal> for pb::HealthSignalResponse` — map NodeRole → NodeRoleProto, NodeStatus → NodeStatusProto
- `impl TryFrom<pb::HealthSignalResponse> for HealthSignal` — reverse

- [ ] **Step 7: Update lib.rs**

```rust
pub mod pb {
    tonic::include_proto!("trondb");
}

pub mod convert_plan;
pub mod convert_result;
pub mod convert_wal;
pub mod convert_health;
```

- [ ] **Step 8: Run all tests**

Run: `cargo test -p trondb-proto`
Expected: All pass

- [ ] **Step 9: Commit**

```bash
git add crates/trondb-proto/
git commit -m "feat(proto): QueryResult, WalRecord, HealthSignal ↔ proto conversions"
```

---

### Task 6: Cluster Configuration

**Files:**
- Create: `crates/trondb-server/src/config.rs`
- Modify: `crates/trondb-core/src/lib.rs:32-36` (EngineConfig)

- [ ] **Step 1: Write config parsing tests**

In `crates/trondb-server/src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_toml() {
        let toml = r#"
[cluster]
node_id = "node-1"
role = "primary"
bind_addr = "0.0.0.0:9400"
data_dir = "/data/trondb"
"#;
        let config = ClusterConfig::from_toml(toml).unwrap();
        assert_eq!(config.node_id, "node-1");
        assert_eq!(config.role, NodeRoleConfig::Primary);
        assert_eq!(config.bind_addr, "0.0.0.0:9400");
        assert!(config.peers.is_empty());
    }

    #[test]
    fn parse_full_toml_with_peers() {
        let toml = r#"
[cluster]
node_id = "primary"
role = "primary"
bind_addr = "0.0.0.0:9400"
data_dir = "/data/trondb"

[cluster.replication]
min_ack_replicas = 1
ack_timeout_ms = 5000
require_replica = false

[cluster.snapshots]
snapshot_interval_secs = 300
hnsw_snapshot_interval_secs = 300

[[cluster.peers]]
node_id = "replica-1"
role = "replica"
addr = "192.168.1.11:9400"

[[cluster.peers]]
node_id = "router-1"
role = "router"
addr = "192.168.1.12:9400"
"#;
        let config = ClusterConfig::from_toml(toml).unwrap();
        assert_eq!(config.peers.len(), 2);
        assert_eq!(config.replication.min_ack_replicas, 1);
        assert_eq!(config.snapshots.snapshot_interval_secs, 300);
    }

    #[test]
    fn env_vars_override_toml() {
        let toml = r#"
[cluster]
node_id = "from-toml"
role = "primary"
bind_addr = "0.0.0.0:9400"
data_dir = "/data/trondb"
"#;
        let mut config = ClusterConfig::from_toml(toml).unwrap();
        // Simulate env var
        config.apply_env_override("TRONDB_NODE_ID", "from-env");
        assert_eq!(config.node_id, "from-env");
    }

    #[test]
    fn parse_peers_env_var() {
        let peers_str = "node-2:replica:192.168.1.11:9400,router-1:router:192.168.1.12:9400";
        let peers = PeerConfig::parse_env(peers_str).unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].node_id, "node-2");
        assert_eq!(peers[0].role, NodeRoleConfig::Replica);
        assert_eq!(peers[0].addr, "192.168.1.11:9400");
        assert_eq!(peers[1].node_id, "router-1");
        assert_eq!(peers[1].role, NodeRoleConfig::Router);
        assert_eq!(peers[1].addr, "192.168.1.12:9400");
    }
}
```

- [ ] **Step 2: Implement ClusterConfig**

```rust
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeRoleConfig {
    Primary,
    Replica,
    Router,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PeerConfig {
    pub node_id: String,
    pub role: NodeRoleConfig,
    pub addr: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReplicationConfig {
    #[serde(default = "default_min_ack")]
    pub min_ack_replicas: u32,
    #[serde(default = "default_ack_timeout")]
    pub ack_timeout_ms: u64,
    #[serde(default)]
    pub require_replica: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotConfig {
    #[serde(default = "default_snapshot_interval")]
    pub snapshot_interval_secs: u64,
    #[serde(default = "default_snapshot_interval")]
    pub hnsw_snapshot_interval_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClusterConfig {
    pub node_id: String,
    pub role: NodeRoleConfig,
    pub bind_addr: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default)]
    pub replication: ReplicationConfig,
    #[serde(default)]
    pub snapshots: SnapshotConfig,
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
}
```

With methods:
- `from_toml(s: &str) -> Result<Self, ConfigError>`
- `from_file(path: &Path) -> Result<Self, ConfigError>`
- `from_env() -> Result<Self, ConfigError>` — construct entirely from `TRONDB_*` env vars (used when no config file provided)
- `apply_env_overrides(&mut self)` — reads all `TRONDB_*` env vars, overriding existing values
- `apply_env_override(&mut self, key: &str, value: &str)` — apply single override (test helper)
- `PeerConfig::parse_env(s: &str) -> Result<Vec<PeerConfig>, ConfigError>` — parse `TRONDB_PEERS` format
- `to_engine_config(&self) -> EngineConfig` — convert to trondb-core config

- [ ] **Step 3: Add hnsw_snapshot_interval_secs to EngineConfig**

In `crates/trondb-core/src/lib.rs`, add to `EngineConfig`:

```rust
pub struct EngineConfig {
    pub data_dir: PathBuf,
    pub wal: WalConfig,
    pub snapshot_interval_secs: u64,
    pub hnsw_snapshot_interval_secs: u64,  // NEW
}
```

Update all call sites that construct `EngineConfig` (lib.rs defaults, cli main.rs) to provide the new field with a default of 300.

- [ ] **Step 4: Run all tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-server/src/config.rs crates/trondb-core/src/lib.rs crates/trondb-cli/src/main.rs
git commit -m "feat(server): cluster config TOML + env var overrides + EngineConfig extension"
```

---

## Chunk 2: Server & RPCs

### Task 7: TronNodeService — Execute + HealthSnapshot RPCs

**Files:**
- Create: `crates/trondb-server/src/service.rs`
- Modify: `crates/trondb-core/src/error.rs` (add `Internal` variant)

**Prerequisites:** Add `EngineError::Internal(String)` variant to `crates/trondb-core/src/error.rs`:
```rust
#[error("internal error: {0}")]
Internal(String),
```
This is needed for gRPC error mapping throughout Chunk 2. Also add `tokio-stream = "0.1"` to the workspace deps and to `trondb-server/Cargo.toml`.

- [ ] **Step 1: Write test for Execute RPC**

In `crates/trondb-server/src/service.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use trondb_core::planner::*;
    use trondb_proto::pb;

    /// Create a temporary EngineConfig for testing.
    fn test_config() -> trondb_core::EngineConfig {
        let dir = tempfile::tempdir().unwrap().into_path();
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
    async fn execute_create_collection_via_grpc() {
        // Spin up in-process service
        let (engine, _) = Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);
        let service = TronNodeService::new(engine.clone(), NodeRoleConfig::Primary);

        let plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "test_coll".into(),
            representations: vec![],
            fields: vec![],
            indexes: vec![],
        });
        let request = tonic::Request::new((&plan).into());
        let response = service.execute(request).await.unwrap();
        let result = QueryResult::try_from(response.into_inner()).unwrap();
        assert_eq!(result.rows.len(), 1); // status row
    }
}
```

- [ ] **Step 2: Implement TronNodeService**

```rust
use std::sync::Arc;
use tonic::{Request, Response, Status};
use trondb_core::Engine;
use trondb_core::planner::Plan;
use trondb_core::result::QueryResult;
use trondb_proto::pb;
use trondb_proto::pb::tron_node_server::TronNode;
use crate::config::NodeRoleConfig;

pub struct TronNodeService {
    engine: Arc<Engine>,
    role: NodeRoleConfig,
    primary_channel: Option<tonic::transport::Channel>,  // for write forwarding
}

impl TronNodeService {
    pub fn new(engine: Arc<Engine>, role: NodeRoleConfig) -> Self {
        Self { engine, role, primary_channel: None }
    }

    pub fn with_primary(mut self, channel: tonic::transport::Channel) -> Self {
        self.primary_channel = Some(channel);
        self
    }

    fn is_write_plan(plan: &Plan) -> bool {
        matches!(plan,
            Plan::Insert(_) | Plan::UpdateEntity(_) | Plan::DeleteEntity(_) |
            Plan::CreateCollection(_) | Plan::CreateEdgeType(_) | Plan::InsertEdge(_) |
            Plan::DeleteEdge(_) | Plan::CreateAffinityGroup(_) |
            Plan::AlterEntityDropAffinity(_) | Plan::Demote(_) | Plan::Promote(_)
        )
    }
}

#[tonic::async_trait]
impl TronNode for TronNodeService {
    async fn execute(&self, request: Request<pb::PlanRequest>) -> Result<Response<pb::QueryResponse>, Status> {
        let proto_plan = request.into_inner();
        let plan = Plan::try_from(proto_plan)
            .map_err(|e| Status::invalid_argument(e))?;

        // Write forwarding: non-primary nodes forward writes to primary
        if self.role != NodeRoleConfig::Primary && Self::is_write_plan(&plan) {
            return self.forward_to_primary(&plan).await;
        }

        let result = self.engine.execute(&plan).await
            .map_err(|e| Status::internal(e.to_string()))?;
        let proto_result: pb::QueryResponse = (&result).into();
        Ok(Response::new(proto_result))
    }

    async fn health_snapshot(&self, _request: Request<pb::Empty>) -> Result<Response<pb::HealthSignalResponse>, Status> {
        let signal = self.compute_local_health();
        let proto: pb::HealthSignalResponse = (&signal).into();
        Ok(Response::new(proto))
    }

    // StreamHealth, StreamWal, StreamLocationUpdates — stub implementations
    // (replaced by real impls in Tasks 8, 12, 15)
}

impl TronNodeService {
    /// Compute a HealthSignal from the local engine state.
    /// Uses estimated values until Task 17 (real metrics) replaces them.
    fn compute_local_health(&self) -> HealthSignal {
        let entity_count = self.engine.entity_count() as u64;
        HealthSignal {
            node_id: NodeId::from_string("local"),
            node_role: match self.role {
                NodeRoleConfig::Primary => NodeRole::Primary,
                NodeRoleConfig::Replica => NodeRole::ReadReplica,
                NodeRoleConfig::Router => NodeRole::Router,
            },
            signal_ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default().as_millis() as i64,
            sequence: 0,
            cpu_utilisation: 0.0,  // placeholder until Task 17
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

    /// Forward a write plan to the primary. Returns Err if no primary channel configured.
    async fn forward_to_primary(&self, plan: &Plan) -> Result<Response<pb::QueryResponse>, Status> {
        let channel = self.primary_channel.as_ref()
            .ok_or_else(|| Status::unavailable("no primary configured for write forwarding"))?;
        let mut client = pb::tron_node_client::TronNodeClient::new(channel.clone());
        let proto_plan: pb::PlanRequest = plan.into();
        client.execute(tonic::Request::new(proto_plan)).await
    }
}
```

- [ ] **Step 3: Add stub streaming RPCs** (to be implemented in later tasks)

```rust
type StreamHealthStream = tokio_stream::wrappers::ReceiverStream<Result<pb::HealthSignalResponse, Status>>;
type StreamWalStream = tokio_stream::wrappers::ReceiverStream<Result<pb::WalRecordMessage, Status>>;
type StreamLocationUpdatesStream = tokio_stream::wrappers::ReceiverStream<Result<pb::LocationUpdateMessage, Status>>;

async fn stream_health(&self, _: Request<pb::Empty>) -> Result<Response<Self::StreamHealthStream>, Status> {
    Err(Status::unimplemented("implemented in Task 8"))
}

async fn stream_wal(&self, _: Request<tonic::Streaming<pb::WalAck>>) -> Result<Response<Self::StreamWalStream>, Status> {
    Err(Status::unimplemented("implemented in Task 11"))
}

async fn stream_location_updates(&self, _: Request<pb::Empty>) -> Result<Response<Self::StreamLocationUpdatesStream>, Status> {
    Err(Status::unimplemented("implemented in Task 14"))
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-server`
Expected: Execute test passes

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-server/src/service.rs
git commit -m "feat(server): TronNodeService with Execute and HealthSnapshot RPCs"
```

---

### Task 8: StreamHealth Push RPC

**Files:**
- Create: `crates/trondb-server/src/stream_health.rs`
- Modify: `crates/trondb-server/src/service.rs`

- [ ] **Step 1: Write test**

```rust
#[tokio::test]
async fn stream_health_sends_periodic_signals() {
    let (engine, _) = Engine::open(test_config()).await.unwrap();
    let engine = Arc::new(engine);
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let handle = spawn_health_stream(engine.clone(), tx, Duration::from_millis(50));

    let mut stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    use tokio_stream::StreamExt;
    let first = tokio::time::timeout(Duration::from_secs(1), stream.next()).await;
    assert!(first.is_ok());
    handle.abort();
}
```

- [ ] **Step 2: Implement health streaming**

`stream_health.rs` — function `spawn_health_stream()` that periodically computes a `HealthSignal` from the engine and sends it on a channel. The `TronNodeService::stream_health()` method creates a channel, spawns this task, and returns the receiver as a stream.

- [ ] **Step 3: Wire into TronNodeService**

Replace the stub `stream_health()` in `service.rs` with the real implementation.

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-server stream_health`
Expected: Pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-server/src/stream_health.rs crates/trondb-server/src/service.rs
git commit -m "feat(server): StreamHealth push RPC"
```

---

### Task 9: RemoteNode — NodeHandle over gRPC

**Files:**
- Create: `crates/trondb-server/src/remote_node.rs`

- [ ] **Step 1: Write test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // Integration test: start a real TronNodeService on localhost,
    // connect a RemoteNode, and execute a plan through it.
    #[tokio::test]
    async fn remote_node_execute_via_loopback() {
        // Start in-process gRPC server
        let (engine, _) = Engine::open(test_config()).await.unwrap();
        let engine = Arc::new(engine);
        let service = TronNodeService::new(engine.clone(), NodeRoleConfig::Primary);

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
        ).await.unwrap();

        // Execute CREATE COLLECTION through RemoteNode
        let plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "remote_test".into(),
            representations: vec![],
            fields: vec![],
            indexes: vec![],
        });
        let result = remote.execute(&plan).await.unwrap();
        assert_eq!(result.rows.len(), 1);

        server_handle.abort();
    }
}
```

- [ ] **Step 2: Implement RemoteNode**

```rust
use std::sync::Arc;
use async_trait::async_trait;
use tonic::transport::Channel;
use trondb_core::planner::Plan;
use trondb_core::result::QueryResult;
use trondb_core::error::EngineError;
use trondb_routing::node::{NodeHandle, NodeId, NodeRole};
use trondb_routing::health::HealthSignal;
use trondb_proto::pb;
use trondb_proto::pb::tron_node_client::TronNodeClient;

pub struct RemoteNode {
    node_id: NodeId,
    role: NodeRole,
    client: TronNodeClient<Channel>,
}

impl RemoteNode {
    pub async fn connect(node_id: NodeId, role: NodeRole, addr: String) -> Result<Self, tonic::transport::Error> {
        let channel = Channel::from_shared(addr)?.connect().await?;
        Ok(Self {
            node_id,
            role,
            client: TronNodeClient::new(channel),
        })
    }
}

#[async_trait]
impl NodeHandle for RemoteNode {
    fn node_id(&self) -> &NodeId { &self.node_id }
    fn node_role(&self) -> NodeRole { self.role }

    async fn execute(&self, plan: &Plan) -> Result<QueryResult, EngineError> {
        let proto_plan: pb::PlanRequest = plan.into();
        let mut client = self.client.clone();
        let response = client.execute(tonic::Request::new(proto_plan)).await
            .map_err(|e| EngineError::Internal(e.to_string()))?;
        QueryResult::try_from(response.into_inner())
            .map_err(|e| EngineError::Internal(e))
    }

    async fn health_snapshot(&self) -> HealthSignal {
        let mut client = self.client.clone();
        match client.health_snapshot(tonic::Request::new(pb::Empty {})).await {
            Ok(response) => {
                HealthSignal::try_from(response.into_inner())
                    .unwrap_or_else(|_| HealthSignal::faulted(self.node_id.clone()))
            }
            Err(_) => HealthSignal::faulted(self.node_id.clone()),
        }
    }
}
```

Note: `HealthSignal::faulted()` is a helper that returns a signal with status=Faulted and load_score=1.0. Add this to `trondb-routing/src/health.rs`.

- [ ] **Step 3: Run tests**

Run: `cargo test -p trondb-server remote_node`
Expected: Loopback test passes

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-server/src/remote_node.rs crates/trondb-routing/src/health.rs
git commit -m "feat(server): RemoteNode implements NodeHandle over gRPC"
```

---

### Task 10: Server Main — Role-Based Startup

**Files:**
- Modify: `crates/trondb-server/src/main.rs`

- [ ] **Step 1: Implement main with CLI + config loading**

```rust
use std::sync::Arc;
use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;
use trondb_proto::pb;

mod config;
mod service;
mod stream_health;
mod remote_node;

#[derive(Parser)]
#[command(name = "trondb-server", about = "TronDB distributed node")]
struct Cli {
    /// Path to config TOML file
    #[arg(long, env = "TRONDB_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Structured JSON logging
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    // Load config: file → env overrides
    let mut config = match cli.config {
        Some(path) => config::ClusterConfig::from_file(&path)?,
        None => config::ClusterConfig::from_env()?,
    };
    config.apply_env_overrides();

    tracing::info!(node_id = %config.node_id, role = ?config.role, "starting trondb-server");

    match config.role {
        config::NodeRoleConfig::Primary => start_primary(config).await?,
        config::NodeRoleConfig::Replica => start_replica(config).await?,
        config::NodeRoleConfig::Router => start_router(config).await?,
    }

    Ok(())
}

async fn start_primary(config: config::ClusterConfig) -> Result<(), Box<dyn std::error::Error>> {
    let engine_config = config.to_engine_config();
    let (engine, _pending_records) = trondb_core::Engine::open(engine_config).await?;
    let engine = Arc::new(engine);

    // Process pending WAL records (TierMigration + AffinityGroup)
    // (wired in Task 18)

    let service = service::TronNodeService::new(engine.clone(), config.role);
    let addr = config.bind_addr.parse()?;

    tracing::info!(%addr, "primary listening");
    tonic::transport::Server::builder()
        .add_service(pb::tron_node_server::TronNodeServer::new(service))
        .serve(addr)
        .await?;
    Ok(())
}

async fn start_replica(config: config::ClusterConfig) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("start_replica stub — completed in Task 20");
    let _ = config;
    Ok(())
}

async fn start_router(config: config::ClusterConfig) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("start_router stub — completed in Task 20");
    let _ = config;
    Ok(())
}
```

- [ ] **Step 2: Write config role dispatch test**

In `crates/trondb-server/src/config.rs` tests:

```rust
#[test]
fn role_dispatches_correctly() {
    assert_eq!(NodeRoleConfig::Primary, NodeRoleConfig::Primary);
    assert_ne!(NodeRoleConfig::Primary, NodeRoleConfig::Replica);
    assert_ne!(NodeRoleConfig::Primary, NodeRoleConfig::Router);
}
```

- [ ] **Step 3: Verify it builds**

Run: `cargo build -p trondb-server`
Expected: Compiles

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-server`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-server/src/main.rs crates/trondb-server/src/config.rs crates/trondb-core/src/error.rs
git commit -m "feat(server): role-based startup with config loading and structured logging"
```

---

## Chunk 3: Replication

### Task 11: ReplicaTracker

**Files:**
- Create: `crates/trondb-server/src/replication.rs`

- [ ] **Step 1: Write tests**

```rust
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
```

- [ ] **Step 2: Implement ReplicaTracker**

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use dashmap::DashMap;
use tokio::sync::{mpsc, Notify};

#[derive(Clone)]
pub struct ReplicaTracker {
    replicas: Arc<DashMap<String, ReplicaState>>,
    ack_notify: Arc<Notify>,
}

struct ReplicaState {
    confirmed_lsn: u64,
    connected_at: std::time::Instant,
    sender: mpsc::Sender<pb::WalRecordMessage>,
}

impl ReplicaTracker {
    pub fn new() -> Self { ... }
    pub fn register(&self, replica_id: String, sender: mpsc::Sender<pb::WalRecordMessage>) { ... }
    pub fn disconnect(&self, replica_id: &str) { ... }
    pub fn update_confirmed_lsn(&self, replica_id: &str, lsn: u64) { ... }
    pub fn confirmed_lsn(&self, replica_id: &str) -> Option<u64> { ... }
    pub fn connected_count(&self) -> usize { ... }

    /// Wait until at least `min_replicas` have confirmed `lsn`, or timeout.
    pub async fn wait_for_ack(&self, lsn: u64, min_replicas: u32, timeout: Duration)
        -> Result<(), ReplicationError> { ... }

    /// Broadcast a WAL record to all connected replicas.
    pub async fn broadcast(&self, record: &pb::WalRecordMessage) { ... }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p trondb-server replica_tracker`
Expected: All pass

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-server/src/replication.rs
git commit -m "feat(server): ReplicaTracker — per-replica LSN tracking and ack waiting"
```

---

### Task 12: WAL Streaming — StreamWal Bidirectional

**Files:**
- Modify: `crates/trondb-server/src/replication.rs`
- Modify: `crates/trondb-server/src/service.rs`

- [ ] **Step 1: Write integration test**

```rust
#[tokio::test]
async fn wal_stream_catches_up_and_streams_live() {
    // Setup: primary with some pre-existing entities
    let (engine, _) = Engine::open(test_config()).await.unwrap();
    let engine = Arc::new(engine);

    // Insert some data to generate WAL records
    engine.execute_tql("CREATE COLLECTION test_wal_stream { };").await.unwrap();
    let initial_lsn = engine.wal_head_lsn();

    // Create tracker + service
    let tracker = Arc::new(ReplicaTracker::new());
    let service = TronNodeService::new_with_tracker(engine.clone(), NodeRoleConfig::Primary, tracker.clone());

    // Start server
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(/* ... serve ... */);

    // Connect as replica, send initial ack with LSN=0
    let mut client = TronNodeClient::connect(format!("http://{addr}")).await.unwrap();
    let (tx, rx) = mpsc::channel(16);
    tx.send(pb::WalAck { confirmed_lsn: 0 }).await.unwrap();

    let mut stream = client.stream_wal(ReceiverStream::new(rx)).await.unwrap().into_inner();

    // Should receive catch-up records
    let first = tokio::time::timeout(Duration::from_secs(2), stream.message()).await.unwrap().unwrap().unwrap();
    assert!(first.lsn > 0);

    server_handle.abort();
}
```

- [ ] **Step 2: Implement StreamWal handler**

In `service.rs`, replace the stub `stream_wal()`:

1. Read the initial `WalAck` from the client stream to get `last_confirmed_lsn`
2. Register the replica in `ReplicaTracker`
3. Catch-up: scan WAL for records with `lsn > last_confirmed_lsn`, send each to the stream
4. Live: the `ReplicaTracker::broadcast()` sends live records; the replica's channel is the stream sender
5. Background task reads `WalAck` messages from the client stream and calls `tracker.update_confirmed_lsn()`

- [ ] **Step 3: Run tests**

Run: `cargo test -p trondb-server wal_stream`
Expected: Pass

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-server/src/replication.rs crates/trondb-server/src/service.rs
git commit -m "feat(server): StreamWal bidirectional — catch-up + live WAL streaming"
```

---

### Task 13: Semi-Synchronous Write Path

**Files:**
- Modify: `crates/trondb-server/src/service.rs`

- [ ] **Step 1: Write test**

```rust
#[tokio::test]
async fn semi_sync_write_waits_for_replica_ack() {
    // Setup primary with ReplicaTracker
    // Register a fake "replica" that confirms after 50ms
    // Execute an INSERT plan
    // Verify the INSERT only returns after the replica confirms
    // (check timing: should take >=50ms)
}
```

- [ ] **Step 2: Modify Execute handler for write plans**

When the primary executes a write plan:
1. Check `require_replica`: if `config.require_replica == true` and `tracker.connected_count() == 0`, return an error immediately (`Status::unavailable("no replicas connected and require_replica is true")`)
2. Execute locally (existing logic)
3. Convert the WAL record to proto
4. Broadcast to all replicas via `tracker.broadcast()`
5. If `tracker.connected_count() > 0`: call `tracker.wait_for_ack(lsn, config.min_ack_replicas, config.ack_timeout)`
6. If timeout: log warning but still return success (semi-sync, not strict sync)
7. If no replicas connected and `require_replica == false`: skip ack wait, succeed immediately (single-node mode)
8. Return result to caller

- [ ] **Step 3: Run tests**

Run: `cargo test -p trondb-server semi_sync`
Expected: Pass

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-server/src/service.rs
git commit -m "feat(server): semi-synchronous write path — wait for replica ack"
```

---

### Task 14: Write Forwarding

**Files:**
- Create: `crates/trondb-server/src/write_forward.rs`
- Modify: `crates/trondb-server/src/service.rs`

- [ ] **Step 1: Write test**

```rust
#[tokio::test]
async fn replica_forwards_write_to_primary() {
    // Start primary on port A
    // Start replica service that knows primary is on port A
    // Send INSERT plan to replica
    // Verify replica forwarded it and returned success
    // Verify entity exists on primary
}
```

- [ ] **Step 2: Implement write forwarding**

In `service.rs`, the `execute()` handler already has the check:
```rust
if self.role != NodeRoleConfig::Primary && Self::is_write_plan(&plan) {
    return self.forward_to_primary(&plan).await;
}
```

Implement `forward_to_primary()`:
1. Serialize plan to proto
2. Call `Execute` RPC on the primary channel
3. Return the result

The `primary_channel` is set during startup when the node knows its primary's address.

- [ ] **Step 3: Run tests**

Run: `cargo test -p trondb-server forward`
Expected: Pass

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-server/src/write_forward.rs crates/trondb-server/src/service.rs
git commit -m "feat(server): write forwarding — replica/router proxies writes to primary"
```

---

### Task 15: Location Table Streaming

**Files:**
- Create: `crates/trondb-server/src/location_stream.rs`
- Modify: `crates/trondb-server/src/service.rs`

- [ ] **Step 1: Write test**

```rust
#[tokio::test]
async fn location_stream_sends_snapshot_then_deltas() {
    // Start primary, insert some data
    // Connect to StreamLocationUpdates
    // First message should be a snapshot (is_snapshot=true)
    // Insert more data, next messages should be deltas
}
```

- [ ] **Step 2: Implement StreamLocationUpdates handler**

1. On connect: call `engine.location().snapshot(engine.wal_head_lsn())`, wrap in `LocationUpdateMessage { is_snapshot: true, payload, lsn }`
2. Subscribe to a broadcast channel (new `tokio::sync::broadcast` on the service) that emits `LocationUpdate` WAL records
3. Forward each delta as `LocationUpdateMessage { is_snapshot: false, payload: wal_record_bytes, lsn }`

The service needs a `broadcast::Sender<pb::LocationUpdateMessage>` that the write path feeds into whenever a `LocationUpdate` WAL record is written.

- [ ] **Step 3: Run tests**

Run: `cargo test -p trondb-server location_stream`
Expected: Pass

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-server/src/location_stream.rs crates/trondb-server/src/service.rs
git commit -m "feat(server): StreamLocationUpdates — snapshot + delta to routers"
```

---

## Chunk 4: Routing & Metrics

### Task 16: Scatter-Gather SEARCH

**Files:**
- Create: `crates/trondb-server/src/scatter.rs`
- Modify: `crates/trondb-routing/src/router.rs:407-444`

- [ ] **Step 1: Write tests for merge logic**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use trondb_core::result::*;
    use trondb_core::types::Value;

    #[test]
    fn merge_dense_results_sorts_by_score() {
        let r1 = QueryResult {
            columns: vec!["name".into()],
            rows: vec![
                Row { values: [("name".into(), Value::String("A".into()))].into(), score: Some(0.9) },
                Row { values: [("name".into(), Value::String("B".into()))].into(), score: Some(0.7) },
            ],
            stats: QueryStats { elapsed: Duration::ZERO, entities_scanned: 100, mode: QueryMode::Probabilistic, tier: "hot".into() },
        };
        let r2 = QueryResult {
            columns: vec!["name".into()],
            rows: vec![
                Row { values: [("name".into(), Value::String("C".into()))].into(), score: Some(0.95) },
                Row { values: [("name".into(), Value::String("D".into()))].into(), score: Some(0.6) },
            ],
            stats: QueryStats { elapsed: Duration::ZERO, entities_scanned: 200, mode: QueryMode::Probabilistic, tier: "hot".into() },
        };

        let merged = merge_dense_results(vec![r1, r2], 3);
        assert_eq!(merged.rows.len(), 3);
        assert_eq!(merged.rows[0].score, Some(0.95)); // C
        assert_eq!(merged.rows[1].score, Some(0.9));  // A
        assert_eq!(merged.rows[2].score, Some(0.7));  // B
        assert_eq!(merged.stats.entities_scanned, 300);
    }

    #[test]
    fn merge_hybrid_results_uses_rrf() {
        // Node 1 returns [X, Y] (rank 1, 2)
        // Node 2 returns [Y, Z] (rank 1, 2)
        // Node 1 ranks: X=rank1, Y=rank2
        // Node 2 ranks: Y=rank1, Z=rank2
        // RRF(X) = 1/(60+1)              = 1/61 ≈ 0.0164
        // RRF(Y) = 1/(60+2) + 1/(60+1)   = 1/62 + 1/61 ≈ 0.0325  (Y in both sets)
        // RRF(Z) =            1/(60+2)    = 1/62 ≈ 0.0161
        // Expected order: Y, X, Z
        let r1 = QueryResult {
            columns: vec!["name".into()],
            rows: vec![
                Row { values: [("name".into(), Value::String("X".into()))].into(), score: Some(0.9) },
                Row { values: [("name".into(), Value::String("Y".into()))].into(), score: Some(0.8) },
            ],
            stats: QueryStats { elapsed: Duration::ZERO, entities_scanned: 50, mode: QueryMode::Probabilistic, tier: "hot".into() },
        };
        let r2 = QueryResult {
            columns: vec!["name".into()],
            rows: vec![
                Row { values: [("name".into(), Value::String("Y".into()))].into(), score: Some(0.95) },
                Row { values: [("name".into(), Value::String("Z".into()))].into(), score: Some(0.7) },
            ],
            stats: QueryStats { elapsed: Duration::ZERO, entities_scanned: 60, mode: QueryMode::Probabilistic, tier: "hot".into() },
        };

        let merged = merge_hybrid_results(vec![r1, r2], 3);
        assert_eq!(merged.rows.len(), 3);
        // Y should be first (appears in both sets)
        let names: Vec<&str> = merged.rows.iter()
            .map(|r| match r.values.get("name").unwrap() {
                Value::String(s) => s.as_str(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(names, vec!["Y", "X", "Z"]);
        assert_eq!(merged.stats.entities_scanned, 110);
    }
}
```

- [ ] **Step 2: Implement merge functions**

```rust
pub fn merge_dense_results(results: Vec<QueryResult>, limit: usize) -> QueryResult { ... }
pub fn merge_hybrid_results(results: Vec<QueryResult>, limit: usize) -> QueryResult { ... }
```

- Dense: collect all rows, sort by score descending, take top `limit`, sum entities_scanned. Apply affinity-aware boost: results from nodes with higher affinity density for the query collection get a score multiplier (max 5% boost, i.e. `score *= 1.0 + 0.05 * affinity_density` where `affinity_density` is 0.0–1.0 based on fraction of query collection entities on that node that belong to affinity groups).
- Hybrid: apply RRF with k=60 across node results. For each result set, assign rank 1..n. RRF score = sum over sets of 1/(k + rank). Sort by RRF score descending. Same affinity-aware boost applies to RRF scores before final sort.

> **Note:** The `affinity_density` for a node can be obtained from the HealthSignal or computed from the Location Table. For Phase 9, a simple approach: pass an optional `affinity_boost: Option<HashMap<NodeId, f64>>` to the merge functions. The scatter-gather caller computes this from the Location Table before fanning out. If `None`, no boost is applied (backwards compatible).

- [ ] **Step 3: Modify SemanticRouter to use ScatterGather conditionally**

In `crates/trondb-routing/src/router.rs`, change the `route()` method:

Currently: `QueryVerb::Search => RoutingStrategy::ScatterGather`

Change to: ScatterGather only when the collection spans multiple nodes. For Phase 9, this is determined by checking if more than one node reports having entities for the target collection. For now, use LocationAware as default (single-node), and ScatterGather when explicitly triggered by the server's scatter-gather logic.

- [ ] **Step 4: Implement scatter-gather execution in trondb-server**

`scatter.rs` — function `scatter_gather_search()`:
1. Check Location Table for which nodes hold entities in the target collection
2. If single node: direct dispatch (call `Execute` RPC on that node)
3. If multiple nodes: fan-out the SearchPlan to all nodes via `Execute` RPC in parallel
4. Collect results, call `merge_dense_results()` or `merge_hybrid_results()` based on strategy
5. Return merged result

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-server/src/scatter.rs crates/trondb-routing/src/router.rs
git commit -m "feat(server): scatter-gather SEARCH with dense/hybrid merge"
```

---

### Task 17: Real CPU/RAM Metrics

**Files:**
- Create: `crates/trondb-server/src/metrics.rs`
- Modify: `crates/trondb-routing/src/node.rs:142-166` (LocalNode health_snapshot)
- Modify: `crates/trondb-core/src/lib.rs` (query counter)

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_metrics_returns_valid_ranges() {
        let metrics = SystemMetrics::new();
        let cpu = metrics.cpu_utilisation();
        let ram = metrics.ram_pressure();
        assert!(cpu >= 0.0 && cpu <= 1.0);
        assert!(ram >= 0.0 && ram <= 1.0);
    }

    #[test]
    fn rolling_percentile_tracks_values() {
        let mut rp = RollingPercentile::new(100);
        for i in 0..100 {
            rp.record(i as f32);
        }
        let p99 = rp.percentile(99);
        assert!(p99 >= 98.0);
        let p50 = rp.percentile(50);
        assert!(p50 >= 49.0 && p50 <= 51.0);
    }

    #[test]
    fn query_counter_increments_and_decrements() {
        let counter = QueryCounter::new();
        let guard = counter.enter();
        assert_eq!(counter.current(), 1);
        drop(guard);
        assert_eq!(counter.current(), 0);
    }
}
```

- [ ] **Step 2: Implement SystemMetrics**

```rust
use sysinfo::System;

pub struct SystemMetrics {
    system: std::sync::Mutex<System>,
}

impl SystemMetrics {
    pub fn new() -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_all();
        sys.refresh_memory();
        Self { system: std::sync::Mutex::new(sys) }
    }

    pub fn cpu_utilisation(&self) -> f32 {
        let mut sys = self.system.lock().unwrap();
        sys.refresh_cpu_all();
        sys.global_cpu_usage() / 100.0
    }

    pub fn ram_pressure(&self) -> f32 {
        let mut sys = self.system.lock().unwrap();
        sys.refresh_memory();
        sys.used_memory() as f32 / sys.total_memory() as f32
    }
}
```

- [ ] **Step 3: Implement RollingPercentile**

Ring buffer of recent latencies with a `percentile(p)` method (sort the buffer, return element at index `p/100 * len`).

- [ ] **Step 4: Implement QueryCounter**

Atomic u32 counter. `enter()` returns a guard that increments on creation and decrements on drop.

- [ ] **Step 5: Wire metrics into health_snapshot**

Modify `LocalNode::health_snapshot()` (or create a new `MetricsAwareLocalNode`) to use `SystemMetrics` for cpu/ram and `QueryCounter` for queue_depth. The `RollingPercentile` is fed by the SEARCH executor recording latencies.

Add an `Arc<AtomicU32>` query counter to `Engine` or create a shared state that the executor and health snapshot both reference.

- [ ] **Step 6: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 7: Commit**

```bash
git add crates/trondb-server/src/metrics.rs crates/trondb-routing/src/node.rs crates/trondb-core/src/lib.rs
git commit -m "feat(server): real CPU/RAM metrics via sysinfo + query counter + rolling percentile"
```

---

### Task 18: TierMigration Startup Wiring

**Files:**
- Create: `crates/trondb-routing/src/startup.rs`
- Modify: `crates/trondb-routing/src/lib.rs`
- Modify: `crates/trondb-server/src/main.rs`

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn process_pending_affinity_records() {
        let (engine, pending) = Engine::open(test_config()).await.unwrap();
        let affinity = AffinityIndex::new();
        // Create WAL records for AffinityGroupCreate + AffinityGroupMember
        // Call process_pending_wal_records
        // Verify affinity index has the group and member
    }

    #[tokio::test]
    async fn process_pending_tier_migration_already_complete() {
        // Create a scenario where tier migration record exists
        // but target tier already has the entity (AlreadyComplete case)
        // Verify no error and location table is updated
    }
}
```

- [ ] **Step 2: Implement process_pending_wal_records**

```rust
use trondb_core::Engine;
use trondb_wal::record::{RecordType, WalRecord};
use crate::affinity::AffinityIndex;
use crate::migrator::{TierMigrator, TierMigrationPayload, MigrationRecoveryAction};

pub async fn process_pending_wal_records(
    engine: &Engine,
    records: &[WalRecord],
    affinity: &AffinityIndex,
) -> Result<(), trondb_core::error::EngineError> {
    let max_group_size = 1000; // default

    for record in records {
        match record.record_type {
            RecordType::AffinityGroupCreate
            | RecordType::AffinityGroupMember
            | RecordType::AffinityGroupRemove => {
                affinity.replay_affinity_record(
                    record.record_type,
                    &record.payload,
                    max_group_size,
                );
            }
            RecordType::TierMigration => {
                let payload: TierMigrationPayload = rmp_serde::from_slice(&record.payload)
                    .map_err(|e| EngineError::Internal(e.to_string()))?;

                let entity_id = trondb_core::types::LogicalId::from_string(&payload.entity_id);
                let source_exists = engine.read_tiered(&payload.collection, &entity_id, payload.from_tier)?.is_some();
                let target_exists = engine.read_tiered(&payload.collection, &entity_id, payload.to_tier)?.is_some();

                match TierMigrator::classify_migration_state(source_exists, target_exists) {
                    MigrationRecoveryAction::NoAction => {}
                    MigrationRecoveryAction::AlreadyComplete => {
                        // Entity already in target tier — just update location table
                        // LocationTable::update_tier takes (key, new_tier)
                        for repr_index in 0..1 { // Phase 8 constraint: repr_index 0 only
                            let key = trondb_core::location::ReprKey::new(entity_id.clone(), repr_index);
                            engine.location_table().update_tier(&key, payload.to_tier);
                        }
                    }
                    MigrationRecoveryAction::CleanupSource => {
                        engine.delete_from_tier(&payload.collection, &entity_id, payload.from_tier)?;
                    }
                    MigrationRecoveryAction::ReExecute => {
                        // Re-run the migration: read from source, write to target, delete source
                        if let Some(entity_data) = engine.read_tiered(&payload.collection, &entity_id, payload.from_tier)? {
                            engine.write_tiered(&payload.collection, &entity_id, payload.to_tier, &entity_data)?;
                            engine.delete_from_tier(&payload.collection, &entity_id, payload.from_tier)?;
                            for repr_index in 0..1 {
                                let key = trondb_core::location::ReprKey::new(entity_id.clone(), repr_index);
                                engine.location_table().update_tier(&key, payload.to_tier);
                            }
                        }
                        // If source is gone too, it's a NoAction case — entity was deleted
                    }
                }
            }
            _ => {} // Unknown records — safe to skip
        }
    }
    Ok(())
}
```

- [ ] **Step 3: Export from trondb-routing**

Add `pub mod startup;` to `crates/trondb-routing/src/lib.rs`.

- [ ] **Step 4: Wire into server startup**

In `crates/trondb-server/src/main.rs`, in `start_primary()`:

```rust
let (engine, pending_records) = Engine::open(engine_config).await?;
let engine = Arc::new(engine);
let affinity = AffinityIndex::new();
trondb_routing::startup::process_pending_wal_records(&engine, &pending_records, &affinity).await?;
```

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-routing/src/startup.rs crates/trondb-routing/src/lib.rs crates/trondb-server/src/main.rs
git commit -m "feat(routing): TierMigration startup wiring — process pending WAL records"
```

---

## Chunk 5: Deployment & Testing

### Task 19: Graceful Shutdown + Health Probes

**Files:**
- Modify: `crates/trondb-server/src/main.rs`

> **Note:** The shutdown closure references `background_handles` (a `Vec<JoinHandle<()>>`) — the primary startup function should collect all spawned background task handles (health poller, decay sweeper, tier migrator, HNSW snapshot task) into this vec and pass it into the shutdown closure. Also references `engine.flush_wal()` — this Engine method is added in Task 20's prerequisites.

- [ ] **Step 1: Add SIGTERM handler**

```rust
use tokio::signal;

async fn shutdown_signal() {
    let ctrl_c = signal::ctrl_c();
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate()).unwrap();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT"),
        _ = sigterm.recv() => tracing::info!("received SIGTERM"),
    }
}
```

Wire into server startup:
```rust
tonic::transport::Server::builder()
    .add_service(tonic_health::server::health_reporter().0)  // readiness/liveness
    .add_service(TronNodeServer::new(service))
    .serve_with_shutdown(addr, shutdown_signal())
    .await?;
```

- [ ] **Step 2: Add tonic-health integration**

```rust
let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
// Set serving status after startup completes
health_reporter.set_serving::<TronNodeServer<TronNodeService>>().await;

tonic::transport::Server::builder()
    .add_service(health_service)
    .add_service(TronNodeServer::new(service))
    .serve_with_shutdown(addr, async {
        shutdown_signal().await;
        tracing::info!("graceful shutdown starting");

        // 1. Abort background tasks (health polling, decay sweeper, tier migration)
        for handle in &background_handles {
            handle.abort();
        }
        tracing::info!("background tasks aborted");

        // 2. Flush WAL to ensure all pending writes are durable
        if let Err(e) = engine.flush_wal().await {
            tracing::error!("WAL flush failed during shutdown: {e}");
        }
        tracing::info!("WAL flushed");

        // 3. Save HNSW snapshots
        if let Err(e) = engine.save_hnsw_snapshots() {
            tracing::error!("HNSW snapshot save failed: {e}");
        }
        tracing::info!("HNSW snapshots saved");

        // 4. gRPC streams close automatically when server stops
        tracing::info!("shutdown complete");
    })
    .await?;
```

- [ ] **Step 3: Run build**

Run: `cargo build -p trondb-server`
Expected: Compiles

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-server/src/main.rs
git commit -m "feat(server): graceful SIGTERM shutdown + tonic-health probes"
```

---

### Task 20: Replica + Router Startup

**Files:**
- Modify: `crates/trondb-server/src/main.rs`
- Modify: `crates/trondb-core/src/lib.rs` (add new Engine methods)
- Modify: `crates/trondb-server/src/service.rs` (add `new_router` constructor)

> **Prerequisite Engine methods:** This task references several Engine methods that don't exist yet. Add these to `crates/trondb-core/src/lib.rs`:
> - `pub fn last_applied_lsn(&self) -> u64` — returns the last WAL LSN that has been applied (from the WAL writer's head position)
> - `pub async fn apply_wal_record(&self, record: &WalRecord) -> Result<(), EngineError>` — applies a single WAL record received from the primary (writes to local store + control fabric)
> - `pub async fn flush_wal(&self) -> Result<(), EngineError>` — flushes the WAL writer's buffer to disk (delegates to `WalWriter::flush()`)
> - `pub fn location_table(&self) -> &LocationTable` — exposes the location table (already accessible via executor but add a convenience method)
>
> Also add `TronNodeService::new_router(router: Arc<SemanticRouter>) -> Self` constructor to `service.rs` for router nodes that have no Engine.

- [ ] **Step 1: Implement start_replica**

```rust
async fn start_replica(config: ClusterConfig) -> Result<(), Box<dyn std::error::Error>> {
    let engine_config = config.to_engine_config();
    let (engine, pending_records) = Engine::open(engine_config).await?;
    let engine = Arc::new(engine);

    // Find primary in peers
    let primary_peer = config.peers.iter()
        .find(|p| p.role == NodeRoleConfig::Primary)
        .ok_or("no primary peer configured")?;

    let primary_addr = format!("http://{}", primary_peer.addr);

    // Connect to primary's StreamWal for replication
    let mut client = TronNodeClient::connect(primary_addr.clone()).await?;

    // Start WAL catch-up + live streaming in background task
    let wal_engine = engine.clone();
    let wal_handle = tokio::spawn(async move {
        let last_lsn = wal_engine.last_applied_lsn();
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        // Send initial ack with our last LSN so primary knows where to start
        tx.send(pb::WalAck { confirmed_lsn: last_lsn }).await.unwrap();

        let response = client.stream_wal(tokio_stream::wrappers::ReceiverStream::new(rx))
            .await
            .expect("failed to start WAL stream");
        let mut stream = response.into_inner();

        while let Some(wal_msg) = stream.message().await.expect("WAL stream error") {
            let record = trondb_proto::convert_wal::proto_to_wal_record(wal_msg)
                .expect("invalid WAL record from primary");
            wal_engine.apply_wal_record(&record).await
                .expect("failed to apply WAL record");
            tx.send(pb::WalAck { confirmed_lsn: record.lsn }).await.unwrap();
        }
        tracing::warn!("WAL stream from primary ended");
    });

    // Start gRPC server for serving queries
    let service = TronNodeService::new(engine.clone(), NodeRoleConfig::Replica)
        .with_primary(Channel::from_shared(primary_addr)?.connect().await?);

    let addr = config.bind_addr.parse()?;
    tracing::info!(%addr, "replica listening");

    tonic::transport::Server::builder()
        .add_service(TronNodeServer::new(service))
        .serve_with_shutdown(addr, shutdown_signal())
        .await?;

    wal_handle.abort();
    Ok(())
}
```

- [ ] **Step 2: Implement start_router**

```rust
async fn start_router(config: ClusterConfig) -> Result<(), Box<dyn std::error::Error>> {
    // Router has no Engine — only Location Table + Health Cache

    let primary_peer = config.peers.iter()
        .find(|p| p.role == NodeRoleConfig::Primary)
        .ok_or("no primary peer configured")?;

    let primary_addr = format!("http://{}", primary_peer.addr);

    // Subscribe to Location Table updates from primary
    let mut client = TronNodeClient::connect(primary_addr.clone()).await?;
    let location_table = LocationTable::new();

    let loc_table = location_table.clone();
    let loc_handle = tokio::spawn(async move {
        let response = client.stream_location_updates(pb::LocationSubscribeRequest {})
            .await
            .expect("failed to start location stream");
        let mut stream = response.into_inner();

        while let Some(update) = stream.message().await.expect("location stream error") {
            match update.update_type() {
                pb::LocationUpdateType::Snapshot => {
                    // Full snapshot: clear and rebuild
                    loc_table.clear();
                    for entry in &update.entries {
                        loc_table.insert_from_proto(entry);
                    }
                    tracing::info!(entries = update.entries.len(), "location table snapshot applied");
                }
                pb::LocationUpdateType::Delta => {
                    // Incremental update
                    for entry in &update.entries {
                        loc_table.upsert_from_proto(entry);
                    }
                }
            }
        }
        tracing::warn!("location stream from primary ended");
    });

    // Connect RemoteNodes for all data nodes
    let mut remote_nodes: Vec<Arc<dyn NodeHandle>> = vec![];
    for peer in &config.peers {
        if peer.role != NodeRoleConfig::Router {
            let remote = RemoteNode::connect(
                NodeId::from_string(&peer.node_id),
                format!("http://{}", peer.addr),
            ).await?;
            remote_nodes.push(Arc::new(remote));
        }
    }

    // Create SemanticRouter with RemoteNodes
    let router = Arc::new(SemanticRouter::new(remote_nodes, RouterConfig::default()));

    // Router service wraps SemanticRouter — accepts Execute RPCs, routes via router
    let service = TronNodeService::new_router(router.clone())
        .with_primary(Channel::from_shared(primary_addr)?.connect().await?);

    let addr = config.bind_addr.parse()?;
    tracing::info!(%addr, "router listening");

    tonic::transport::Server::builder()
        .add_service(TronNodeServer::new(service))
        .serve_with_shutdown(addr, shutdown_signal())
        .await?;

    loc_handle.abort();
    Ok(())
}
```

- [ ] **Step 3: Run build**

Run: `cargo build -p trondb-server`
Expected: Compiles

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-server/src/main.rs
git commit -m "feat(server): replica and router startup sequences"
```

---

### Task 21: Dockerfile + Docker Compose

**Files:**
- Create: `Dockerfile`
- Create: `docker-compose.yml`

- [ ] **Step 1: Create Dockerfile**

```dockerfile
# Builder stage
FROM rust:1.85-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

RUN cargo build --release -p trondb-server

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/trondb-server /usr/local/bin/trondb-server

ENV TRONDB_DATA_DIR=/data/trondb
VOLUME /data/trondb

EXPOSE 9400

ENTRYPOINT ["trondb-server"]
```

- [ ] **Step 2: Create docker-compose.yml**

Per the spec's example Docker Compose (3-node cluster):

```yaml
services:
  primary:
    build: .
    environment:
      TRONDB_NODE_ID: primary
      TRONDB_ROLE: primary
      TRONDB_BIND_ADDR: "0.0.0.0:9400"
      TRONDB_PEERS: "replica-1:replica:replica-1:9400"
    volumes:
      - primary-data:/data/trondb
    ports:
      - "9400:9400"

  replica-1:
    build: .
    environment:
      TRONDB_NODE_ID: replica-1
      TRONDB_ROLE: replica
      TRONDB_BIND_ADDR: "0.0.0.0:9400"
      TRONDB_PEERS: "primary:primary:primary:9400"
    volumes:
      - replica-data:/data/trondb

  router-1:
    build: .
    environment:
      TRONDB_NODE_ID: router-1
      TRONDB_ROLE: router
      TRONDB_BIND_ADDR: "0.0.0.0:9400"
      TRONDB_PEERS: "primary:primary:primary:9400,replica-1:replica:replica-1:9400"
    ports:
      - "9401:9400"

volumes:
  primary-data:
  replica-data:
```

- [ ] **Step 3: Verify Docker build**

Run: `docker build -t trondb:test .`
Expected: Image builds successfully

- [ ] **Step 4: Commit**

```bash
git add Dockerfile docker-compose.yml
git commit -m "feat: Dockerfile and Docker Compose for 3-node cluster"
```

---

### Task 22: Integration Tests — Loopback gRPC

**Files:**
- Create: `crates/trondb-server/tests/integration.rs`

- [ ] **Step 1: Write loopback integration test**

Tests exercise the full RemoteNode → gRPC → Engine path:

```rust
use std::sync::Arc;
use std::time::Duration;
use trondb_core::{Engine, EngineConfig};
use trondb_server::service::TronNodeService;
use trondb_server::remote_node::RemoteNode;
use trondb_server::config::NodeRoleConfig;
use trondb_routing::node::{NodeId, NodeHandle};
use trondb_proto::pb;

#[tokio::test]
async fn loopback_create_insert_fetch() {
    let (engine, _) = Engine::open(test_config()).await.unwrap();
    let engine = Arc::new(engine);
    let service = TronNodeService::new(engine.clone(), NodeRoleConfig::Primary);

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

    let remote = RemoteNode::connect(
        NodeId::from_string("test-node"),
        format!("http://{addr}"),
    ).await.unwrap();

    // CREATE COLLECTION
    let create_plan = Plan::CreateCollection(CreateCollectionPlan {
        name: "test_col".into(),
        representations: vec![trondb_core::planner::RepresentationPlan {
            name: "default".into(),
            model: None,
            dimensions: Some(3),
            metric: trondb_tql::Metric::Cosine,
            sparse: false,
        }],
        fields: vec![],
        indexes: vec![],
    });
    let result = remote.execute(&create_plan).await.unwrap();

    // INSERT
    let insert_plan = Plan::Insert(InsertPlan {
        collection: "test_col".into(),
        entity_id: trondb_core::types::LogicalId::from_string("e1"),
        fields: vec![],
        values: vec![],
        vectors: vec![("default".into(), trondb_core::types::VectorData::Dense(vec![1.0, 0.0, 0.0]))],
        collocate_with: None,
        affinity_group: None,
    });
    let result = remote.execute(&insert_plan).await.unwrap();

    // FETCH
    let fetch_plan = Plan::Fetch(FetchPlan {
        collection: "test_col".into(),
        fields: trondb_tql::FieldList::All,
        filter: None,
        limit: Some(10),
        strategy: FetchStrategy::FullScan,
    });
    let result = remote.execute(&fetch_plan).await.unwrap();
    assert!(!result.rows.is_empty());

    server_handle.abort();
}

#[tokio::test]
async fn loopback_health_snapshot() {
    let (engine, _) = Engine::open(test_config()).await.unwrap();
    let engine = Arc::new(engine);
    let service = TronNodeService::new(engine.clone(), NodeRoleConfig::Primary);

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

    let remote = RemoteNode::connect(
        NodeId::from_string("health-test"),
        format!("http://{addr}"),
    ).await.unwrap();

    let signal = remote.health_snapshot().await.unwrap();
    assert!(signal.cpu_utilisation >= 0.0 && signal.cpu_utilisation <= 1.0);
    assert!(signal.ram_pressure >= 0.0 && signal.ram_pressure <= 1.0);

    server_handle.abort();
}
```

- [ ] **Step 2: Run integration tests**

Run: `cargo test -p trondb-server --test integration`
Expected: All pass

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-server/tests/integration.rs
git commit -m "test(server): loopback gRPC integration tests"
```

---

### Task 23: Docker Compose Cluster Test

**Files:**
- Create: `tests/cluster/cluster_test.sh`
- Create: `tests/cluster/test_queries.tql`

> **Note:** This test is gated behind `#[ignore]` (or equivalent shell guard). It requires Docker and is intended for CI or manual validation, not for `cargo test --workspace`.

- [ ] **Step 1: Create test TQL script**

```sql
-- tests/cluster/test_queries.tql
-- Run against router endpoint

CREATE COLLECTION cluster_test {
    REPRESENTATION default DIMENSIONS 3 METRIC cosine;
    FIELD name TEXT;
};

INSERT INTO cluster_test (name) VALUES ('entity1') REPRESENTATION default VECTOR [1.0, 0.0, 0.0];
INSERT INTO cluster_test (name) VALUES ('entity2') REPRESENTATION default VECTOR [0.0, 1.0, 0.0];

FETCH * FROM cluster_test;
-- Expect 2 rows
```

- [ ] **Step 2: Create cluster test script**

```bash
#!/usr/bin/env bash
# tests/cluster/cluster_test.sh
# Spins up a 3-node cluster, runs queries, verifies replication, tears down.
# Requires: docker compose, curl/grpcurl
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

echo "=== Building trondb image ==="
docker build -t trondb:test "$PROJECT_ROOT"

echo "=== Starting 3-node cluster ==="
docker compose -f "$PROJECT_ROOT/docker-compose.yml" up -d

# Wait for health
echo "=== Waiting for nodes to be ready ==="
# Primary is exposed on 9400, router on 9401. Replica has no host port mapping.
for port in 9400 9401; do
    for i in $(seq 1 30); do
        if grpcurl -plaintext "localhost:$port" grpc.health.v1.Health/Check >/dev/null 2>&1; then
            echo "Node on port $port is ready"
            break
        fi
        if [ "$i" -eq 30 ]; then echo "TIMEOUT waiting for port $port"; exit 1; fi
        sleep 1
    done
done

echo "=== Running queries against router (port 9401) ==="
# Use a gRPC client to execute test_queries.tql against the router
# Verify FETCH returns 2 rows

echo "=== Verifying replication via primary (port 9400) ==="
# Query the primary directly — FETCH should also return 2 rows (written through router)

echo "=== Tearing down ==="
docker compose -f "$PROJECT_ROOT/docker-compose.yml" down -v

echo "=== Cluster test passed ==="
```

- [ ] **Step 3: Make script executable**

Run: `chmod +x tests/cluster/cluster_test.sh`

- [ ] **Step 4: Commit**

```bash
git add tests/cluster/
git commit -m "test: Docker Compose 3-node cluster test (ignored, CI-only)"
```

---

### Task 24: Full Workspace Test + CLAUDE.md Update

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All tests pass (existing 319+ tests + all new tests)

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: Zero warnings

- [ ] **Step 3: Update CLAUDE.md**

Add Phase 9 sections:
- New crates: `trondb-proto`, `trondb-server`
- gRPC transport: tonic service definition, RemoteNode
- WAL streaming: semi-synchronous replication, ReplicaTracker
- Cluster config: TOML + env vars, static peers
- Server roles: Primary, Replica, Router startup sequences
- Container deployment: single binary, Docker Compose, tonic-health probes
- Run server: `cargo run -p trondb-server -- --config cluster.toml`

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: update CLAUDE.md for Phase 9 — multi-node distribution"
```
