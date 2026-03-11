# TronDB Phase 9: Multi-Node Distribution

**Goal:** Close the gap between single-process TronDB and the v4 distributed topology. After Phase 9, TronDB operates as a cluster of cooperating nodes with a single write primary, one or more read replicas, and stateless router nodes.

**Scope:** gRPC transport (tonic), WAL streaming replication, Location Table replication, scatter-gather SEARCH, static cluster config, real CPU/RAM metrics, TierMigration startup wiring, container-ready deployment.

**Non-scope:** Multi-primary writes, dynamic cluster membership (join/leave), automatic failover/leader election, cross-datacenter replication.

**Reference documents:**
- `trondb_design_v4.docx` — Architecture & Distribution (two-plane fabric, node topology, Location Table as NVSwitch)
- `trondb_wal_v1.docx` — WAL Format & Streaming Protocol (replication, synchronous ack, crash recovery)
- `trondb_routing_v1.docx` — Routing Intelligence (health signals, co-location, semantic router)
- `trondb_planner_v1.docx` — Query Planner & Cost Model (ACU, optimization, routing-aware planning)
- `trondb_grammar_v1.docx` — TQL Grammar (node-agnostic query syntax)

---

## 1. Node Topology

Three node roles per the v4 design. The existing `NodeRole` enum has `HotNode`, `WarmNode`, `ReadReplica`. Phase 9 adds `Primary` and `Router` variants to this enum:

### Write Primary

Single node accepting all mutations. Writes to local WAL + Fjall, streams WAL records to replicas via gRPC bidirectional stream. Runs its own Engine. Broadcasts Location Table updates to router nodes.

### Read Replica

Receives WAL stream from primary, applies records to local Fjall + rebuilds control fabric (HNSW, SparseIndex, FieldIndex, AdjacencyIndex). Serves FETCH/SEARCH/TRAVERSE queries. Does not accept writes — any write plan received is proxied to the primary.

### Router Node

Stateless. Holds HealthCache + AffinityIndex + Location Table in RAM (replicated from primary). Routes queries to the best node using SemanticRouter. No Engine, no Fjall. Accepts client queries and dispatches to the appropriate data node.

---

## 2. gRPC Service Definition

### New crates

**`crates/trondb-proto/`** — Protobuf definitions + tonic-generated code. Build-time code generation via tonic-build + prost. No business logic, just types and service stubs.

**`crates/trondb-server/`** — gRPC server binary. Wraps Engine in a tonic service. Depends on trondb-core, trondb-routing, trondb-proto. Single binary that acts as primary, replica, or router based on config.

### Service definition

```protobuf
service TronNode {
    // Query execution — router sends Plan, node executes
    rpc Execute(PlanRequest) returns (QueryResponse);

    // Health — router polls or node pushes
    rpc HealthSnapshot(Empty) returns (HealthSignalResponse);
    rpc StreamHealth(Empty) returns (stream HealthSignalResponse);

    // WAL replication — primary streams to replica
    rpc StreamWal(stream WalAck) returns (stream WalRecord);

    // Location Table — primary broadcasts updates
    rpc StreamLocationUpdates(Empty) returns (stream LocationUpdate);
}
```

### Serialisation decisions

- **Plan** — serialised as a protobuf oneof matching ALL existing `Plan` enum variants. The full set (from `planner.rs`): `Fetch`, `Search`, `Traverse`, `CreateCollection`, `Insert`, `CreateEdgeType`, `InsertEdge`, `DeleteEntity`, `DeleteEdge`, `Explain(Box<Plan>)`, `UpdateEntity`, `CreateAffinityGroup`, `AlterEntityDropAffinity`, `Demote`, `Promote`, `ExplainTiers`. Each variant carries its fields as protobuf messages. `Explain` is recursive — the proto message contains a nested `PlanRequest` field. Nested TQL types (`WhereClause`, `VectorLiteral`, `FieldList`, `PreFilter`, `Literal`, `TierTarget`, `DecayConfigDecl`) all need corresponding protobuf messages.
- **QueryResult** — `columns: repeated string` + `rows: repeated Row`. Each `Row` contains `values: map<string, Value>` (matching the existing `HashMap<String, Value>`) + `optional double score`. `Value` is a protobuf oneof (string, int64, double, bool, null).
- **WalRecord** — `record_type` enum, `collection` string, `tx_id` uint64, `schema_ver` uint32, `lsn` uint64, `timestamp` uint64 (maps to the `ts` field in Rust's `WalRecord` struct — the proto field uses the full name for clarity), `payload` bytes (opaque MessagePack — not re-encoded to protobuf, preserving the existing WAL format).
- **HealthSignal** — mirrors the actual `HealthSignal` struct in `health.rs` exactly: `node_id`, `node_role`, `signal_ts`, `sequence`, `cpu_utilisation`, `ram_pressure`, `hot_entity_count`, `hot_tier_capacity`, `warm_entity_count`, `archive_entity_count`, `query_queue_depth`, `queue_capacity`, `hnsw_p50_ms`, `hnsw_p99_ms`, `replica_lag_ms` (optional uint64), `load_score`, `status`.

### RemoteNode

Implements `NodeHandle` by holding a tonic channel to a remote TronNode service:

- `execute(plan)` → `Execute` RPC
- `health_snapshot()` → `HealthSnapshot` RPC (on-demand pull)
- Background task subscribes to `StreamHealth` for push updates, feeding into the HealthCache

Connection management: tonic `Channel` with default reconnect policy. `RemoteNode::new(addr)` establishes the channel; the background health stream reconnects automatically on disconnect.

---

## 3. WAL Streaming Replication

Per the WAL spec (trondb_wal_v1), the write primary streams WAL records to replicas via the `StreamWal` bidirectional gRPC stream.

### Flow

1. Replica connects to primary's `StreamWal` RPC, sending its last confirmed LSN in the initial `WalAck` message
2. Primary sends all WAL records with `lsn > replica_last_lsn` (catch-up phase), then continues streaming live records
3. Replica applies each record to its local Fjall + control fabric (same `replay_wal_records` path used on startup)
4. Replica sends `WalAck { confirmed_lsn }` after each record (or batch) is durably applied
5. Primary tracks each replica's confirmed LSN in a `ReplicaTracker`

### Semi-synchronous write path

1. Client sends write to primary (via Execute RPC or local CLI)
2. Primary appends to local WAL, flushes + fsync, applies to Fjall + control fabric
3. Primary sends WAL record to all connected replica streams
4. Primary waits for **at least one** replica to confirm (`min_ack_replicas`, default 1)
5. Primary acks client
6. Remaining replicas catch up asynchronously — their lag appears in HealthSignal, router deprioritises them

### Failure modes

- **Replica disconnects mid-stream:** Primary logs warning, continues serving. Replica reconnects with its last confirmed LSN, primary replays from that point (catch-up).
- **No replicas connected:** Primary still operates (single-node mode). Writes succeed immediately. Configurable: `require_replica: bool` to block writes if no replica is available (default false).
- **Replica falls behind:** Lag tracked in HealthSignal (`replica_lag = primary_lsn - confirmed_lsn`). Router stops routing queries to it when load_score exceeds shed threshold (0.85).

### ReplicaTracker

New struct in `trondb-server`:

- `HashMap<NodeId, ReplicaState>` where `ReplicaState` holds: `confirmed_lsn`, `connected_at`, stream sender handle
- `wait_for_ack(lsn, min_replicas, timeout) -> Result<(), ReplicationError>` — blocks until enough replicas confirm or timeout expires
- Background cleanup for disconnected replicas (remove from map on stream close)

---

## 4. Location Table Replication

Router nodes need the Location Table to know where entities live. Per v4, the Location Table acts as the "NVSwitch" — any router can answer any query by consulting it.

### Protocol

- Router nodes subscribe to `StreamLocationUpdates` on the primary
- Primary filters its WAL stream to send only `LocationUpdate` records (0x40) to routers — routers don't need EntityWrite, EdgeWrite, etc. since they have no Engine
- On connect, primary sends a full Location Table snapshot (using the existing `location.snapshot(wal_head_lsn)` method — requires current WAL LSN), then streams deltas
- Router applies updates to its local DashMap-backed LocationTable

### Consistency model

Eventually consistent — staleness bounded by network latency (sub-millisecond on LAN). If a router routes to a node that no longer holds an entity (stale location), the data node returns "not found". The router can then:

1. Re-fetch the location entry from the primary (on-demand pull)
2. Retry with the corrected location

This is the same pattern as NVSwitch cache invalidation — rare, bounded, self-correcting.

---

## 5. Scatter-Gather SEARCH

The `RoutingStrategy::ScatterGather` enum variant already exists and is currently assigned unconditionally to all SEARCH queries. Phase 9 changes this: ScatterGather is only used when the collection actually spans multiple nodes. Single-node collections use `LocationAware` (direct dispatch).

### Execution flow

1. Router receives a SEARCH plan
2. Router checks Location Table — if target collection spans multiple nodes, strategy is ScatterGather; otherwise LocationAware (direct dispatch to the single node)
3. Router fans out the SEARCH plan to all nodes holding entities in that collection (via `Execute` RPC)
4. Each node executes locally, returns top-K results with scores
5. Router merges results and applies the original LIMIT

### Merge logic

- **Dense-only SEARCH:** Sort all results by similarity score descending, take top LIMIT
- **Hybrid SEARCH (dense + sparse):** Each node returns its own RRF-merged results with scores. Router does a second RRF merge across nodes (k=60, same as intra-node)
- **Affinity-aware boost:** Results from nodes with higher affinity density for the query get a slight score boost (max 5%, per routing spec)

### Write routing

All writes (INSERT, UPDATE, DELETE, CREATE COLLECTION, CREATE EDGE) are forwarded to the write primary. If a router or replica receives a write plan, it proxies to the primary via the `Execute` RPC, waits for the response, and returns it to the caller. This is transparent — the caller doesn't know which node is primary.

**Loop prevention:** The primary MUST NOT forward write plans it receives via `Execute` RPC. When the primary's `Execute` handler receives a plan, it executes locally regardless of plan type — it never proxies. Only replicas and routers proxy writes. This is enforced by checking `self.role == Primary` in the execute handler — if true, execute locally; if false, check if the plan is a write and proxy to the primary.

---

## 6. Cluster Configuration & Node Discovery

### Static cluster config (TOML)

No dynamic discovery in Phase 9. Nodes configured at startup:

```toml
[cluster]
node_id = "node-1"
role = "primary"            # primary | replica | router
bind_addr = "0.0.0.0:9400"
data_dir = "/data/trondb"

[cluster.replication]
min_ack_replicas = 1        # semi-synchronous: wait for at least N replicas
ack_timeout_ms = 5000       # timeout for replica ack before proceeding
require_replica = false     # if true, block writes when no replicas connected

[cluster.snapshots]
snapshot_interval_secs = 300           # Location Table snapshot interval
hnsw_snapshot_interval_secs = 300      # HNSW index snapshot interval

# These map directly to EngineConfig fields:
#   EngineConfig.snapshot_interval = Duration::from_secs(snapshot_interval_secs)
#   EngineConfig.hnsw_snapshot_interval = Duration::from_secs(hnsw_snapshot_interval_secs)

[[cluster.peers]]
node_id = "node-2"
role = "replica"
addr = "192.168.1.11:9400"

[[cluster.peers]]
node_id = "router-1"
role = "router"
addr = "192.168.1.12:9400"
```

### Environment variable overrides

Every config field is overridable via environment variables (container-friendly):

- `TRONDB_NODE_ID` — node identifier
- `TRONDB_ROLE` — `primary`, `replica`, or `router`
- `TRONDB_BIND_ADDR` — gRPC listen address
- `TRONDB_DATA_DIR` — data directory path
- `TRONDB_PEERS` — comma-separated `node_id:role:addr` entries (e.g. `node-2:replica:192.168.1.11:9400,router-1:router:192.168.1.12:9400`). Note: the `addr` component contains a colon (host:port), so parsing splits on comma first, then splits each entry into exactly 3 parts: `node_id`, `role`, and the remainder as `addr`
- `TRONDB_MIN_ACK_REPLICAS` — replication quorum
- `TRONDB_ACK_TIMEOUT_MS` — replication ack timeout
- `TRONDB_CONFIG` — path to TOML config file

Environment variables take precedence over TOML values.

### Startup sequence by role

| Role | Startup Sequence |
|---|---|
| Primary | Engine::open → process unhandled WAL records (TierMigration wiring + AffinityGroup replay) → start gRPC server → await replica connections → begin WAL streaming + Location Table broadcasting |
| Replica | Engine::open (empty or from prior state) → connect to primary's StreamWal → catch up from last confirmed LSN → rebuild control fabric from replayed records → start gRPC server (for query serving) |
| Router | No Engine → connect to primary's StreamLocationUpdates → load Location Table snapshot → subscribe to StreamHealth from all data nodes → start accepting client queries via SemanticRouter |

---

## 7. Real CPU/RAM Metrics

Currently `LocalNode::health_snapshot()` estimates load from entity count. Phase 9 replaces this with real measurements via the `sysinfo` crate:

- `cpu_utilisation` — `System::global_cpu_usage()` normalised to 0.0–1.0
- `ram_pressure` — `System::used_memory() / System::total_memory()`
- `queue_depth` — atomic counter on the executor (increment on query entry, decrement on completion)
- `hnsw_p99_ms` — rolling percentile tracked via a ring buffer of recent SEARCH latencies
- `replica_lag` — `primary_lsn - confirmed_lsn` from ReplicaTracker (primary) or from the WAL stream position (replica)

These feed into the existing load_score formula unchanged. The weights (RAM 35%, queue 30%, CPU 20%, HNSW 10%, lag 5%) are already correct per the routing spec.

---

## 8. TierMigration Startup Wiring

Gap from Phase 8: the `classify_migration_state()` function exists but is never called during startup.

### Wiring

New function in `trondb-routing`: `pub async fn process_pending_wal_records(engine: &Engine, records: &[WalRecord], affinity: &AffinityIndex) -> Result<(), EngineError>`. Called by `trondb-server` during primary startup, after `Engine::open()` returns `(Engine, Vec<WalRecord>)` and before the gRPC server starts. This function iterates the unhandled records:

1. AffinityGroup records → `AffinityIndex::replay_affinity_record()` (already specified in Phase 8)
2. TierMigration records → deserialise `TierMigrationPayload`, check actual tier state via `Engine::read_tiered()`, call `classify_migration_state(source_exists, target_exists)`, execute recovery action:
   - `ReExecute` → re-run the migration (demote entity from source to target tier)
   - `CleanupSource` → delete from source tier, update location
   - `AlreadyComplete` → update location table to match target tier
   - `NoAction` → entity was subsequently deleted, skip

This runs before the system becomes queryable, maintaining the invariant from Phase 8: no queryable state until all WAL records are processed.

---

## 9. Container & Deployment

### Docker-native design

- **Single binary** — `trondb-server` acts as primary, replica, or router based on config
- **Data dir as volume mount** — `TRONDB_DATA_DIR` defaults to `/data/trondb`. Fjall, WAL, HNSW snapshots, Location Table snapshots all under this path
- **Health endpoint** — tonic-health (gRPC health checking protocol) for container orchestrator liveness/readiness probes. Readiness transitions to serving only after startup sequence completes
- **Graceful shutdown** — SIGTERM handler: abort background tasks, save HNSW snapshots, flush WAL, close gRPC streams, exit
- **Logging** — structured JSON to stdout via `tracing` + `tracing-subscriber` with JSON formatter. Replaces `eprintln!` calls

### Dockerfile

Multi-stage build:
- Builder stage: `cargo build --release -p trondb-server`
- Runtime stage: debian-slim or distroless, copy binary + protobuf assets

### Example Docker Compose (3-node cluster)

```yaml
services:
  primary:
    image: trondb:latest
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
    image: trondb:latest
    environment:
      TRONDB_NODE_ID: replica-1
      TRONDB_ROLE: replica
      TRONDB_BIND_ADDR: "0.0.0.0:9400"
      TRONDB_PEERS: "primary:primary:primary:9400"
    volumes:
      - replica-data:/data/trondb

  router-1:
    image: trondb:latest
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

---

## 10. Testing Strategy

### Three levels

1. **Unit tests** — per-crate, same pattern as existing. Proto serialisation round-trips, ReplicaTracker logic, scatter-gather merge, config parsing, environment variable override precedence.

2. **Integration tests with loopback gRPC** — `SimulatedRemoteNode` wraps a real in-process Engine behind the gRPC service (bind to `127.0.0.1:0` for ephemeral port). Tests exercise the full RemoteNode → gRPC → Engine path without multiple processes.

3. **Docker Compose cluster test** — spin up a 3-node cluster (primary + replica + router), run TQL queries against the router, verify replication by querying the replica directly, then tear down. Lives in `tests/cluster/`, gated behind `#[ignore]` (runs with `--include-ignored` or in CI).

### Backwards compatibility

- `trondb-cli` continues to work unchanged — calls `Engine::open()` directly, single-node, no gRPC
- `Engine` public API unchanged — `trondb-server` wraps it without modification
- Existing tests all pass — Phase 9 adds new crates and modifies `trondb-routing` for TierMigration wiring + scatter-gather execution, but does not change `trondb-core` or `trondb-tql`
- Single-node mode remains the default — if no cluster config is provided, `trondb-server` runs as a standalone primary with no replication

---

## New Crate Dependency Map

```
trondb-tql          trondb-wal          trondb-proto (NEW)
    ↓                   ↓                    ↓
trondb-core ←───────────┘                    │
    ↓                                        │
trondb-routing ←─────────────────────────────┘
    ↓                                        │
trondb-server (NEW) ←───────────────────────┘
    ↓
trondb-cli (unchanged, no gRPC dependency)
```

No circular dependencies. `trondb-core` and `trondb-tql` remain unaware of gRPC. `trondb-cli` does not depend on `trondb-proto` or `trondb-server`.

---

## Architecture Summary

```
Client (TQL) ──► Router Node ──► SemanticRouter
                                      │
                      ┌───────────────┼───────────────┐
                      ▼               ▼               ▼
               Write Primary    Read Replica 1   Read Replica N
               ┌──────────┐    ┌──────────┐    ┌──────────┐
               │ Engine   │    │ Engine   │    │ Engine   │
               │ WAL      │───►│ WAL      │    │ WAL      │
               │ Fjall    │    │ Fjall    │    │ Fjall    │
               │ HNSW     │    │ HNSW     │    │ HNSW     │
               └──────────┘    └──────────┘    └──────────┘
                    │
                    │ StreamWal (gRPC)
                    ├──────────────────────────────────┘
                    │
                    │ StreamLocationUpdates (gRPC)
                    └──────────────────────────────────► Router Node
```

**Key invariant:** Writes flow through the primary only. Replicas and routers never accept mutations directly — they proxy to the primary. The WAL stream is the single authoritative mutation log for the entire cluster.
