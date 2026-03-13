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

## What TronDB Is (Today)

TronDB is an inference-first storage engine: vector + graph + confidence/decay + explainable planning.

It is not trying to be a general-purpose relational OLTP database or an OLAP analytics engine. The boundaries below are intentional scope choices, not permanent limitations.

> **The gap TronDB occupies:** Existing vector databases treat vectors as the retrieval primitive. Existing graph databases treat edges as the retrieval primitive. TronDB treats inferred relationships — probabilistic, durable, decaying, explainable — as the retrieval primitive. No production system currently combines all of: typed durable edges, confidence lifecycle (decay/reinforcement/promotion), vector-native planning, and explicit inference provenance in a single engine.

---

### System Comparison

| System | What it's great at | Where TronDB overlaps | What TronDB (currently) does not try to be |
|--------|-------------------|----------------------|---------------------------------------------|
| **TronDB** | Entity storage + semantic search (dense + sparse + hybrid), typed edges, confidence/decay lifecycle, tiered storage, explainable inference plans | N/A | Not a full SQL RDBMS; not an OLAP engine; not a pure vector index; not a managed cloud service |
| **Postgres + pgvector** | OLTP system-of-record: transactions, constraints, joins, full SQL ecosystem, mature tooling | Structured field storage + filtered retrieval; vector similarity as an add-on via pgvector | TronDB is not targeting the full relational feature set (joins, constraints, schemas-as-truth) as its core value. pgvector is bolted on; TronDB's vector layer is foundational. |
| **DuckDB** | Fast local analytics (OLAP): columnar scans, aggregations, Parquet/Arrow workflows, SQL-everything | Some query planner concepts | TronDB is not optimised for large analytical scans or aggregations. It is retrieval and inference oriented, not scan oriented. |
| **Elasticsearch / OpenSearch** | Search-engine workloads: inverted index ranking, hybrid lexical + semantic retrieval, distributed search ops, document indexing at scale | Hybrid retrieval (dense + sparse) and explainability concepts | TronDB is not a document search engine. Its primary abstraction is typed entities and durable edges with WAL replication — not document indexing and relevance ranking. |
| **Pinecone** | Managed vector similarity search at scale with metadata filtering, minimal operational overhead | Approximate nearest-neighbour semantic retrieval | TronDB is not API-only managed vector infrastructure. It targets native inference and graph semantics inside the storage engine, not vector retrieval as the product. |
| **Qdrant / Milvus** | Self-hosted vector DBs for high-throughput ANN search with payload filtering and collections | Vector search + field filtering | TronDB does not focus solely on vector retrieval. It treats edges, confidence lifecycle, and inference as first-class concerns — not payload attributes on a vector record. |
| **Weaviate** | App-facing vector DB with schema, optional built-in vectorisation modules, and GraphQL interface | Schema + vectors; some representation overlap | The meaningful distinction: Weaviate's inference is query-time only. TronDB persists inferred relationships as durable first-class edges with their own lifecycle — decay, reinforcement, promotion, pruning. That architectural difference is fundamental. |
| **Neo4j** | Property-graph DB: deep traversals, graph algorithms, Cypher query ecosystem, graph analytics | Typed edges + traversal | TronDB is not primarily a graph analytics platform. It does not have Cypher, graph algorithm libraries, or analytical graph processing. Its graph layer exists in service of probabilistic inference, not as the product itself. |
| **TigerGraph** | High-performance graph analytics: multi-hop traversal at scale, GSQL pattern matching, graph ML pipelines, complex connected-data reasoning | Multi-hop traversal concepts; inference over connected data | TigerGraph is the most direct comparison on the reasoning side. The difference: TigerGraph's edges are deterministic facts. TronDB's inferred edges are probabilistic, durable, and have a confidence lifecycle. TigerGraph scales graph algorithms; TronDB scales inferred relationship management. |
| **Vespa** | Combined vector + structured + ranking in one engine: ANN + BM25 + learning-to-rank + real-time updates at scale | Hybrid retrieval (dense + lexical); field filtering before vector search; real-time indexing | Vespa is the closest existing system in retrieval architecture. The gap: Vespa has no graph layer, no typed edges, no confidence/decay lifecycle, and no inference provenance. It is a retrieval engine; TronDB is a storage engine with inference semantics. |
| **TerminusDB / Stardog** | Knowledge graph + semantic reasoning: RDF/OWL ontologies, SPARQL, formal logical inference, schema-level constraints | Knowledge graph concepts; typed relationships; reasoning over connected data | TronDB does not do formal logical inference (OWL, description logic, SPARQL). Its inference is probabilistic and vector-driven, not rule-driven ontological reasoning. Different philosophical model. |
| **LanceDB** | Embedded vector DB with columnar storage (Lance format), tiered storage concepts, serverless-friendly design | Tiered storage architecture; vector + field indexing | LanceDB is an embedded library oriented around ANN retrieval. TronDB is a server-side storage engine with WAL replication, typed edges, and an inference lifecycle. Different deployment model and abstraction level. |
| **FAISS** | In-process ANN search library: maximum control over indexing strategy, GPU acceleration, research-grade ANN algorithms | ANN indexing concepts | TronDB is a database with durability, WAL, replication, and a query planner — not a library you embed and build everything around. Different abstraction level entirely. |
| **Redis + vector search** | Low-latency cache with simple secondary indexes and basic vector search via RediSearch/Redis Stack | Fast retrieval patterns; field filtering | TronDB is not primarily a cache. It is durable storage with tiering, replication semantics, and inference lifecycle — not ephemeral hot data in front of a real database. |

---

### What Makes TronDB Architecturally Distinct

Across all comparisons, four properties distinguish TronDB from every system above. Individually some are present elsewhere — no production system combines all four:

- **Durable inferred edges** — inferred relationships are not query-time computations. They are first-class durable objects stored in Fjall, WAL-logged, and replicated. They persist across restarts.
- **Confidence lifecycle** — every inferred edge has a confidence score with configurable decay, reinforcement, promotion thresholds, and pruning. The engine manages this lifecycle automatically.
- **Candidate admission gate** — inference generates candidates, not edges. Candidates pass through a four-check admission gate before becoming durable. This prevents edge explosion without sacrificing recall.
- **Explainable inference provenance** — `EXPLAIN` on any `INFER` query returns the full reasoning chain: signals consulted, families combined, routing decisions, confidence at each step. Not a black box.

---

### Quick Decision Model

| If you need... | Reach for... |
|----------------|-------------|
| Relational correctness, joins, constraints, transactions | Postgres |
| Analytics on large tables or files | DuckDB |
| Search-engine ranking and document retrieval | Elasticsearch / OpenSearch |
| Pure vector retrieval as a managed product | Pinecone / Qdrant / Milvus |
| Hybrid retrieval at scale (dense + structured + ranking) | Vespa |
| Deep graph traversal and graph algorithms | Neo4j / TigerGraph |
| Formal ontological reasoning (OWL, SPARQL) | Stardog / TerminusDB |
| Semantic retrieval + graph traversal + confidence/decay/inference lifecycle | **TronDB** |

> **Where TronDB fits in a real stack:** TronDB is not a replacement for Postgres or Redis — it sits alongside them. Postgres remains the system-of-record for operational data. Redis remains the cache layer. TronDB is the reasoning layer: it answers questions that require semantic similarity, graph traversal, and probabilistic inference together, with full explainability.
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
| `FETCH` | Get entities by ID or field value. Deterministic. Fast. Supports ORDER BY, advanced WHERE (IN, LIKE, IS NULL, NOT). |
| `SEARCH` | Find entities by semantic similarity — explicit vector, sparse vector, or natural language text. Returns ranked results with similarity scores. |
| `TRAVERSE` | Walk the graph. BFS multi-hop (depth cap 10) with cycle detection. MATCH pattern syntax for directed/undirected edges and depth ranges. |
| `JOIN` | Cross-collection queries via structural (field match) or probabilistic (edge-based with CONFIDENCE threshold) joins. INNER/LEFT/RIGHT/FULL. |
| `INFER` | Propose new edges from vector similarity. Returns ranked candidates with confidence scores. |
| `CONFIRM` | Promote an inferred edge to confirmed status. |
| `DROP` | Remove collections or edge types with cascading cleanup across all subsystems. |

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

### What's built (Phases 1-11)

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
| 11 | Inference pipeline (INFER verb, CONFIRM verb, EdgeSource classification, InferenceSweeper, DecaySweeper, audit buffer) | Done |
| 12 | Query language completions (advanced WHERE, ORDER BY, DROP, query hints) | Done |
| 12b | JOINs (structural + probabilistic) and TRAVERSE MATCH (Cypher-inspired pattern syntax) | Done |
| 13 | Planner & cost model (ACU cost units, CostProvider, PlanWarning, 5 optimisation rules, two-pass strategy) | Done |
| 14 | Bi-temporal model (valid time, transaction time, vector time, temporal queries) | Done |
| 15 | Operational excellence (UPSERT, CHECKPOINT, metrics, slow query log, backup/restore, schema migration, bulk import, benchmarks) | Done |

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

-- Traverse (legacy)
TRAVERSE 'act1' VIA 'performs_at' DEPTH 2;

-- Traverse with pattern matching
TRAVERSE FROM 'act1' MATCH (a)-[e:performs_at]->(b) DEPTH 1..3 CONFIDENCE > 0.5;

-- Join across collections
FETCH a.name, v.address FROM acts AS a
    INNER JOIN venues AS v ON a.id = v.act_id;

-- Probabilistic join (edge-based with confidence)
FETCH a.name, v.name, _edge.confidence FROM acts AS a
    INNER JOIN venues AS v ON a.id = v.id CONFIDENCE > 0.75;

-- Advanced WHERE
FETCH * FROM venues WHERE category IN ('music', 'comedy') ORDER BY name ASC LIMIT 20;
FETCH * FROM venues WHERE name LIKE 'Jazz%' AND city IS NOT NULL;

-- Query hints
FETCH /*+ FORCE_FULL_SCAN */ * FROM venues WHERE city = 'London';
SEARCH /*+ NO_PREFILTER */ venues NEAR 'jazz' LIMIT 10;

-- Drop with cascading cleanup
DROP COLLECTION venues;
DROP EDGE TYPE 'performs_at';

-- Temporal queries (Phase 14)
INSERT INTO venues (id, name) VALUES ('v2', 'Jazz Cafe')
    VALID FROM '2025-01-01T00:00:00Z' TO '2025-12-31T00:00:00Z';
FETCH * FROM venues AS OF '2025-06-01T00:00:00Z' WHERE city = 'London';
FETCH * FROM venues VALID DURING '2025-01-01'..'2025-06-30';
FETCH * FROM venues AS OF TRANSACTION 42891;
TRAVERSE FROM 'act1' MATCH (a)-[e:performs_at]->(b) DEPTH 1..3
    AS OF '2025-06-01T00:00:00Z';

-- Operational commands (Phase 15)
INSERT OR UPDATE INTO venues (id, name) VALUES ('v1', 'Updated Name');
CHECKPOINT;
BACKUP TO '/backups/2025-06-01';
ALTER COLLECTION venues RENAME FIELD city TO location;
ALTER COLLECTION venues DROP FIELD old_field;
IMPORT INTO venues FROM '/data/venues.jsonl';

-- Explain any query (now includes ACU cost breakdown)
EXPLAIN SEARCH venues NEAR VECTOR [0.1, 0.2, ...] LIMIT 10;
```

---

## Glossary

| Term | Meaning |
|------|---------|
| ACU | Abstract Cost Unit. Planner's measure of query cost. Baseline 1.0 = hot-tier FETCH by ID. Shown in EXPLAIN output. |
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
