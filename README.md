# TronDB

**An inference-first storage engine, written in Rust.**

Most databases answer the question: *"What did I put in here?"* TronDB answers a different question: *"What does this data imply?"*

---

## What Is TronDB?

TronDB is a storage engine where meaning is native. The semantic interior is vector-first — similarity is measured by distance, relationships emerge from geometry. The structural boundary provides explicit types, edges, routing, and durability.

It is not a wrapper around an existing database with a vector column bolted on. Vectors, edges, confidence scores, and inference are first-class primitives in the storage engine, the query planner, and the replication protocol.

---

## The Plain English Version

A regular database is a filing cabinet. You put things in labelled folders and retrieve them by label. Fast, reliable, dumb.

TronDB is more like a brain that has read every file in the cabinet and formed opinions. It understands relationships between entities. It connects new information to everything it already knows. And it tells you how confident it is — and why.

### Two kinds of knowledge

- **Structural facts** — things you explicitly told it. Confidence 1.0. Stored, indexed, durable. Behave exactly like a conventional database — deterministic, no probabilistic layer involved.
- **Inferred facts** — things the engine worked out. Confidence 0.0–1.0. Generated from signals, tracked, decayed over time if not reinforced, promoted to confirmed if evidence accumulates.

Both are first-class. You can query either, filter by confidence threshold, and ask TronDB to show its working. The probabilistic layer only activates when you ask for it.

### Storage tiers

| Tier | Medium | Role |
|------|--------|------|
| Hot | RAM | Actively queried entities. Full vector index in memory. Millisecond lookups. |
| Warm | NVMe/Disk | Recent but less active. Int8 quantised vectors (~75% size reduction). |
| Archive | High-capacity storage | Rarely touched. Binary quantised vectors (~97% size reduction). |

Data flows between tiers automatically based on access patterns. Entities are promoted on demand mid-query when accessed via FETCH.

---

## How It Works

### Vectors and meaning

Every entity is represented as one or more vectors: lists of numbers that encode its meaning in high-dimensional space. Similar entities have vectors that are close together. The engine uses these distances to find relationships, even when no explicit relationship was stored.

An entity can have multiple representations — each a different lens on the same thing:

- **Passthrough** — caller provides pre-computed vectors at INSERT time
- **Managed** — the engine generates vectors from declared entity fields using a pluggable vectoriser (local ONNX model, network model server, or external API)
- **Composite** — multiple fields amalgamated into a single representation

```sql
CREATE COLLECTION venues (
    MODEL 'bge-small-en-v1.5'
    MODEL_PATH '/models/bge-small-en-v1.5.onnx'
    DEVICE 'cpu'

    FIELD name TEXT,
    FIELD description TEXT,
    FIELD category TEXT,

    -- Passthrough: caller provides vector
    REPRESENTATION identity DIMENSIONS 384,

    -- Managed: engine generates vector from fields
    REPRESENTATION semantic DIMENSIONS 384 FIELDS (name, description, category),

    INDEX idx_category ON (category),
);
```

### Pluggable vectoriser

TronDB generates vectors, not just stores them. The vectoriser is pluggable with three tiers:

| Tier | Type | Auth | Example |
|------|------|------|---------|
| 1 | Local ONNX | None | `MODEL_PATH '/models/bge.onnx'` |
| 2 | Network cluster | None | `VECTORISER 'network' ENDPOINT 'http://gpu-box:8080/embed'` |
| 3 | External API | Required | `VECTORISER 'external' ENDPOINT 'https://api.openai.com/...' AUTH 'env:OPENAI_API_KEY'` |

### Representation validity and mutation cascade

When an entity's source fields change via UPDATE, the engine detects which representations depend on the changed fields, marks them dirty, and queues background recomputation. Dirty representations remain fetchable by ID but are excluded from SEARCH results until recomputed.

Each representation carries a `recipe_hash` (SHA-256 of model ID + field names) for staleness detection. The recomputation cycle:

```
UPDATE field → detect affected representations → mark Dirty (WAL-logged)
    → background recompute via vectoriser → write new vector (WAL-logged)
    → transition to Clean → entity re-enters SEARCH results
```

### The two fabrics

TronDB has two internal layers that never mix:

- **Control Fabric** — knows where everything is: tier location, node address, graph topology, edge metadata, confidence scores, representation state. Always RAM-resident via DashMap with a contention-minimised concurrent lookup path. Sub-microsecond access. This is the routing brain.
- **Data Fabric** — holds actual entity bytes, vectors, and raw edges. Lives wherever the tier says. The control fabric tells you where to look before you look.

The engine never searches for something. It always knows where to go first.

### Edges and confidence

Relationships between entities are edges. Every edge has a declared type, a direction, and a confidence score.

Edge types must be declared before use — the same discipline as collections. A minimal declaration requires only a name and the from/to collection pair. Decay configuration is optional — if omitted, confidence is fixed and the edge never decays.

- **Structural edges** start at confidence 1.0 and stay there.
- **Inferred edges** (planned) start lower and change: reinforced when evidence supports them, decayed when they go stale, pruned when they fall below the floor.

```sql
-- Minimal (structural, no decay)
CREATE EDGE TYPE 'performs_at'
  FROM acts TO venues;

-- With decay
CREATE EDGE TYPE 'visited'
  FROM people TO venues
  DECAY EXPONENTIAL RATE 0.05 FLOOR 0.1 PRUNE 0.05;
```

### Querying

TronDB has a query language called TQL (TronDB Query Language). Current verbs:

| Verb | What it does |
|------|-------------|
| `FETCH` | Get entities by ID or field value. Deterministic. Fast. |
| `SEARCH` | Find entities by semantic similarity — explicit vector, sparse vector, or natural language text. Returns ranked results with similarity scores. |
| `TRAVERSE` | Walk the graph. BFS multi-hop (depth cap 10) with cycle detection. Returns entities connected via a named edge type. |

Natural language search encodes the query string through the collection's registered vectoriser:

```sql
-- Vector search (explicit)
SEARCH venues NEAR VECTOR [0.1, 0.2, ...] LIMIT 10;

-- Natural language search (vectoriser encodes the query)
SEARCH venues NEAR 'live jazz in Bristol' USING semantic LIMIT 10;

-- Hybrid dense + sparse
SEARCH venues NEAR VECTOR [...] NEAR SPARSE [(0, 0.8), (42, 0.3)] LIMIT 10;

-- With pre-filter
SEARCH venues WHERE category = 'music' NEAR 'jazz clubs' LIMIT 10;
```

Prefix any query with `EXPLAIN` to see the full reasoning: strategy used, index names, routing decisions, candidate scores.

---

## Architecture

### Crate structure

```
trondb-tql          TQL parser (logos lexer + recursive descent). No engine dependency.
trondb-wal          Write-Ahead Log: MessagePack records, CRC32, segment files, crash recovery.
trondb-core         Engine: types, Fjall store, Location Table, HNSW, edges, planner, executor.
trondb-vectoriser   Pluggable vectoriser implementations (Passthrough, Mock, ONNX, Network, External).
trondb-routing      Routing: health signals, co-location (AffinityIndex), semantic router.
trondb-proto        Protobuf + tonic codegen: gRPC service, Plan/Result/WAL conversions.
trondb-server       gRPC server: primary/replica/router roles, WAL streaming, scatter-gather.
trondb-cli          Interactive REPL (Tokio + rustyline).
```

### Storage: Fjall

TronDB does not implement its own LSM tree. It uses [Fjall](https://github.com/fjall-rs/fjall), a pure-Rust LSM-backed storage engine. Fjall owns durability for entity bytes, representation data, and edge records. TronDB owns everything above: indexing, routing, the WAL streaming layer, and inference.

### Five index types

| Index | Structure | Query shape |
|-------|-----------|-------------|
| Location Table | DashMap (RAM) | Where is entity X? What tier, what node, what state? |
| HNSW | Graph (dense vectors, hnsw_rs) | What is semantically similar to X? |
| Sparse index | Inverted index (DashMap) | What matches these SPLADE sparse token weights? |
| Field index | Fjall LSM key space | Entities where field = value, or range (>, <, >=, <=) |
| Adjacency index | DashMap + backward index | Who is connected to X via this edge type? |

Dense and sparse search run in parallel and merge via reciprocal rank fusion (RRF, k=60) for hybrid queries. Field indexes act as pre-filters before vector search — the planner applies this automatically when a `WHERE` clause is present alongside `NEAR`.

### Write path

```
TQL statement → parse → plan → WAL append → flush + fsync
    → apply to Fjall + Location Table + HNSW + Sparse + Field Index + Adjacency
    → ack
```

Every mutation is WAL-logged before acknowledgement. Crash recovery replays from WAL with CRC32 verification.

### Multi-node distribution

TronDB runs as a single binary with three roles:

| Role | Responsibilities |
|------|-----------------|
| **Primary** | Write authority. WAL origin. Broadcasts to replicas. |
| **Replica** | WAL-streaming read replica. Forwards writes to primary. |
| **Router** | Stateless query router. Location Table replica. Scatter-gather for SEARCH. |

```bash
# Single-node (CLI)
cargo run -p trondb-cli

# Cluster
cargo run -p trondb-server -- --config cluster.toml

# Or via environment
TRONDB_ROLE=primary TRONDB_BIND_ADDR=0.0.0.0:9400 cargo run -p trondb-server
```

Docker Compose for a 3-node cluster (primary + replica + router) is included.

### Routing intelligence

The router is stateless and horizontally scalable. Three levels operate simultaneously:

- **Location-aware** — consults a local Location Table replica (RAM, sub-microsecond) to find the owning node.
- **Load-aware** — each node pushes health signals. The router routes away from degraded nodes when a replica exists.
- **Semantic** — understands query verb and entity affinity. SEARCH prefers low HNSW p99. TRAVERSE prefers low queue depth.

```
routing_score = health_score * 0.40 + verb_fit * 0.30 + affinity_score * 0.30
```

### WAL format

MessagePack encoding. 16+ record types cover the full mutation surface. Semi-synchronous replication — writes are acknowledged after configurable replica ack count with timeout.

### HNSW topology stability

The HNSW graph topology is never modified under memory pressure. When a hot-tier entity is demoted to warm, its position in the HNSW graph is preserved via tombstone. Recall stays stable regardless of tier pressure.

---

## Current Status

### What's built (Phases 1-10)

| Phase | Deliverable | Status |
|-------|------------|--------|
| PoC | TQL parser, in-memory store, HNSW index, CLI REPL | Done |
| 2 | Fjall persistence, WAL (MessagePack, CRC32, crash recovery) | Done |
| 3 | Location Table (DashMap, WAL-logged, snapshotted) | Done |
| 4 | HNSW vector index (hnsw_rs, snapshot + incremental catch-up) | Done |
| 5 | Structural edges, TRAVERSE, field indexes, sparse vectors, hybrid SEARCH | Done |
| 6 | Tiered storage (Int8/Binary quantisation, TierMigrator, promotion on access) | Done |
| 7 | Edge decay (exponential/linear/step), routing intelligence (health, co-location, semantic) | Done |
| 8 | UPDATE mutations, HNSW persistence, WAL replay for all record types | Done |
| 9 | Multi-node (gRPC, WAL streaming, write forwarding, scatter-gather, Docker) | Done |
| 10 | Pluggable vectoriser (ONNX/Network/External), auto-vectorise INSERT, mutation cascade, natural language SEARCH | Done |

### Planned (Phases 11-15)

| Phase | Deliverable |
|-------|-------------|
| 11 | INFER verb + inferred edges + admission gate + confidence lifecycle |
| 12 | CONFIRM verb + inference triggers + candidate generation strategies |
| 13 | Advanced query features (JOINs, ORDER BY, advanced WHERE, query hints) |
| 14 | ACU cost model + bi-temporal model |
| 15 | Advanced compression (RaBitQ, MRL, Float16/BF16, PQ) |

---

## Quick Start

```bash
# Build
cargo build --workspace

# Run tests
cargo test --workspace

# Start the REPL
cargo run -p trondb-cli

# Start with a custom data directory
cargo run -p trondb-cli -- --data-dir /path/to/data
```

### TQL examples

```sql
-- Create a collection
CREATE COLLECTION venues (
    FIELD name TEXT,
    FIELD city TEXT,
    REPRESENTATION identity DIMENSIONS 384,
    INDEX idx_city ON (city),
);

-- Insert an entity
INSERT INTO venues (id, name, city) VALUES ('v1', 'Blue Note', 'New York')
    REPRESENTATION identity VECTOR [0.1, 0.2, 0.3, ...];

-- Fetch by field
FETCH * FROM venues WHERE city = 'New York';

-- Semantic search
SEARCH venues NEAR VECTOR [0.1, 0.2, 0.3, ...] LIMIT 10;

-- Create an edge type
CREATE EDGE TYPE 'performs_at' FROM acts TO venues;

-- Insert an edge
INSERT EDGE 'performs_at' FROM 'act1' TO 'v1';

-- Traverse
TRAVERSE 'act1' VIA 'performs_at' DEPTH 2;

-- Explain any query
EXPLAIN SEARCH venues NEAR VECTOR [0.1, 0.2, ...] LIMIT 10;
```

---

## Glossary

| Term | Meaning |
|------|---------|
| ACU | Abstract Cost Unit. Planner's measure of query cost. Baseline 1.0 = hot-tier FETCH by ID. (Planned) |
| Adjacency index | DashMap keyed by (EntityId, EdgeType) for O(1) TRAVERSE lookups. RAM-resident. |
| Confidence | Ordinal ranking signal 0.0-1.0 on every edge. Structural = 1.0. |
| Control Fabric | RAM-resident layer: Location Table, HNSW topology, adjacency index. Never mixed with data bytes. |
| Co-location | Pinning related entities to the same hot node via COLLOCATE WITH or learned affinity. |
| Data Fabric | Entity bytes, vectors, raw edges. Lives on whatever tier the Location Table says. |
| Decay | Confidence reduction over time for edges. Configurable per edge type (exponential/linear/step). |
| Dirty representation | A representation whose source fields changed and recomputation is pending. Excluded from SEARCH. |
| Fjall | LSM-backed Rust storage library. TronDB's durability primitive. |
| HNSW | Hierarchical Navigable Small World. Dense vector index for SEARCH. RAM-resident on hot tier. |
| Location Table | RAM-resident DashMap holding tier, node address, and state for every representation. |
| Recipe hash | SHA-256 of model ID + field names. Detects when a representation's configuration has changed. |
| Representation | A vector encoding of an entity. Multiple per entity (dense, sparse, composite). |
| SPLADE | Sparse Lexical and Expansion model. Produces sparse vectors for term-importance matching. |
| TQL | TronDB Query Language. SQL-dialect with graph extensions. |
| Vectoriser | Pluggable component that generates vectors from entity fields. Local ONNX, network, or external API. |
| WAL | Write-Ahead Log. Every mutation logged before ack. Basis for replication and recovery. |
