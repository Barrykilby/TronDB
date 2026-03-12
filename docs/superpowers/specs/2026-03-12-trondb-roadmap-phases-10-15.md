# TronDB Roadmap — Phases 10–15

**Status**: Approved — verbal walkthrough complete 2026-03-12
**Date**: 2026-03-12
**Context**: Post-Phase 9 (Multi-Node Distribution). Phases PoC–9 complete and merged to master.
**Purpose**: Record what remains to be built, why, and in what order — with enough signal that a cold session can execute each phase without re-reading the original design docs.

---

## 1. What Exists (Phases PoC–9)

| Phase | Deliverable | Status |
|-------|------------|--------|
| PoC | TQL parser (logos + recursive descent), in-memory store, HNSW index, CLI REPL | Done |
| 2 | Fjall-backed persistence, WAL (MessagePack, CRC32, segment files) | Done |
| 3 | Location Table (DashMap, WAL-logged, snapshotted, state machine) | Done |
| 4 | Field indexes (Fjall-backed, sortable encoding, compound/partial) | Done |
| 5 | Structural edges, TRAVERSE (BFS, depth cap 10, cycle detection), edge types | Done |
| 5a | Sparse vectors (inverted index), hybrid SEARCH (RRF merge) | Done |
| 6 | Tiered storage — warm (Int8) and archive (Binary), TierMigrator, promotion on access | Done |
| 7a | Edge confidence decay (exponential/linear/step), DecaySweeper | Done |
| 7b | Routing intelligence — health signals, co-location (AffinityIndex), semantic router | Done |
| 8 | UPDATE mutations, planner improvements, ScalarPreFilter | Done |
| 9 | Multi-node distribution — gRPC (tonic), WAL streaming, write forwarding, scatter-gather SEARCH, Docker deployment | Done |

## 2. What Was Skipped

The v4 design doc specified a build order. We diverged. The following core features were specified but not built:

| Feature | Specified In | Why It Matters |
|---------|-------------|----------------|
| **Pluggable Vectoriser** | v3 §10, v4 §2.1 | Without it, TronDB is a passthrough vector store. The engine can't generate embeddings from fields. No composite vectors, no mutation cascade, no natural language SEARCH. This is the core value proposition. |
| **INFER verb** | v4 Phase 6, Grammar §6 | The engine cannot propose edges from vector similarity. TRAVERSE reads what exists; INFER reasons about what might exist. This is the "inference" in inference-compatible. |
| **CONFIRM verb** | Grammar §6.3 | No way to promote engine-proposed edges to application-approved status. |
| **Inferred edge type** | v3 §8, Grammar §6.4 | Edges currently have no source classification (Structural / Inferred / Confirmed). Confidence decay exists but the edge lifecycle doesn't distinguish why an edge exists. |
| **Inference audit** | Grammar §7.3 | No EXPLAIN HISTORY. No ring buffer tracking inference decisions. |
| **Mutation cascade** | v3 §9 | When raw data changes, dependent representations should be marked Dirty and recomputed. recipe_hash exists in the type system but isn't used for staleness detection. |
| **Composite vectors** | v3 §3.4 | Schema-configured multi-field vector amalgamation. The representation type field exists (Atomic/Composite) but composites aren't generated. |
| **JOINs** | Grammar §3 | Structural and probabilistic JOINs with CONFIDENCE clause. Not implemented. |
| **Advanced WHERE** | Grammar §9 | NOT, IS NULL, IN, LIKE operators. Only EQ/GT/LT/GTE/LTE/AND/OR exist. |
| **ORDER BY** | Grammar §3.1 | Not implemented. |
| **Query hints** | Planner §5 | /*+ NO_PROMOTE */ etc. Not implemented. |
| **ACU cost model** | Planner §2 | Abstract Cost Units, CostProvider trait, degraded plan warnings. Not implemented. |
| **Bi-temporal model** | v3 §7, Grammar §3.1 | AS OF, VALID DURING. valid_from/valid_to on entities. Not implemented. |
| **Advanced compression** | v3 §6 | RaBitQ, MRL truncation, Float16/BF16, PQ. Only Int8 and Binary exist. |

---

## 3. Phase 10: Pluggable Vectoriser + Mutation Cascade

### Goal
Give TronDB the ability to generate vectors from entity fields — not just store pre-computed ones. This is the foundation that INFER, composite vectors, natural language SEARCH, and mutation cascade all depend on.

### Why This Is Phase 10
Everything downstream requires the vectoriser. INFER needs vectors to exist to propose edges. Composite vectors need the vectoriser to amalgamate fields. Mutation cascade needs the vectoriser to recompute dirty representations. Natural language SEARCH (`field ~ 'text query'`) needs the vectoriser to embed the query string. Without this phase, Phases 11–15 cannot deliver their full value.

### Design

#### 3.1 Vectoriser Trait (pluggable architecture)

The vectoriser is a trait. Implementations are registered per collection at schema time. The trait boundary must be clean because this area will continue to evolve — new models, new providers, new hardware.

```rust
#[async_trait]
pub trait Vectoriser: Send + Sync {
    /// Unique identifier for this vectoriser instance
    fn id(&self) -> &str;

    /// Model identifier (e.g. "bge-small-en-v1.5", "splade-v3")
    fn model_id(&self) -> &str;

    /// Output dimensions (dense) or vocabulary size (sparse)
    fn output_size(&self) -> usize;

    /// Whether this produces dense or sparse vectors
    fn output_kind(&self) -> VectorKind; // Dense | Sparse

    /// Whether MRL truncation is supported
    fn supports_mrl(&self) -> bool;

    /// Generate a vector from entity fields
    async fn encode(&self, fields: &FieldSet) -> Result<VectorOutput, VectoriserError>;

    /// Batch encode for throughput
    async fn encode_batch(&self, batch: &[FieldSet]) -> Result<Vec<VectorOutput>, VectoriserError>;

    /// Encode a query string (for SEARCH ~ 'text')
    async fn encode_query(&self, query: &str) -> Result<VectorOutput, VectoriserError>;

    /// Optional: incremental update (delta encoding)
    async fn encode_delta(&self, prev: &VectorOutput, delta: &FieldDelta)
        -> Result<Option<VectorOutput>, VectoriserError> {
        Ok(None) // default: full recompute
    }
}

pub enum VectorKind { Dense, Sparse }

pub enum VectorOutput {
    Dense(Vec<f64>),
    Sparse(Vec<(u32, f32)>),
}
```

#### 3.2 Built-in Vectoriser Types (Three-Tier Modular Architecture)

The vectoriser system uses a three-tier modular architecture. All tiers implement the same `Vectoriser` trait — the interface contract is identical regardless of where computation happens. The tiers differ in deployment model and auth requirements:

**Tier 1 — Local (no auth):**
| Type | Description | Device |
|------|------------|--------|
| **OnnxDenseVectoriser** | Local ONNX model for dense embeddings (BGE, jina, nomic, etc.). Primary path for self-hosted. | CPU or GPU (configurable) |
| **OnnxSparseVectoriser** | Local ONNX model for sparse embeddings (SPLADE, etc.). Same ONNX runtime, different output shape. | CPU or GPU (configurable) |
| **PassthroughVectoriser** | Accepts pre-computed vectors from the client at INSERT time. Formalises what exists today. No encode_query support (returns error). | N/A |

**Tier 2 — Network (local cluster, load balanced):**
| Type | Description | Device |
|------|------------|--------|
| **NetworkVectoriser** | Routes encode requests to ONNX model servers on the local network. Load balanced, no external auth. For clusters where GPU nodes are separate from storage nodes. | Remote (LAN) |

**Tier 3 — External (third-party, with auth):**
| Type | Description | Device |
|------|------------|--------|
| **ExternalVectoriser** | HTTP/gRPC hook to external vectorisation providers (OpenAI, Cohere, Voyage, etc.). Configurable endpoint, auth (API key / OAuth), timeout, retry. | Remote (WAN) |

#### 3.3 Device Configuration

The ONNX vectorisers need to know where to run. This is specified at collection creation time and stored in the schema.

```sql
CREATE COLLECTION venues (
    MODEL = 'bge-small-en-v1.5',
    MODEL_PATH = '/models/bge-small-en-v1.5.onnx',
    DEVICE = 'cpu',                    -- cpu | cuda | metal | external
    DIMENSIONS = 384,
    -- fields...
);
```

For Tier 2 (network cluster):
```sql
CREATE COLLECTION venues (
    MODEL = 'bge-small-en-v1.5',
    VECTORISER = 'network',
    ENDPOINT = 'http://gpu-cluster.local:8080/encode',
    DIMENSIONS = 384,
    -- fields...
);
```

For Tier 3 (external provider):
```sql
CREATE COLLECTION venues (
    MODEL = 'text-embedding-3-small',
    VECTORISER = 'external',
    ENDPOINT = 'https://api.openai.com/v1/embeddings',
    AUTH = 'env:OPENAI_API_KEY',           -- environment variable reference
    DIMENSIONS = 1536,
    -- fields...
);
```

The ONNX runtime crate (`ort`) already supports CPU, CUDA, CoreML, and TensorRT execution providers. The vectoriser wraps this with TronDB's async model.

#### 3.4 Composite Vectors

A composite vector encodes the joint meaning of multiple fields. Field inclusion is configured at schema time per representation.

```sql
CREATE COLLECTION events (
    MODEL = 'bge-small-en-v1.5',
    MODEL_PATH = '/models/bge-small-en-v1.5.onnx',
    DIMENSIONS = 384,

    FIELD name TEXT,
    FIELD description TEXT,
    FIELD category TEXT,
    FIELD start_date TIMESTAMP,

    -- Atomic representation: single field
    REPRESENTATION name_embed VECTOR MODEL 'bge-small-en-v1.5' FIELDS (name),

    -- Composite representation: multiple fields amalgamated
    REPRESENTATION semantic VECTOR MODEL 'bge-small-en-v1.5' FIELDS (name, description, category),

    -- Sparse representation
    REPRESENTATION sparse_embed SPARSE MODEL 'splade-v3' FIELDS (name, description),
);
```

The vectoriser concatenates the field values (with field-name prefixes for disambiguation), encodes the combined text, and produces one vector. The recipe_hash is computed from the field configuration — if fields change, the hash changes and the representation is marked Dirty.

#### 3.5 Mutation Cascade

When an entity's fields change (via UPDATE), dependent representations must be recomputed:

1. **UPDATE commits** — WAL + Fjall, synchronous
2. **Staleness check** — For each representation on the entity, compare the current field values against the recipe_hash. If any contributing field changed, mark the representation Dirty (REPR_DIRTY WAL record, 0x21)
3. **Background recomputation** — Tokio task detects Dirty representations, calls the vectoriser's encode() with the current field values, writes the new vector (REPR_WRITE WAL record, 0x20), updates Location Table state to Clean
4. **SEARCH exclusion** — Dirty representations are excluded from SEARCH results. The entity is still queryable via FETCH on structural fields.
5. **Priority** — Recomputation priority is derived from entity hotness (access frequency). Hot entities recompute first.

The cascade terminates naturally: composite representations have no downstream dependents beyond the HNSW index.

#### 3.6 Natural Language SEARCH

With a vectoriser in-engine, SEARCH can accept text queries:

```sql
-- Current syntax (passthrough vector)
SEARCH venues NEAR VECTOR [0.12, -0.34, ...] LIMIT 10;

-- New syntax (natural language — vectoriser encodes the query)
SEARCH venues NEAR 'live jazz venues in Bristol' LIMIT 10;

-- Using a specific representation
SEARCH venues NEAR 'live jazz venues' USING semantic LIMIT 10;
```

The parser detects a string literal after NEAR (vs a vector literal) and the executor calls encode_query() on the collection's vectoriser before passing the resulting vector to HNSW.

#### 3.7 New Crate

`crates/trondb-vectoriser/` — Contains the Vectoriser trait, ONNX implementation, Passthrough, External provider. Depends on `ort` (ONNX Runtime), `reqwest` (external HTTP), `trondb-core` (types only).

#### 3.8 Key Dependencies

| Crate | Purpose |
|-------|---------|
| `ort` | ONNX Runtime Rust bindings (CPU, CUDA, CoreML) |
| `tokenizers` | HuggingFace tokenizers for text preprocessing |
| `reqwest` | HTTP client for external vectorisation endpoints |

#### 3.9 What This Phase Delivers

- INSERT can accept raw field data and the engine generates vectors automatically
- INSERT can still accept pre-computed vectors (Passthrough — backwards compatible)
- UPDATE triggers cascade recomputation of affected representations
- SEARCH accepts natural language queries
- Composite vectors work
- The architecture is pluggable — new vectoriser types slot in without engine changes
- Device selection (CPU/GPU) per collection

---

## 4. Phase 11: Inference Pipeline

### Goal
Give TronDB the ability to reason about relationships — propose edges that don't yet exist, based on vector similarity and graph topology.

### Why This Is Phase 11
INFER depends on vectors existing (Phase 10). It's the second half of the "inference-compatible" promise: Phase 10 generates vectors, Phase 11 uses them to propose edges.

### What Gets Built

#### 4.1 Inferred Edge Type
Edges gain a `source` field:
- **Structural** — Application assertion. Confidence 1.0 (implicit). Permanent.
- **Inferred** — Engine proposal. Confidence 0.0–1.0. Subject to decay and pruning.
- **Confirmed** — Inferred, then application-approved via CONFIRM. Excluded from automatic pruning.

This was specified in v3 §8 and Grammar §6.4. Edge decay (Phase 7a) already exists — this phase adds the lifecycle classification.

#### 4.2 INFER Verb
```sql
-- Propose top 10 edges of any type
INFER EDGES FROM 'ent_abc123' RETURNING TOP 10;

-- With confidence floor
INFER EDGES FROM 'ent_abc123' RETURNING TOP 10 CONFIDENCE > 0.70;

-- Scoped to specific edge types
INFER EDGES FROM 'ent_abc123' VIA PERFORMED_AT, HEADLINED_BY
    RETURNING TOP 5 CONFIDENCE > 0.80;

-- All edges above threshold
INFER EDGES FROM 'ent_abc123' RETURNING ALL CONFIDENCE > 0.90;
```

Implementation: INFER takes an entity, retrieves its vector(s), performs HNSW search against the same or related collections, filters by edge type scope, scores candidates, and returns proposed edges with confidence scores. Proposed edges are NOT written to the graph — they are returned as query results.

#### 4.3 CONFIRM Verb
```sql
CONFIRM EDGE FROM 'ent_abc123' TO 'ent_def456'
    TYPE PERFORMED_AT
    CONFIDENCE 0.95;
```

Promotes an inferred edge to Confirmed status. WAL record: EDGE_CONFIRM (0x33). Confirmed edges are excluded from automatic pruning.

#### 4.4 Background Inference
The engine can run INFER as a background task when new entities or edges are written. This is configurable per edge type — some edge types benefit from automatic inference, others should be caller-initiated only.

#### 4.5 Inference Audit Ring Buffer
Every INFER execution records its reasoning in a RAM-resident ring buffer. Queryable via:
```sql
EXPLAIN HISTORY 'ent_abc123' LIMIT 50;
```

Returns: which entities were considered, what scores they received, which were above threshold, which edge types were evaluated. This is observability for the inference engine.

#### 4.6 Planner Integration
The planner gets an InferPlan type (from Planner spec §3.4). INFER has base ACU 80.0. VIA scope narrowing reduces by 20.0 ACU.

---

## 5. Phase 12: Query Language Completions

### Goal
Fill the gaps between TronDB's current TQL and the grammar specification (Grammar v1).

### What Gets Built

#### 5.1 JOINs
```sql
-- Structural JOIN
FETCH e.name, v.address
    FROM entities AS e
    INNER JOIN venues AS v ON e.venue_id = v.id
    WHERE e.type = 'event';

-- Probabilistic JOIN (across inferred edge, returns confidence)
FETCH e.name, a.name, _edge.confidence
    FROM entities AS e
    INNER JOIN acts AS a ON e.id = a.entity_id CONFIDENCE > 0.75
    WHERE e.type = 'event';
```

Join types: INNER, LEFT, RIGHT, FULL. Probabilistic joins include _edge.confidence in results and filter below threshold.

#### 5.2 ORDER BY
```sql
FETCH name, created_at FROM events ORDER BY created_at DESC LIMIT 20;
```

#### 5.3 Advanced WHERE Operators
```sql
WHERE NOT (type = 'archived')
WHERE name IS NOT NULL
WHERE category IN ('music', 'theatre', 'comedy')
WHERE name LIKE 'Jazz%'
```

#### 5.4 DROP Statements
```sql
DROP COLLECTION venues;
DROP EDGE TYPE PERFORMED_AT;
```

Cascading cleanup: all entities, edges, indexes, HNSW entries, tiered storage for the collection.

#### 5.5 Query Hints
```sql
FETCH /*+ NO_PROMOTE */ * FROM entities WHERE id = 'ent_abc123';
SEARCH /*+ NO_PREFILTER MAX_ACU(200) */ venues NEAR 'jazz' LIMIT 10;
```

Hints are advisory. The planner honours them unless doing so would produce incorrect results.

#### 5.6 TRAVERSE MATCH Syntax
Upgrade TRAVERSE from the current simplified BFS to the Cypher-inspired MATCH pattern from the grammar spec:
```sql
TRAVERSE FROM 'ent_abc123'
    MATCH (a)-[e:RELATED_TO]->(b)
    DEPTH 1..3
    CONFIDENCE > 0.70;
```

Directed, undirected, and typed edge patterns. Variable depth ranges.

---

## 6. Phase 13: Planner & Cost Model

### Goal
Replace the current rule-based planner with the ACU cost model from the Planner spec. Enable the engine to make intelligent plan selection, warn about degraded plans, and surface costs via EXPLAIN.

### What Gets Built

#### 6.1 ACU Cost Model
Abstract Cost Units as defined in Planner spec §2. Base costs calibrated so that a single hot-tier FETCH = 1.0 ACU.

#### 6.2 CostProvider Trait
```rust
pub trait CostProvider: Send + Sync {
    fn fetch_hot_acu(&self) -> f64 { 1.0 }
    fn fetch_warm_acu(&self) -> f64 { 25.0 }
    fn search_hnsw_acu(&self) -> f64 { 50.0 }
    // ... per Planner spec §2.4
}
```

Phase 13 ships ConstantCostProvider (all defaults). Statistics hooks are built into the trait for future runtime-stats-backed implementation.

#### 6.3 PlanWarning System
Degraded plans emit warnings with severity, ACU breakdown, and suggestions. The engine always tries to give an answer — it tells you the answer was expensive and how to fix it, but it doesn't silently fail.

#### 6.4 Optimisation Rules
Five rules, all enabled by default, all individually disableable via config or query hints:
- **ScalarPreFilter** — already exists, gets ACU integration
- **ConfidencePushdown** — push CONFIDENCE floor into HNSW ef_search
- **TraverseHopReorder** — cheap (structural) edges first, prune before expensive (inferred) hops
- **OnDemandPromotion** — promote warm entities to hot on access (with memory pressure suppression)
- **BatchedFetchAfterSearch** — group post-SEARCH FETCHes by owning node

#### 6.5 Two-Pass Query Strategy
For large candidate sets: first pass uses binary/Int8 representations for fast shortlisting, second pass rescores survivors with full-precision vectors. Planner selects single-pass vs two-pass based on candidate set size and tier distribution.

---

## 7. Phase 14: Bi-Temporal Model

### Goal
Implement the three time axes from v3 §7: Valid Time (when the fact was true), Transaction Time (when TronDB learned it), Vector Time (when the embedding was computed).

### What Gets Built

#### 7.1 Entity Temporality
- valid_from / valid_to on entities (application-controlled)
- tx_time on entities (engine-controlled, append-only)
- Representation computed_at + model_version (engine-controlled)

#### 7.2 Query Modifiers
```sql
-- What did TronDB know at this transaction time?
FETCH * FROM entities AS OF '2025-01-01T00:00:00Z' WHERE id = 'ent_abc123';

-- What was true during this world-time window?
FETCH * FROM entities VALID DURING '2025-01-01'..'2025-06-30' WHERE type = 'event';

-- Transaction time (WAL-based)
FETCH * FROM entities AS OF TRANSACTION 42891;
```

#### 7.3 Edge Temporality
Edges carry valid_from/valid_to independently of the entities they connect. TRAVERSE supports temporal filters:
```sql
TRAVERSE FROM 'ent_abc123'
    MATCH (a)-[:RELATED_TO]->(b)
    AS OF '2025-06-01T00:00:00Z';
```

#### 7.4 Resolution
Temporal queries resolve through the Location Table's valid_from/valid_to fields without touching raw entity data, making them cheap.

---

## 8. Phase 15: Operational Excellence + Security + Performance

### Goal
Production-readiness. Everything needed to deploy TronDB in an environment where other people depend on it.

### What Gets Built

#### 8.1 Security
- **Authentication**: mTLS for node-to-node, JWT or API key for client-to-router
- **Per-collection RBAC**: READ, WRITE, ADMIN permissions per collection per principal
- **Field-level access control**: Certain fields restricted to certain roles (important for Clark's identity intelligence use case — v3 §14.3 flagged this explicitly)

#### 8.2 Operational
- **Backup/restore**: Point-in-time snapshot export + restore from snapshot. WAL-based incremental backup.
- **Schema migration**: Field rename, field remove, field type change. recipe_hash invalidation triggers cascade recomputation.
- **UPSERT**: INSERT OR UPDATE semantics
- **Bulk import/export**: Streaming INSERT from external format (JSON lines, Parquet). Streaming FETCH to file.
- **CHECKPOINT admin command**: Manual checkpoint trigger (already in WAL spec as 0xFF)

#### 8.3 Observability
- **Prometheus metrics export**: Entity counts, tier distribution, HNSW latency percentiles, query throughput, WAL lag, replication lag, vectoriser latency
- **Slow query log**: Queries exceeding configurable ACU threshold logged with full EXPLAIN output
- **Distributed tracing**: OpenTelemetry integration for cross-node query tracing

#### 8.4 Performance Review
- **Benchmark suite**: Standard workloads (FETCH throughput, SEARCH recall@k, TRAVERSE fan-out, INSERT sustained rate)
- **Profiling**: CPU + memory profiling under load. HNSW recall validation against ground truth.
- **Stress testing**: Memory pressure scenarios, replica lag under write load, tier migration throughput

#### 8.5 Advanced Compression & Archive Format

**Vector quantization upgrades (warm tier):**
- RaBitQ (SIGMOD 2024, preferred binary method — adopted by LanceDB, Elastic BBQ)
- Extended-RaBitQ (2–4 bits/dim, outperforms PQ at same compression budget)
- MRL truncation (front-loaded semantic signal, needs MRL-capable model)
- Float16/BF16 (near-lossless intermediate)
- PQ with streaming codebook training (Qdrant pattern — no read-only window)

**Archive format — columnar + seekable compression (cold tier):**

This is a research-heavy area. The core idea (from brainstorming session 2026-03-12) is a multi-level narrowing funnel for archive storage, inspired by H3's hierarchical spatial indexing: identify candidate data coarsely without decompression, then selectively decompress only what's needed.

**Concept — multi-level archive blocks:**
```
Archive Block (e.g. 1000 entities, grouped by affinity/co-access)
├── Block Sketch (uncompressed, tiny, always readable)
│   ├── bloom filter: categorical field values
│   ├── min/max: numeric/timestamp field ranges
│   └── centroid: coarse binary vector for the block
│
├── Column Frames (each independently zstd-seekable compressed)
│   ├── frame: id column
│   ├── frame: name column
│   ├── frame: category column
│   ├── frame: repr_0 vectors (quantized)
│   └── frame: repr_1 vectors (quantized)
│
└── Seek Table (frame offsets → zstd seekable format)
```

**Key properties:**
- **Level 1 (sketch scan)**: Query block sketches to eliminate non-candidate blocks. Zero decompression.
- **Level 2 (block selection)**: Only candidate blocks proceed. Non-candidates are never touched.
- **Level 3 (column-selective decompression)**: Within a candidate block, decompress only the columns needed. A `FETCH WHERE category = 'music'` decompresses only the category column, not vectors or other fields.
- **Columnar compression**: Homogeneous data (all f64 vectors together, all strings together) compresses dramatically better than row-oriented storage.
- **Affinity groups as block boundaries**: Phase 7b's AffinityIndex already identifies co-accessed entities — these are natural grouping boundaries for archive blocks.
- **INFER on archive becomes plausible**: Scan block centroids → decompress only vector columns of candidate blocks → score → return proposals without promoting entities to hot.

**Technology:**
- zstd seekable format (official spec: independent frames + seek table, compatible with standard zstd)
- Rust: `zstd-framed` crate (async support, `.with_seek_table()` API)
- Frame boundaries aligned to TronDB's entity structure (semantic framing, not arbitrary byte offsets)

**Research needed before implementation:**
- Optimal block size (entity count per block) — trade-off between sketch granularity and compression ratio
- Sketch design — bloom filter false positive rates, centroid quantization level
- Whether zstd dictionary training per-block improves ratios for small blocks
- Compression ratio vs decompression latency benchmarks for TronDB's actual data shapes
- Comparison: zstd seekable vs LZMA for archive-class compression (zstd likely wins on decompression speed; LZMA on ratio)
- How this interacts with the two-pass query strategy from the Planner spec

---

## 9. Reference: Original Design Documents

| Document | Location | Authority |
|----------|----------|-----------|
| **Design v4** (authoritative) | `~/Downloads/files_unzipped/trondb_design_v4.docx` | Architecture, distribution, tier topology, build order |
| **Grammar v1** | `~/Downloads/files_unzipped/trondb_grammar_v1.docx` | TQL EBNF — FETCH, SEARCH, TRAVERSE, INFER, EXPLAIN, schema, hints |
| **Planner v1** | `~/Downloads/files_unzipped/trondb_planner_v1.docx` | ACU cost model, plan types, optimisation rules, query hints, PlanWarning |
| **WAL v1** | `~/Downloads/files_unzipped/trondb_wal_v1.docx` | Record format, streaming protocol, crash recovery |
| **Routing v1** | `~/Downloads/trondb_routing_v1.docx` | Health signals, co-location, semantic router |
| Design v3 (context only) | `~/Downloads/files_unzipped/trondb_design_v3.docx` | Pre-architecture. Useful for rationale and edge model detail. Not authoritative. |

---

## 10. Phase Dependencies

```
Phase 10 (Vectoriser + Mutation Cascade)
    │
    ├── Phase 11 (Inference Pipeline) ─── depends on vectors existing
    │
    ├── Phase 12 (Query Language) ─────── independent, but JOINs on inferred edges need Phase 11
    │
    └── Phase 13 (Planner & Cost Model) ─ independent, but INFER plan type needs Phase 11

Phase 14 (Bi-Temporal) ──────────────── independent of 10–13, can be reordered

Phase 15 (Ops + Security + Perf) ────── should come last, reviews everything built
```

Phase 10 is the hard dependency. Phases 11–14 have soft dependencies on each other but could be reordered. Phase 15 is always last.

---

## 11. Open Questions

### Resolved (2026-03-12)

1. **~~External vectoriser auth~~** → Three-tier modular architecture: Tier 1 (local ONNX, no auth), Tier 2 (network cluster, no external auth), Tier 3 (external providers, auth via env var references in schema). See §3.2.

2. **~~Build ordering vs Clark~~** → Build in the order TronDB needs. Don't worry about Clark integration ordering. TronDB will only go near real/semi-real data once fully built.

### Still Open

3. **ONNX model distribution**: Should models be bundled with the TronDB binary, downloaded on first use, or referenced by path? The schema currently has MODEL_PATH — is that sufficient?

4. **Composite vector field weighting**: Should fields in a composite vector be equally weighted, or should the schema support per-field weights? (e.g. `FIELDS (name WEIGHT 2.0, description WEIGHT 1.0)`)

5. **Background inference policy**: Should INFER run automatically on entity insert (configurable per edge type), or only when explicitly called?

6. **Bi-temporal priority**: Phase 14 is currently late in the sequence. Is the current ordering right?

7. **MRL/RaBitQ timing**: Advanced compression is parked in Phase 15. Should it move earlier (e.g. into Phase 10 alongside the vectoriser, since MRL-capable models need vectoriser support)?
