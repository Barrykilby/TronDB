# TronDB Phase 5 — Structural Edges + TRAVERSE

## Goal

Build structural edges: schema-first edge types with optional decay config, per-type Fjall storage, RAM adjacency index for fast TRAVERSE, and the TRAVERSE query verb (single-hop).

## Source Requirements

- v4 design doc Table 19 Phase 5 row: "Structural edges + TRAVERSE"
- v4 Table 3: Control Fabric contains "edge metadata (Phase 5+)"
- Phase 2 design: WAL record types EDGE_WRITE (0x30), EDGE_DELETE (0x34), SCHEMA_CREATE_EDGE_TYPE (0x51)
- User-confirmed: schema-first, optional decay config, Fjall + RAM adjacency index, OrientDB-style TQL syntax

## Architecture

Edge types must be declared before use (`CREATE EDGE TYPE`). Edges are stored in Fjall (one partition per edge type), with a RAM adjacency index (`AdjacencyIndex`) for fast TRAVERSE lookups. The adjacency index is rebuilt from Fjall on startup (same pattern as HNSW). Structural edges have `confidence = 1.0` and `decay_fn = None`.

### Control Fabric (updated from Phase 4)

| Component | Phase 4 | Phase 5 |
|-----------|---------|---------|
| Location Table | DashMap, WAL-logged, snapshotted | Unchanged |
| HNSW Indexes | DashMap<String, HnswIndex>, rebuilt from Fjall | Unchanged |
| Adjacency Index | — | DashMap, rebuilt from Fjall |

## Types

### Edge

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from_id: LogicalId,
    pub to_id: LogicalId,
    pub edge_type: String,
    pub confidence: f32,
    pub metadata: HashMap<String, Value>,
}
```

Structural edges always have `confidence = 1.0`. The field exists for Phase 6 (inferred edges).

### EdgeType

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeType {
    pub name: String,
    pub from_collection: String,
    pub to_collection: String,
    pub decay_config: DecayConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecayConfig {
    pub decay_fn: Option<DecayFn>,
    pub decay_rate: Option<f64>,
    pub floor: Option<f64>,
    pub promote_threshold: Option<f64>,
    pub prune_threshold: Option<f64>,
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self {
            decay_fn: None,
            decay_rate: None,
            floor: None,
            promote_threshold: None,
            prune_threshold: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DecayFn {
    Exponential,
    Linear,
    Step,
}
```

Phase 5 structural edges use `DecayConfig::default()` (no decay). The decay fields are defined now but only driven in Phase 6+.

### AdjacencyIndex

```rust
pub struct AdjacencyIndex {
    forward: DashMap<(LogicalId, String), Vec<AdjEntry>>,
}

#[derive(Debug, Clone)]
pub struct AdjEntry {
    pub to_id: LogicalId,
    pub confidence: f32,
}
```

The forward map provides O(1) lookup by `(from_id, edge_type)`. Reverse lookup (given `to_id`, find `from_id`) is deferred.

### AdjacencyIndex API

```rust
impl AdjacencyIndex {
    pub fn new() -> Self;
    pub fn insert(&self, from_id: &LogicalId, edge_type: &str, to_id: &LogicalId, confidence: f32);
    pub fn remove(&self, from_id: &LogicalId, edge_type: &str, to_id: &LogicalId);
    pub fn get(&self, from_id: &LogicalId, edge_type: &str) -> Vec<AdjEntry>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

## TQL Grammar

### New Keywords

`EDGE`, `TYPE`, `TRAVERSE`, `DEPTH`, `TO`, `DELETE`

### New Statements

```sql
-- Schema: declare edge type
CREATE EDGE TYPE <name> FROM <collection> TO <collection>;

-- Data: create/delete edges
INSERT EDGE <type> FROM '<from_id>' TO '<to_id>';
INSERT EDGE <type> FROM '<from_id>' TO '<to_id>' WITH (key = 'value', key2 = 42);
DELETE EDGE <type> FROM '<from_id>' TO '<to_id>';

-- Query: traverse edges
TRAVERSE <type> FROM '<entity_id>';
TRAVERSE <type> FROM '<entity_id>' DEPTH 1 LIMIT 10;

-- Explain
EXPLAIN TRAVERSE <type> FROM '<entity_id>';
```

### AST Additions

```rust
pub enum Statement {
    // ... existing ...
    CreateEdgeType(CreateEdgeTypeStmt),
    InsertEdge(InsertEdgeStmt),
    DeleteEdge(DeleteEdgeStmt),
    Traverse(TraverseStmt),
}

pub struct CreateEdgeTypeStmt {
    pub name: String,
    pub from_collection: String,
    pub to_collection: String,
}

pub struct InsertEdgeStmt {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
    pub metadata: Vec<(String, Literal)>,
}

pub struct DeleteEdgeStmt {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
}

pub struct TraverseStmt {
    pub edge_type: String,
    pub from_id: String,
    pub depth: usize,       // default 1
    pub limit: Option<usize>,
}
```

DEPTH defaults to 1. DEPTH > 1 parsed but returns `UnsupportedOperation` at execution.

## Executor Changes

### Struct

```rust
pub struct Executor {
    store: FjallStore,
    wal: WalWriter,
    location: Arc<LocationTable>,
    indexes: DashMap<String, HnswIndex>,
    adjacency: AdjacencyIndex,              // NEW
    edge_types: DashMap<String, EdgeType>,   // NEW
}
```

### CREATE EDGE TYPE Path

```
1. Validate from/to collections exist
2. WAL: TxBegin → SchemaCreateEdgeType → TxCommit
3. Store EdgeType in Fjall (edge_types partition)
4. Register in edge_types DashMap
5. Ack
```

### INSERT EDGE Path

```
1. Validate edge type exists
2. Validate from/to entities exist in their respective collections
3. Build Edge { from_id, to_id, edge_type, confidence: 1.0, metadata }
4. WAL: TxBegin → EdgeWrite → TxCommit
5. Store in Fjall edges:{edge_type} partition, key = {from_id}:{to_id}
6. Insert into AdjacencyIndex
7. Ack
```

Duplicate INSERT EDGE (same from/to/type) is idempotent — overwrites metadata, confidence unchanged.

### DELETE EDGE Path

```
1. WAL: TxBegin → EdgeDelete → TxCommit
2. Remove from Fjall
3. Remove from AdjacencyIndex
4. Ack (no-op if edge doesn't exist)
```

### TRAVERSE Path

```
1. Validate edge type exists
2. If depth > 1, return UnsupportedOperation
3. Look up AdjacencyIndex.get(from_id, edge_type) → Vec<AdjEntry>
4. Resolve each to_id → Entity from Fjall (via edge type's to_collection)
5. Apply LIMIT
6. Return QueryResult with mode = Deterministic, tier = "Ram"
```

### EXPLAIN TRAVERSE

```rust
Plan::Traverse(p) => {
    props.push(("mode", "Deterministic".into()));
    props.push(("verb", "TRAVERSE".into()));
    props.push(("edge_type", p.edge_type.clone()));
    props.push(("tier", "Ram".into()));
    props.push(("strategy", "AdjacencyIndex".into()));
    props.push(("depth", p.depth.to_string()));
}
```

### WAL Replay

Add handlers for EdgeWrite, EdgeDelete, SchemaCreateEdgeType to replay into Fjall. AdjacencyIndex rebuilt from Fjall after replay (not from WAL records directly).

## FjallStore Changes

New partitions:
- `edge_types` — stores EdgeType structs, keyed by name
- `edges:{edge_type}` — stores Edge structs, keyed by `{from_id}:{to_id}`

New methods:
- `create_edge_type(edge_type: &EdgeType)`
- `get_edge_type(name: &str) -> Option<EdgeType>`
- `list_edge_types() -> Vec<EdgeType>`
- `insert_edge(edge: &Edge)`
- `delete_edge(edge_type: &str, from_id: &str, to_id: &str)`
- `scan_edges(edge_type: &str) -> Vec<Edge>`

## Engine::open — AdjacencyIndex Rebuild

After HNSW rebuild, scan all edge types and rebuild adjacency index:

```rust
for edge_type in executor.list_edge_types() {
    let edges = executor.scan_edges(&edge_type.name)?;
    for edge in &edges {
        executor.adjacency().insert(
            &edge.from_id, &edge.edge_type,
            &edge.to_id, edge.confidence,
        );
    }
}
```

## Error Handling

- CREATE EDGE TYPE with non-existent from/to collection → `CollectionNotFound`
- CREATE EDGE TYPE duplicate → `EdgeTypeAlreadyExists` (new error)
- INSERT EDGE with undeclared edge type → `EdgeTypeNotFound` (new error)
- INSERT EDGE with non-existent from/to entity → `EntityNotFound` (new error)
- INSERT EDGE duplicate (same from/to/type) → idempotent (overwrite metadata)
- DELETE EDGE that doesn't exist → no-op
- TRAVERSE with DEPTH > 1 → `UnsupportedOperation`
- TRAVERSE on undeclared edge type → `EdgeTypeNotFound`

New error variants: `EdgeTypeNotFound`, `EdgeTypeAlreadyExists`, `EntityNotFound`.

## Crate Placement

- New file: `crates/trondb-core/src/edge.rs` — Edge, EdgeType, DecayConfig, DecayFn, AdjacencyIndex
- Modifications: executor.rs, store.rs, lib.rs, planner.rs, error.rs
- TQL parser: token.rs, ast.rs, parser.rs (in trondb-tql crate)
- No new crates or dependencies

## What Phase 5 Does NOT Build

- Decay computation (fields stored but not driven)
- Inferred edges (Phase 6)
- Edge confirmation (Phase 6)
- Confidence updates (Phase 8)
- Multi-hop TRAVERSE (DEPTH > 1)
- Reverse traversal (given to_id, find from_id)
- Edge metadata querying/filtering in TRAVERSE

## Testing

- Unit tests for AdjacencyIndex (insert, remove, get, empty, idempotent)
- Unit tests for Edge/EdgeType serialization
- Parser tests for CREATE EDGE TYPE, INSERT EDGE, DELETE EDGE, TRAVERSE
- Integration test: CREATE EDGE TYPE → INSERT EDGE → TRAVERSE returns entity
- Integration test: DELETE EDGE → TRAVERSE returns empty
- Integration test: INSERT EDGE with metadata
- Integration test: INSERT EDGE idempotent (duplicate overwrites metadata)
- Integration test: TRAVERSE on non-existent edge type → error
- Integration test: INSERT EDGE on non-existent edge type → error
- Integration test: EXPLAIN TRAVERSE shows Deterministic + AdjacencyIndex
- Integration test: DEPTH > 1 → UnsupportedOperation
- Integration test: edges survive restart (insert → restart → traverse finds edge)
- Integration test: WAL replay recovers edges
