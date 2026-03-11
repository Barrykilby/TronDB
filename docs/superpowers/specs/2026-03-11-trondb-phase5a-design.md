# TronDB Phase 5a — Indexes (Field Index + Sparse Vector Index + Hybrid SEARCH)

## Goal

Add field indexes and sparse vector indexes to TronDB: Fjall-backed field indexes for deterministic lookups (equality, range, compound, partial), a RAM-resident inverted sparse index for SPLADE-style sparse vector search, hybrid SEARCH with reciprocal rank fusion, and ScalarPreFilter for WHERE + SEARCH optimisation. Replace the simple `CREATE COLLECTION` syntax with the full schema declaration form.

## Source Requirements

- trondb_indexes_v1.docx: Full index architecture — sparse index, field index, hybrid SEARCH, ScalarPreFilter, H3 geospatial via field index
- trondb_design_v4.docx: Build phase table, five index types, query planner strategy selection
- User-confirmed: full schema syntax (option A), no backwards-compatible shorthand, ACU costs deferred, EXPLAIN omits cost fields entirely

## Architecture

### Schema Grammar — CREATE COLLECTION Expansion

The current `CREATE COLLECTION venues WITH DIMENSIONS 3;` syntax is replaced entirely with a block form:

```sql
CREATE COLLECTION venues (
    REPRESENTATION identity
        MODEL 'jina-v4'
        DIMENSIONS 1024
        METRIC cosine,

    REPRESENTATION sparse_title
        MODEL 'splade-v3'
        METRIC inner_product
        SPARSE true,

    field start_datetime DATETIME,
    field status TEXT,
    field venue_id ENTITY_REF(venues),
    field h3_res8 TEXT,
    field publish_ready BOOL,

    INDEX idx_status ON (status),
    INDEX idx_venue_start ON (venue_id, start_datetime),
    INDEX idx_h3 ON (h3_res8),
    INDEX idx_publish_ready ON (publish_ready) WHERE publish_ready = true
);
```

All existing tests must be updated to the new syntax. No backwards-compatible shorthand.

### New Keywords

`REPRESENTATION`, `MODEL`, `METRIC`, `SPARSE`, `FIELD`, `DATETIME`, `TEXT`, `BOOL`, `ENTITY_REF`, `INDEX`, `ON`, `INNER_PRODUCT`, `COSINE`

Note: `TRUE`, `FALSE`, `WITH`, `WHERE` already exist. `DELETE`, `EDGE`, `TRAVERSE`, `DEPTH`, `TO` were added in Phase 5.

### AST Types

```rust
pub struct CreateCollectionStmt {
    pub name: String,
    pub representations: Vec<RepresentationDecl>,
    pub fields: Vec<FieldDecl>,
    pub indexes: Vec<IndexDecl>,
}

pub struct RepresentationDecl {
    pub name: String,
    pub model: Option<String>,
    pub dimensions: Option<usize>,
    pub metric: Metric,
    pub sparse: bool,
}

pub enum Metric {
    Cosine,
    InnerProduct,
}

pub struct FieldDecl {
    pub name: String,
    pub field_type: FieldType,
}

pub enum FieldType {
    Text,
    DateTime,
    Bool,
    Int,
    Float,
    EntityRef(String),
}

pub struct IndexDecl {
    pub name: String,
    pub fields: Vec<String>,
    pub partial_condition: Option<WhereClause>,
}
```

### SEARCH Syntax Expansion

```sql
-- Dense only (current, but with new REPRESENTATION awareness)
SEARCH venues NEAR VECTOR [0.1, 0.2, 0.3] LIMIT 20;

-- Sparse only
SEARCH venues NEAR SPARSE [1:0.8, 42:0.5, 1337:0.3] LIMIT 20;

-- Hybrid (both dense and sparse in one query)
SEARCH venues NEAR VECTOR [0.1, 0.2, 0.3] NEAR SPARSE [1:0.8, 42:0.5] LIMIT 20;

-- With ScalarPreFilter
SEARCH venues WHERE h3_res4 = '8928342e3ffffff' NEAR VECTOR [0.1, 0.2, 0.3] LIMIT 10;
```

New AST additions:

```rust
pub struct SearchStmt {
    pub collection: String,
    pub fields: FieldList,
    pub dense_vector: Option<Vec<f64>>,
    pub sparse_vector: Option<Vec<(u32, f32)>>,
    pub filter: Option<WhereClause>,        // WHERE for ScalarPreFilter
    pub confidence: Option<f64>,
    pub limit: Option<usize>,
}
```

Sparse vector literal syntax: `[dim:weight, dim:weight, ...]` where dim is an integer and weight is a float.

## Types

### VectorData

```rust
pub enum VectorData {
    Dense(Vec<f32>),
    Sparse(Vec<(u32, f32)>),
}
```

Replaces the current `vector: Vec<f32>` field on `Representation`.

### Encoding

```rust
pub enum Encoding {
    Float32,
    Int8,
    Binary,
    Sparse,
}
```

`Sparse` is new. `Int8` and `Binary` are defined but not driven until Phase 7 (warm tier).

### CollectionSchema

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSchema {
    pub name: String,
    pub representations: Vec<StoredRepresentation>,
    pub fields: Vec<StoredField>,
    pub indexes: Vec<StoredIndex>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRepresentation {
    pub name: String,
    pub model: Option<String>,
    pub dimensions: Option<usize>,
    pub metric: Metric,
    pub sparse: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredField {
    pub name: String,
    pub field_type: FieldType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredIndex {
    pub name: String,
    pub fields: Vec<String>,
    pub partial_condition: Option<String>,  // serialised WHERE clause
}
```

Stored in Fjall `_meta` partition under key `collection:{name}:schema`.

### FieldType

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FieldType {
    Text,
    DateTime,
    Bool,
    Int,
    Float,
    EntityRef(String),
}
```

### Metric

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Metric {
    Cosine,
    InnerProduct,
}

impl Default for Metric {
    fn default() -> Self {
        Metric::Cosine
    }
}
```

## Field Index

### Storage

One Fjall partition per declared index, named `fidx.{collection}.{index_name}`. Keys are field values serialised as sortable bytes. Values are `EntityId` (serialised LogicalId).

### Sortable Byte Encoding

| FieldType | Encoding |
|-----------|----------|
| Text | Raw UTF-8 bytes, null-terminated for compound key separation |
| DateTime | ISO 8601 string stored as-is (lexicographic sort works) |
| Bool | `0x00` (false) / `0x01` (true) |
| Int | Big-endian i64 with sign bit flipped (negative sorts before positive) |
| Float | IEEE 754 with sign/exponent manipulation for sort order |
| EntityRef | Same as Text (LogicalId string) |

### Compound Indexes

Key is the concatenation of field values in declaration order, separated by null bytes. `idx_venue_start ON (venue_id, start_datetime)` produces keys like `{venue_id_bytes}\x00{start_datetime_bytes}`. Fjall's native prefix scan handles queries on the leading field(s) only.

### Partial Indexes

Only entities matching the `WHERE` condition are indexed. Evaluated at write time. `INDEX idx_publish_ready ON (publish_ready) WHERE publish_ready = true` stores only entries where `publish_ready` is true.

### FieldIndex API

```rust
pub struct FieldIndex {
    partition: fjall::PartitionHandle,
    schema: StoredIndex,
    field_types: Vec<(String, FieldType)>,  // resolved from collection schema
}

impl FieldIndex {
    pub fn new(partition: fjall::PartitionHandle, schema: StoredIndex, field_types: Vec<(String, FieldType)>) -> Self;
    pub fn insert(&self, entity: &Entity, collection_fields: &[StoredField]) -> Result<(), EngineError>;
    pub fn remove(&self, entity: &Entity, collection_fields: &[StoredField]) -> Result<(), EngineError>;
    pub fn lookup_eq(&self, values: &[Value]) -> Result<Vec<LogicalId>, EngineError>;
    pub fn lookup_range(&self, from: &[Value], to: &[Value]) -> Result<Vec<LogicalId>, EngineError>;
    pub fn lookup_prefix(&self, prefix_values: &[Value]) -> Result<Vec<LogicalId>, EngineError>;
}
```

### Write Path Integration

Every `ENTITY_WRITE` triggers field index updates:
1. Check which indexes cover the entity's collection
2. For each matching index, check partial condition (if any)
3. Extract declared field values from entity metadata
4. Encode sortable key, insert into index partition
5. On `ENTITY_DELETE`, remove corresponding entries

### Startup Rebuild

Field indexes are not rebuilt from entity data — they are Fjall partitions and survive restart natively. The `FieldIndex` structs are re-instantiated from the collection schema on startup.

### H3 Geospatial

H3 cell IDs are stored as plain `TEXT` fields and indexed as field indexes. Hierarchical range queries across resolutions are prefix lookups. No spatial index needed.

```sql
-- Find events in an H3 res4 cell
FETCH events WHERE h3_res4 = '8928342e3ffffff';

-- Find events in a more precise res8 cell
FETCH events WHERE h3_res8 = '89283470c27ffff';
```

## Sparse Vector Index

### Structure

RAM-resident inverted index, one per sparse representation per collection:

```rust
pub struct SparseIndex {
    postings: DashMap<u32, Vec<(LogicalId, f32)>>,  // dimension_id → [(entity, weight)]
}
```

### SparseIndex API

```rust
impl SparseIndex {
    pub fn new() -> Self;
    pub fn insert(&self, entity_id: &LogicalId, vector: &[(u32, f32)]);
    pub fn remove(&self, entity_id: &LogicalId, vector: &[(u32, f32)]);
    pub fn search(&self, query: &[(u32, f32)], k: usize) -> Vec<(LogicalId, f32)>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

### Search Algorithm

1. For each non-zero dimension in the query vector, fetch the posting list
2. Accumulate dot product scores per entity: `score[entity] += query_weight * posting_weight`
3. Return top-k entities by score

### Index-Time Filtering

Sparse dimensions with weight below `min_weight` (hardcoded 0.001 for now, configurable later) are dropped at index time.

### Startup Rebuild

Rebuilt from Fjall on startup — scan all entities with sparse representations, insert each into the SparseIndex. Same pattern as HNSW and AdjacencyIndex.

## Hybrid SEARCH

### Reciprocal Rank Fusion (RRF)

When both `NEAR VECTOR` and `NEAR SPARSE` are present, both searches run and results are merged:

```
score(entity) = Σ 1 / (k + rank_in_list)
```

Where `k` defaults to 60. Each result list contributes a rank-based score. Combined scores determine final order.

### Merge Function

```rust
pub fn merge_rrf(
    dense_results: &[(LogicalId, f32)],
    sparse_results: &[(LogicalId, f32)],
    rrf_k: usize,
) -> Vec<(LogicalId, f32)>;
```

New file: `crates/trondb-core/src/hybrid.rs`

## Planner Changes

### Search Strategy

```rust
pub enum SearchStrategy {
    Hnsw,
    Sparse,
    Hybrid { rrf_k: usize },
}

pub struct SearchPlan {
    pub collection: String,
    pub dense_vector: Option<Vec<f64>>,
    pub sparse_vector: Option<Vec<(u32, f32)>>,
    pub strategy: SearchStrategy,
    pub pre_filter: Option<PreFilter>,
    pub k: usize,
    pub confidence_threshold: f64,
}

pub struct PreFilter {
    pub index_name: String,
    pub condition: WhereClause,
}
```

### Fetch Strategy

```rust
pub enum FetchStrategy {
    FullScan,
    FieldIndexLookup { index_name: String },
}
```

`FetchPlan` gains a `strategy: FetchStrategy` field.

### Strategy Selection

| Query | Available indexes | Strategy |
|-------|------------------|----------|
| FETCH WHERE field = x | Field index on field | FieldIndexLookup |
| FETCH WHERE field = x | No field index | FullScan |
| SEARCH NEAR VECTOR | HNSW only | Hnsw |
| SEARCH NEAR SPARSE | SparseIndex only | Sparse |
| SEARCH NEAR VECTOR NEAR SPARSE | Both | Hybrid |
| SEARCH WHERE x NEAR VECTOR | Field index + HNSW | ScalarPreFilter + Hnsw |

The planner needs access to collection schemas to make these decisions. The executor exposes collection schema info to the planner.

## Executor Changes

### Struct

```rust
pub struct Executor {
    store: FjallStore,
    wal: WalWriter,
    location: Arc<LocationTable>,
    indexes: DashMap<String, HnswIndex>,
    sparse_indexes: DashMap<String, SparseIndex>,   // NEW: key = "{collection}:{repr_name}"
    field_indexes: DashMap<String, FieldIndex>,      // NEW: key = "{collection}:{index_name}"
    adjacency: AdjacencyIndex,
    edge_types: DashMap<String, EdgeType>,
    schemas: DashMap<String, CollectionSchema>,      // NEW: collection schemas in memory
}
```

### CREATE COLLECTION Path

1. Parse the full schema block
2. WAL: TxBegin → SchemaCreateColl (with full CollectionSchema payload) → TxCommit
3. Store CollectionSchema in Fjall `_meta` partition
4. Create Fjall partitions for each representation and each index
5. Instantiate HNSW indexes for dense representations
6. Instantiate SparseIndex for sparse representations
7. Instantiate FieldIndex for each declared index
8. Register in `schemas` DashMap
9. Ack

### INSERT Path (Updated)

After the current entity write + location table + HNSW index steps:

1. For each sparse representation on the entity, insert into SparseIndex
2. For each field index on the collection, evaluate partial condition, encode key, insert

### FETCH Path (Updated)

1. Planner checks if WHERE clause matches a field index
2. If yes: use FieldIndex.lookup_eq/lookup_range to get EntityIds, then resolve from Fjall
3. If no: full scan (current behaviour)

### SEARCH Path (Updated)

1. If NEAR VECTOR only → HNSW search (current, but representation-aware)
2. If NEAR SPARSE only → SparseIndex search
3. If both → parallel HNSW + SparseIndex, merge via RRF
4. If WHERE clause present + field index exists → ScalarPreFilter: narrow via field index first, then vector search on reduced set

## EXPLAIN Updates

No ACU costs (deferred). Shows strategy and indexes consulted:

```
-- FETCH with field index
mode       | Deterministic
verb       | FETCH
collection | events
strategy   | FieldIndexLookup
index      | idx_status

-- Hybrid SEARCH
mode       | Probabilistic
verb       | SEARCH
collection | venues
strategy   | HybridSearch
dense      | HNSW (identity)
sparse     | SparseIndex (sparse_title)
merge      | RRF (k=60)
tier       | Ram

-- ScalarPreFilter + SEARCH
mode       | Probabilistic
verb       | SEARCH
collection | venues
strategy   | ScalarPreFilter
pre_filter | idx_h3 (h3_res4 = '8928342e3ffffff')
search     | HNSW
tier       | Ram
```

## Error Handling

New error variants:

- `DuplicateIndex(String)` — index name already exists in collection
- `DuplicateRepresentation(String)` — representation name already exists
- `DuplicateField(String)` — field name already exists
- `FieldNotIndexed(String)` — WHERE clause references field with no index
- `SparseVectorRequired(String)` — SEARCH NEAR SPARSE on collection with no sparse representation
- `InvalidFieldType { field: String, expected: String, got: String }` — WHERE value doesn't match declared field type

## Startup Rebuild Order

1. WAL replay (entities, edges, location table, schemas)
2. Load collection schemas from Fjall `_meta`
3. HNSW rebuild from Fjall (per dense representation per collection)
4. SparseIndex rebuild from Fjall (per sparse representation per collection)
5. FieldIndex re-instantiation (Fjall partitions survive restart — just create FieldIndex structs)
6. AdjacencyIndex rebuild from Fjall (per edge type)

## WAL

No new record types. Field indexes and sparse indexes are derived from `ENTITY_WRITE` records. The `SchemaCreateColl` payload expands to include the full `CollectionSchema` struct.

## Crate Placement

- New file: `crates/trondb-core/src/field_index.rs` — FieldIndex, sortable byte encoding
- New file: `crates/trondb-core/src/sparse_index.rs` — SparseIndex, posting list search
- New file: `crates/trondb-core/src/hybrid.rs` — RRF merge function
- Modifications: token.rs, ast.rs, parser.rs (trondb-tql crate)
- Modifications: types.rs, store.rs, planner.rs, executor.rs, lib.rs, error.rs (trondb-core crate)
- No new crates or external dependencies

## What Phase 5a Does NOT Build

- ACU cost model (deferred — EXPLAIN omits cost fields entirely)
- Multi-node routing (Phase 9+)
- Warm/cold tier (Phase 7)
- Int8/Binary encodings (Phase 7)
- Configurable sparse min_weight (hardcoded 0.001 for now)
- Configurable RRF k parameter per query (hardcoded 60 for now)
- Linear combination merge strategy (RRF only for now)
- INFER verb (Phase 6)
- Inference triggers (Phase 5b/6)
- Decay/reinforcement (Phase 6/8)
- Query hints (`/*+ NO_PROMOTE MAX_ACU(200) */`)

## Testing

### Unit Tests
- Sortable byte encoding round-trips for all field types (Text, DateTime, Bool, Int, Float, EntityRef)
- Int encoding: negative sorts before positive, zero in correct position
- SparseIndex: insert, search, remove, dot product correctness, top-k ranking
- RRF merge with known rank lists produces correct combined ordering
- FieldIndex: compound key encoding, prefix scan on leading fields
- Partial index: only matching entries indexed, non-matching skipped

### Parser Tests
- New CREATE COLLECTION block syntax (representations, fields, indexes)
- CREATE COLLECTION with partial index (WHERE clause)
- SEARCH NEAR SPARSE with sparse vector literal
- Hybrid SEARCH with both NEAR VECTOR and NEAR SPARSE
- SEARCH with WHERE clause (ScalarPreFilter syntax)

### Integration Tests
- CREATE COLLECTION with fields + indexes → INSERT → FETCH via field index
- Compound index lookup (two-field key)
- Partial index (only matching entries indexed, query confirms)
- Sparse SEARCH returns ranked results with correct ordering
- Hybrid SEARCH merges dense + sparse results correctly
- ScalarPreFilter narrows SEARCH results (fewer results than unfiltered)
- EXPLAIN shows strategy/index for each query type (FieldIndexLookup, Hnsw, Sparse, HybridSearch, ScalarPreFilter)
- All indexes survive restart (rebuild/re-instantiation from Fjall)
- H3 field index equality lookup works
- Existing Phase 5 edge tests updated to new CREATE COLLECTION syntax and still pass
- INSERT EDGE still works with new schema format
- TRAVERSE still works with new schema format
