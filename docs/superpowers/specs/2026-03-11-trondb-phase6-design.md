# TronDB Phase 6 — Routing Intelligence

## Goal

Build TronDB's routing intelligence layer: health signals, co-location, and semantic routing. All three subsystems are implemented with full scoring, affinity learning, and eviction logic behind a `NodeHandle` trait abstraction. Real gRPC transport deferred to Phase 6b.

## Source Requirements

- `trondb_routing_v1.docx` — full specification covering Health Signal Protocol, Co-location System, Semantic Router
- Phase 5a codebase: single-node Engine with HNSW, SparseIndex, FieldIndex, AdjacencyIndex, LocationTable
- CLAUDE.md: "DecayConfig fields defined but not driven until Phase 6"

## Architecture

New crate `trondb-routing` sits above `trondb-core`:

```
trondb-cli → trondb-routing → trondb-core
                             → trondb-tql
```

The routing layer decides WHERE to execute, then dispatches via `NodeHandle`. In single-node mode, a `LocalNode` adapter wraps the in-process `Engine`. Future Phase 6b adds `RemoteNode` with tonic gRPC.

### Design Principle

The router is stateless per-request. It reads the health signal cache and the co-location index — both maintained asynchronously — and makes a deterministic routing decision in microseconds. No locks, no network calls at routing time.

## Identity Types

All identity types are `String` newtypes (same pattern as `LogicalId`):

```rust
/// Unique identifier for a cluster node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(String);

/// Entity identifier — type alias for LogicalId (re-exported from trondb-core).
pub type EntityId = LogicalId;

/// Identifier for an affinity group.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AffinityGroupId(String);
```

`NodeId` and `AffinityGroupId` follow the same `from_string()` / `as_str()` / `Display` pattern as `LogicalId`. `EntityId` is a type alias, not a separate type, to avoid conversion overhead at the core ↔ routing boundary.

## Crate Structure

```
crates/trondb-routing/
├── Cargo.toml
├── src/
│   ├── lib.rs           # Public API, re-exports
│   ├── node.rs          # NodeHandle trait, NodeId, NodeRole, LocalNode adapter
│   ├── health.rs        # HealthSignal, HealthCache, load_score computation
│   ├── affinity.rs      # AffinityIndex, AffinityGroup, co-occurrence tracking
│   ├── router.rs        # SemanticRouter, candidate scoring, dispatch
│   ├── eviction.rs      # Soft eviction policy (priority rules, LRU tracking)
│   ├── config.rs        # RouterConfig, HealthConfig, ColocationConfig
│   └── error.rs         # RouterError
```

## Subsystem 1: Health Signal Protocol

### Delivery Model — Hybrid Push/Pull

Nodes push health signals to routers on a fixed interval (default 200ms). The router can pull an immediate snapshot on demand when a signal is overdue or when query ACU exceeds `pull_threshold_acu` (default 80).

In the trait-abstracted model, a background task polls `NodeHandle::health_snapshot()` at the configured push interval rather than receiving gRPC streams. Same data, simulated delivery. Anomaly detection is checked on each received signal — if any metric crosses its configured threshold (`cpu_warn_threshold`, `ram_warn_threshold`, `queue_depth_alert`, or p99 > 2× `hnsw_baseline_p99_ms`), the node status is set to Degraded and an immediate re-poll is scheduled.

| Mode | Trigger | Behaviour |
|------|---------|-----------|
| Periodic push | Every `push_interval_ms` | Background task polls `NodeHandle::health_snapshot()`, updates `HealthCache` |
| On-demand pull | Signal overdue, or query ACU > `pull_threshold_acu` | Router calls `NodeHandle::health_snapshot()` synchronously before routing |
| Anomaly detection | Any metric crosses configured threshold | Status set to Degraded, immediate re-poll scheduled on next cycle |

### HealthSignal

```rust
pub struct HealthSignal {
    pub node_id:          NodeId,
    pub node_role:        NodeRole,       // HotNode | WarmNode | ReadReplica
    pub signal_ts:        i64,            // unix ms
    pub sequence:         u64,            // monotonic, detects missed signals
    pub cpu_utilisation:  f32,            // 0.0–1.0
    pub ram_pressure:     f32,            // 0.0–1.0 (hot tier fullness)
    pub hot_entity_count: u64,
    pub hot_tier_capacity: u64,
    pub queue_depth:      u32,
    pub queue_capacity:   u32,
    pub hnsw_p99_ms:      f32,            // rolling 60s window
    pub hnsw_p50_ms:      f32,
    pub replica_lag_ms:   Option<u64>,    // None for write primary
    pub load_score:       f32,            // 0.0–1.0, computed by node
    pub status:           NodeStatus,
}

pub enum NodeStatus {
    Healthy,
    Degraded,       // one+ metrics above warning threshold
    Overloaded,     // load_score > load_shed_threshold
    Faulted,        // unrecoverable error
}

pub enum NodeRole {
    HotNode,
    WarmNode,
    ReadReplica,
}
```

### Load Score Computation

Computed on the node before signal broadcast. Weighted sum of normalised metrics:

```rust
fn compute_load_score(s: &HealthSignal, cfg: &HealthConfig) -> f32 {
    let cpu   = s.cpu_utilisation;
    let ram   = s.ram_pressure;
    let queue = s.queue_depth as f32 / s.queue_capacity as f32;
    let hnsw  = (s.hnsw_p99_ms / cfg.hnsw_baseline_p99_ms).min(1.0);
    let lag   = s.replica_lag_ms
                  .map(|l| (l as f32 / cfg.max_replica_lag_ms).min(1.0))
                  .unwrap_or(0.0);

    (ram   * 0.35)
    + (queue * 0.30)
    + (cpu   * 0.20)
    + (hnsw  * 0.10)
    + (lag   * 0.05)
}
```

### HealthCache

```rust
pub struct HealthCache {
    signals:     DashMap<NodeId, CachedSignal>,
    stale_after: Duration,  // default: 3 × push_interval_ms
}

pub struct CachedSignal {
    signal:      HealthSignal,
    received_at: Instant,
}
```

Methods: `get()`, `healthy_nodes()` (sorted by load_score asc, excludes Faulted), `is_stale()`, `update()`.

### Health Polling Task

The health polling background task is owned by `SemanticRouter`. It is spawned when the router is constructed via `SemanticRouter::new()` and cancelled via a `tokio::sync::watch` shutdown channel when the router is dropped. The task holds `Arc` references to `HealthCache` and the node registry.

```rust
// Spawned in SemanticRouter::new()
let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
let handle = tokio::spawn(health_poll_loop(
    nodes.clone(),
    health_cache.clone(),
    config.health.clone(),
    shutdown_rx,
));
// On drop: shutdown_tx.send(true) → task exits
```

### Configuration

| Key | Default | Description |
|-----|---------|-------------|
| `health.push_interval_ms` | 200 | How often nodes push signals |
| `health.stale_multiplier` | 3 | Signal stale after `push_interval_ms × this` |
| `health.pull_threshold_acu` | 80.0 | ACU above this triggers on-demand pull |
| `health.cpu_warn_threshold` | 0.75 | CPU above this → Degraded |
| `health.ram_warn_threshold` | 0.85 | RAM above this → Degraded, suppresses promotion |
| `health.queue_depth_alert` | 500 | Queue above this → Degraded |
| `health.hnsw_baseline_p99_ms` | 10.0 | Expected p99 under normal load |
| `health.max_replica_lag_ms` | 1000.0 | Maximum replica lag for load score normalisation |
| `health.load_shed_threshold` | 0.85 | load_score above this → Overloaded status |

### gRPC Service Definition (Phase 6b)

Preserved as documentation. Not implemented in Phase 6a.

```protobuf
service HealthService {
    rpc StreamHealth(HealthStreamRequest) returns (stream HealthSignal);
    rpc GetHealthSnapshot(SnapshotRequest) returns (HealthSignal);
}
```

## Subsystem 2: Co-location System

Co-location ensures entities frequently accessed together reside on the same hot node. Declared explicitly by the caller and learned implicitly from access patterns.

### Explicit Co-location Hints

TQL syntax extensions. Colocation clauses appear after VALUES (and after any REPRESENTATION clauses if present), before the semicolon:

```sql
-- Co-location hint on INSERT (after VALUES, before semicolon)
INSERT INTO venues (id, name) VALUES ('v1', 'Glastonbury')
  COLLOCATE_WITH ('v_related_1', 'v_related_2');

-- Named affinity groups (standalone statement)
CREATE AFFINITY GROUP 'festival_cluster';

-- Assign entity to group on INSERT
INSERT INTO venues (id, name) VALUES ('v1', 'Glastonbury')
  AFFINITY GROUP 'festival_cluster';

-- Remove entity from its affinity group
ALTER ENTITY 'v1' DROP AFFINITY GROUP;
```

**Parser grammar additions:**

```
insert_stmt     → INSERT INTO ident field_list VALUES value_list
                  representation_clauses?
                  colocation_clause?
                  SEMICOLON

colocation_clause → COLLOCATE_WITH LPAREN string_list RPAREN
                  | AFFINITY GROUP string_literal

create_affinity → CREATE AFFINITY GROUP string_literal SEMICOLON

alter_entity    → ALTER ENTITY string_literal DROP AFFINITY GROUP SEMICOLON
```

Note: `ALTER` is scoped to `ALTER ENTITY` only. No conflict with future `ALTER COLLECTION` or similar.

### AffinityIndex

```rust
pub struct AffinityIndex {
    // Entity → group membership (explicit groups declared by caller)
    explicit: DashMap<EntityId, AffinityGroupId>,
    // Implicit co-occurrence scores, canonically ordered (min_id, max_id)
    implicit: DashMap<(EntityId, EntityId), f32>,
    // Named group definitions
    groups:   DashMap<AffinityGroupId, AffinityGroup>,
}

pub struct AffinityGroup {
    pub id:          AffinityGroupId,
    pub members:     HashSet<EntityId>,
    pub source:      AffinitySource,  // Explicit | Learned
    pub target_node: Option<NodeId>,
    pub created_at:  i64,
    pub last_seen:   i64,
}

pub enum AffinitySource { Explicit, Learned }
```

**Canonical key ordering:** Implicit co-occurrence keys are stored with `min(a, b)` first, `max(a, b)` second (lexicographic comparison on `LogicalId::as_str()`). This ensures symmetric pairs are never double-counted.

### Access Pattern Tracking

After every SEARCH execution, `record_cooccurrence()` increments scores for all entity pairs in the result set. Each pair receives an increment of **1.0 / (result_count - 1)**, normalised so that a single SEARCH with 10 results contributes the same total score as one with 3 results. This makes `learn_threshold = 0.70` reachable after a consistent pattern of co-appearances regardless of result set size.

```rust
fn record_cooccurrence(results: &[EntityId], index: &AffinityIndex) {
    if results.len() < 2 { return; }
    let increment = 1.0 / (results.len() - 1) as f32;
    for i in 0..results.len() {
        for j in (i+1)..results.len() {
            let key = canonical_pair(&results[i], &results[j]);
            index.implicit.entry(key)
                .and_modify(|s| *s += increment)
                .or_insert(increment);
        }
    }
}

fn canonical_pair(a: &EntityId, b: &EntityId) -> (EntityId, EntityId) {
    if a.as_str() <= b.as_str() {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    }
}
```

### Implicit Affinity Promotion

Background task runs every `colocation.learn_interval_ms` (default 30s). Owned by `SemanticRouter`, uses same shutdown channel pattern as health polling.

```rust
async fn promote_implicit_affinities(index: &AffinityIndex, cfg: &ColocationConfig) {
    for ((a, b), score) in index.implicit.iter() {
        if *score >= cfg.learn_threshold {
            index.create_implicit_group(a, b);
        }
        // Decay scores — adapts to changing patterns
        index.implicit.entry((*a, *b))
             .and_modify(|s| *s *= cfg.decay_factor);
    }
    // Prune entries decayed below 0.01
    index.implicit.retain(|_, score| *score >= 0.01);
}
```

### Group Size Enforcement

`max_group_size` (default 500) is enforced on all group mutations:
- **Explicit group (INSERT with AFFINITY GROUP):** If adding the entity would exceed `max_group_size`, the INSERT succeeds but the AFFINITY GROUP clause is rejected with `RouterError::AffinityGroupFull`. The entity is inserted without group membership.
- **Implicit promotion:** If both entities are already in groups that together exceed `max_group_size`, the implicit promotion is skipped (no error, silently ignored — learned affinities are best-effort).
- **COLLOCATE_WITH:** Creates or extends an implicit group. If the resulting group would exceed `max_group_size`, the colocation hint is ignored with a warning in EXPLAIN output.

### Group Promotion

When an entity is promoted to hot tier, the engine attempts to co-promote all group members to the same node:

1. Find affinity group (explicit first, then implicit)
2. Select target node — lowest `load_score` with RAM headroom below threshold
3. Update `AffinityGroup.target_node` to the selected node
4. If all members fit → promote group
5. If not → soft evict lowest-priority residents, then promote

### Soft Eviction Policy

When promotion would exceed hot tier capacity, soft-evicted entities remain hot until next promotion cycle (default 5s delay).

| Priority | Rule |
|----------|------|
| 1 — Evict first | Ungrouped entities, LRU order |
| 2 — Evict second | Implicit affinity group members, group LRU. Entire group moves together |
| 3 — Evict last | Explicit affinity group members. Only if no other option |
| Never evict | PINNED entities (reserved, not Phase 6) |

### Persistence

Explicit affinity groups are WAL-logged and stored in Fjall (they are user-declared schema-like state that must survive restart):

- **WAL record type:** `AffinityGroupCreate = 0x60`, `AffinityGroupMember = 0x61`, `AffinityGroupRemove = 0x62`
- **Fjall partition:** `affinity.groups` — stores serialised `AffinityGroup` keyed by group ID
- **Fjall partition:** `affinity.members` — stores entity-to-group mappings keyed by entity ID
- **Startup rebuild:** Load from Fjall, same pattern as edge types and collection schemas

Implicit co-occurrence scores and learned groups are **RAM-only** — they are transient learned state rebuilt from ongoing access patterns. No WAL logging, no Fjall persistence.

### Configuration

| Key | Default | Description |
|-----|---------|-------------|
| `colocation.learn_threshold` | 0.70 | Score above which implicit affinity created |
| `colocation.learn_interval_ms` | 30000 | Implicit affinity promotion task interval |
| `colocation.decay_factor` | 0.95 | Multiplier applied each learn interval |
| `colocation.max_group_size` | 500 | Maximum entities per group |
| `colocation.ram_headroom` | 0.80 | RAM pressure must be below this for group promotion |
| `colocation.soft_evict_delay_ms` | 5000 | How long soft-evicted entities stay hot |

## Subsystem 3: Semantic Router

The semantic router uses full query context — verb, estimated ACU, entity affinity — to select the optimal node.

### Routing Decision Pipeline

```
Query arrives at router
       │
       ▼
1. PARSE ROUTING CONTEXT
   Extract: verb, target_entity_ids, estimated_acu
       │
       ▼
2. RESOLVE CANDIDATE NODES
   FETCH/TRAVERSE: Location Table → owning node(s)
   SEARCH: All healthy hot nodes (scatter-gather)
   Filter: Faulted, Overloaded (load_score > shed_threshold)
       │
       ▼
3. CHECK STALENESS
   Stale signal AND ACU > pull_threshold → on-demand pull
       │
       ▼
4. SCORE CANDIDATES
   routing_score per candidate (see below)
       │
       ▼
5. SELECT NODE
   Highest routing_score
   Tie: round-robin for SEARCH, location-stable for FETCH/TRAVERSE
       │
       ▼
6. DISPATCH
   Forward to selected NodeHandle
   Record decision for EXPLAIN
```

**Single-node behaviour:** In single-node mode (one `LocalNode`), the router degenerates gracefully. Steps 2–5 resolve to the single node. No scatter-gather (only one node to scatter to). The routing section still appears in EXPLAIN output, showing the single candidate scored at 1.0. This exercises the full pipeline in tests without special-casing.

### Candidate Scoring

```rust
fn routing_score(
    node:     &NodeId,
    context:  &RoutingContext,
    health:   &HealthCache,
    affinity: &AffinityIndex,
) -> f32 {
    let signal = health.get(node);

    // Health (40%): prefer idle nodes
    let health_score = 1.0 - signal.load_score;

    // Verb fit (30%): verb-specific preference
    let verb_score = verb_fit_score(context.verb, &signal);

    // Entity affinity (30%): fraction of target entities on this node
    let affinity_score = entity_affinity_score(
        &context.target_entities, node, affinity
    );

    (health_score    * config.weight_health)
    + (verb_score    * config.weight_verb_fit)
    + (affinity_score * config.weight_affinity)
}
```

**Verb fit scoring:**

| Verb | Scoring logic |
|------|---------------|
| Infer | CPU < 0.40 → 1.0, < 0.65 → 0.5, else 0.1 |
| Search | `1.0 - (hnsw_p99 / baseline).min(1.0)` |
| Traverse | `1.0 - (queue_depth / queue_capacity)` |
| Fetch | Neutral (0.5) |

**Entity affinity scoring:**

```rust
fn entity_affinity_score(
    targets:  &[EntityId],
    node:     &NodeId,
    affinity: &AffinityIndex,
) -> f32 {
    if targets.is_empty() { return 0.5; }  // SEARCH: neutral
    let on_node = targets.iter()
        .filter(|e| affinity.preferred_node(e) == Some(node))
        .count();
    on_node as f32 / targets.len() as f32
}
```

`AffinityIndex::preferred_node()` returns the `target_node` of the entity's affinity group (if any).

### ACU Estimation

Planner attaches estimated ACU (f32) to each plan:

| Verb | ACU formula |
|------|-------------|
| FETCH | 1.0 |
| SEARCH | 10.0 × k |
| TRAVERSE | 5.0 × depth |
| INFER | 100.0 (future) |

### SEARCH Scatter-Gather

SEARCH has no single entity target — scatters to all healthy hot nodes. Merge strategy:

```rust
enum MergeStrategy {
    TopK { k: usize },
    AffinityWeightedTopK { k: usize, affinity_boost: f32 },
}
```

`AffinityWeightedTopK` applies a small boost (max 5%) to results from nodes with high affinity density. Used when the query has a known co-location context.

### Load Shedding

| Condition | Router behaviour |
|-----------|------------------|
| Node overloaded, replica available | Route to replica |
| Node overloaded, no replica | Return `RouterError::Backpressure { retry_after_ms }` |
| Node Faulted | Never route. Alert if no alternative |
| All candidates overloaded | `RouterError::ClusterOverloaded` |

### EXPLAIN Output

EXPLAIN includes a `routing` section with field names matching `ScoredCandidate`:

```json
{
  "routing": {
    "strategy":       "LOCATION_AWARE",
    "selected_node":  "hot-node-02",
    "routing_score":  0.87,
    "candidates": [
      { "node": "hot-node-01", "score": 0.61,
        "health_score": 0.28, "verb_score": 0.50, "affinity_score": 0.60 },
      { "node": "hot-node-02", "score": 0.87,
        "health_score": 0.80, "verb_score": 0.50, "affinity_score": 1.00 }
    ],
    "affinity_group":  "festival_cluster",
    "health_signal_age_ms": 143,
    "on_demand_pull":  false
  }
}
```

### Configuration

| Key | Default | Description |
|-----|---------|-------------|
| `router.weight.health` | 0.40 | Health score weight |
| `router.weight.verb_fit` | 0.30 | Verb fit weight |
| `router.weight.affinity` | 0.30 | Affinity weight |
| `router.affinity_boost_max` | 0.05 | Max affinity boost in scatter-gather merge |
| `router.retry_after_ms` | 500 | Retry-After hint on backpressure |
| `router.tie_break_search` | `round_robin` | Tie-breaking for SEARCH |
| `router.tie_break_fetch` | `location_stable` | Tie-breaking for FETCH/TRAVERSE |

## Top-Level Types

### NodeHandle Trait

```rust
#[async_trait]
pub trait NodeHandle: Send + Sync {
    fn node_id(&self) -> &NodeId;
    fn node_role(&self) -> NodeRole;
    async fn execute(&self, plan: &Plan) -> Result<QueryResult, EngineError>;
    async fn health_snapshot(&self) -> HealthSignal;
}
```

Note: `NodeHandle::execute()` takes `&Plan` directly (same type as `Executor::execute()`), not an `AnnotatedQuery` wrapper. Routing metadata (ACU, verb, targets) lives in `RoutingContext`, which the router constructs from the plan before dispatch. This avoids introducing a new wrapper type.

### AnnotatedPlan

The router pairs a `Plan` with routing metadata for internal use:

```rust
pub struct AnnotatedPlan {
    pub plan:    Plan,
    pub verb:    QueryVerb,
    pub acu:     f32,
    pub targets: Vec<EntityId>,
}
```

Constructed by `SemanticRouter::annotate(plan: &Plan) -> AnnotatedPlan`, which inspects the plan variant to determine verb, estimate ACU, and extract target entity IDs (from FetchPlan filters, TraversePlan from_id, etc.).

### LocalNode (single-node adapter)

Wraps an in-process `Engine`:

```rust
pub struct LocalNode {
    engine:  Arc<Engine>,
    node_id: NodeId,
}

#[async_trait]
impl NodeHandle for LocalNode {
    async fn execute(&self, plan: &Plan) -> Result<QueryResult, EngineError> {
        self.engine.execute(plan).await
    }

    async fn health_snapshot(&self) -> HealthSignal {
        HealthSignal {
            node_id:          self.node_id.clone(),
            node_role:        NodeRole::HotNode,
            cpu_utilisation:  0.0,  // not measurable in-process
            ram_pressure:     self.engine.entity_count() as f32
                              / HOT_TIER_DEFAULT_CAPACITY as f32,
            hot_entity_count: self.engine.entity_count() as u64,
            hot_tier_capacity: HOT_TIER_DEFAULT_CAPACITY,
            queue_depth:      0,    // no queue in single-node
            queue_capacity:   1000,
            hnsw_p99_ms:      0.0,  // not tracked yet
            hnsw_p50_ms:      0.0,
            replica_lag_ms:   None,
            load_score:       0.0,  // recomputed by cache on receipt
            status:           NodeStatus::Healthy,
            signal_ts:        now_ms(),
            sequence:         0,
        }
    }
}
```

`HOT_TIER_DEFAULT_CAPACITY = 100_000` (configurable). In single-node mode, `cpu_utilisation` is 0.0 (not measurable without OS instrumentation — deferred to Phase 6b with `sysinfo` crate). RAM pressure is approximated from entity count vs capacity.

### SimulatedNode (test adapter)

Wraps a real `Engine` with controllable health metrics:

```rust
pub struct SimulatedNode {
    engine:  Arc<Engine>,
    node_id: NodeId,
    health:  Arc<Mutex<HealthSignal>>,  // test-controlled
}

impl SimulatedNode {
    pub fn set_cpu(&self, v: f32) { ... }
    pub fn set_ram_pressure(&self, v: f32) { ... }
    pub fn set_queue_depth(&self, v: u32) { ... }
    pub fn set_status(&self, s: NodeStatus) { ... }
}
```

### SemanticRouter

```rust
pub struct SemanticRouter {
    health:      Arc<HealthCache>,
    affinity:    Arc<AffinityIndex>,
    location:    Arc<LocationTable>,
    nodes:       DashMap<NodeId, Arc<dyn NodeHandle>>,
    config:      RouterConfig,
    _poll_handle: JoinHandle<()>,
    shutdown_tx:  watch::Sender<bool>,
}

impl SemanticRouter {
    /// Create router and start background health polling.
    pub fn new(
        nodes: Vec<Arc<dyn NodeHandle>>,
        location: Arc<LocationTable>,
        config: RouterConfig,
    ) -> Self;

    /// Route and execute a parsed plan. Entry point for CLI.
    pub async fn route_and_execute(&self, plan: &Plan)
        -> Result<QueryResult, RouterError>;

    /// Annotate a plan with routing metadata.
    fn annotate(&self, plan: &Plan) -> AnnotatedPlan;

    /// Execute the 6-step routing pipeline.
    fn route(&self, annotated: &AnnotatedPlan)
        -> Result<RoutingDecision, RouterError>;
}
```

**CLI call chain:** `parse(tql) → plan(stmt) → router.route_and_execute(plan)`. The router internally calls `annotate()`, `route()`, then `node.execute(plan)`.

### RoutingContext

```rust
pub struct RoutingContext {
    pub verb:            QueryVerb,
    pub target_entities: Vec<EntityId>,
    pub estimated_acu:   f32,
    pub strategy:        RoutingStrategy,
}

pub enum QueryVerb { Fetch, Search, Traverse, Infer }
pub enum RoutingStrategy { LocationAware, ScatterGather }
```

### RoutingDecision

```rust
pub struct RoutingDecision {
    pub node:           NodeId,
    pub score:          f32,
    pub all_candidates: Vec<ScoredCandidate>,
    pub strategy:       RoutingStrategy,
}

pub struct ScoredCandidate {
    pub node:           NodeId,
    pub score:          f32,
    pub health_score:   f32,
    pub verb_score:     f32,
    pub affinity_score: f32,
}
```

### RouterError

```rust
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

## Changes to Existing Crates

### trondb-tql

**New tokens:** `Collocate`, `With`, `Affinity`, `Group`, `Alter`, `Drop`, `Entity` (keyword, distinct from entity data).

**Modified AST nodes:**

```rust
// InsertStmt gains optional colocation
pub struct InsertStmt {
    pub collection: String,
    pub fields: Vec<String>,
    pub values: Vec<Literal>,
    pub vectors: Vec<(String, VectorLiteral)>,
    pub collocate_with: Option<Vec<String>>,    // NEW
    pub affinity_group: Option<String>,          // NEW
}
```

**New AST nodes:**

```rust
pub struct CreateAffinityGroupStmt {
    pub name: String,
}

pub struct AlterEntityDropAffinityStmt {
    pub entity_id: String,
}
```

**New Statement variants:**

```rust
pub enum Statement {
    // ... existing variants ...
    CreateAffinityGroup(CreateAffinityGroupStmt),
    AlterEntityDropAffinity(AlterEntityDropAffinityStmt),
}
```

### trondb-core

- Add `Engine::entity_count() -> usize` — returns total entities across all collections (for LocalNode health metrics)
- Add `Engine::collection_count() -> usize` — returns number of collections
- Add `LocationTable::node_for_entity(entity_id) -> Option<NodeId>` — looks up owning node from `LocationDescriptor.node_address`. Maps `NodeAddress` to `NodeId` via a node registry lookup. In single-node mode, always returns the local node ID.

**New Plan variants:**

```rust
pub enum Plan {
    // ... existing variants ...
    CreateAffinityGroup(CreateAffinityGroupPlan),
    AlterEntityDropAffinity(AlterEntityDropAffinityPlan),
}

pub struct CreateAffinityGroupPlan {
    pub name: String,
}

pub struct AlterEntityDropAffinityPlan {
    pub entity_id: String,
}

// InsertPlan gains colocation fields
pub struct InsertPlan {
    pub collection: String,
    pub fields: Vec<String>,
    pub values: Vec<Literal>,
    pub vectors: Vec<(String, VectorLiteral)>,
    pub collocate_with: Option<Vec<String>>,    // NEW
    pub affinity_group: Option<String>,          // NEW
}
```

**New WAL record types** (in trondb-wal):

```rust
pub enum RecordType {
    // ... existing variants ...
    AffinityGroupCreate = 0x60,
    AffinityGroupMember = 0x61,
    AffinityGroupRemove = 0x62,
}
```

### trondb-cli

- Instantiate `SemanticRouter` with a single `LocalNode` wrapping the `Engine`
- Replace `engine.execute(plan)` with `router.route_and_execute(plan)`
- EXPLAIN output includes routing section when available

### Workspace Cargo.toml

Add `async-trait` to workspace dependencies (required for `NodeHandle` trait with `Send + Sync` bounds on async methods).

## Deferred (per spec "Outstanding Items")

| Item | Priority | Deferred to |
|------|----------|-------------|
| PINNED entity hint | Low | Post-6 (syntax reserved) |
| Session affinity context | Medium | Post-6 |
| Affinity group splitting | Medium | Post-6 (max_group_size enforced, splitting deferred) |
| Health signal persistence | Low | Post-6 |
| Sharding | Post-v1 | — |
| Real gRPC transport | Phase 6b | tonic RemoteNode adapter |
| OS-level CPU measurement | Phase 6b | sysinfo crate for real cpu_utilisation |

## Testing Strategy

All tests use `SimulatedNode` — in-process `NodeHandle` with controllable health metrics.

### Unit Tests
- `compute_load_score` with various metric combinations
- `HealthCache` staleness detection and refresh
- `AffinityIndex` CRUD: explicit groups, implicit co-occurrence, promotion
- Co-occurrence increment normalisation (1.0 / (n-1))
- Co-occurrence canonical key ordering (symmetric pairs)
- Co-occurrence decay and pruning below 0.01
- Soft eviction priority ordering (ungrouped → implicit → explicit)
- `routing_score` with all three weighted factors
- `verb_fit_score` for each verb type
- `entity_affinity_score` computation
- ACU estimation for each query verb
- Group size enforcement (explicit reject, implicit skip)

### Integration Tests
- Multi-node routing: 3 SimulatedNodes, verify routing prefers healthy + affinity-local
- Load shedding: overload one node, verify routing avoids it
- Scatter-gather SEARCH: verify results merged from multiple nodes
- Affinity-weighted merge: verify boost applied correctly
- Co-occurrence learning: run SEARCHes, verify implicit groups form
- Group promotion: verify co-located entities promoted together
- Soft eviction: verify priority ordering and delay
- EXPLAIN routing: verify output includes routing section with correct field names
- Single-node mode: verify LocalNode adapter works end-to-end (score = 1.0)
- COLLOCATE_WITH syntax: parse + execute + verify affinity group membership
- CREATE AFFINITY GROUP + INSERT with AFFINITY GROUP
- ALTER ENTITY DROP AFFINITY GROUP
- Affinity groups survive restart (WAL replay + Fjall restore)
- Implicit scores do NOT survive restart (RAM-only)
