# TronDB

[![CI](https://github.com/Barrykilby/TronDB/actions/workflows/ci.yml/badge.svg)](https://github.com/Barrykilby/TronDB/actions/workflows/ci.yml)
[![Benchmark](https://github.com/Barrykilby/TronDB/actions/workflows/benchmark.yml/badge.svg)](https://github.com/Barrykilby/TronDB/actions/workflows/benchmark.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)

**A storage engine for knowledge that has to age, written in Rust.**

Most databases assume a fact stays true until you delete it. Memory does not
work that way. Some of what you store gets less true over time, some gets
reinforced by evidence, and some should quietly stop being retrievable.

TronDB makes that lifecycle part of the schema instead of part of your
application:

```sql
CREATE EDGE visited
  FROM people TO venues
  DECAY EXPONENTIAL RATE 0.05 FLOOR 0.1 PRUNE 0.05;
```

That declaration is the whole point. The engine decays the confidence on those
edges, reinforces them when evidence recurs, promotes them when evidence
accumulates, and prunes them when they fall through the floor. No cron job, no
application-side scoring pass, no hand-tuned constants in three different
codebases.

---

## Why This Exists

Every agent-memory and knowledge-retention system reimplements the same three
things in application code: a confidence score, some notion of decay or
recency, and a way for repeated evidence to reinforce a memory rather than
duplicate it.

This engine exists because that layer had been written by hand three times over
three different stores (an orchestrator keeping transcripts in Postgres, a
retrieval service scoring cards in Redis, a memory graph in DuckDB), and every
version had confidence, access-tracking-as-decay-prevention, and a
reinforcement edge type. Same three concepts, three sets of hand-set weights,
three places to get it wrong.

Retention is a storage concern. It belongs next to the data.

---

## Two Kinds of Knowledge

- **Structural facts.** Things you told it. Confidence 1.0, fixed. Behave
  exactly like a conventional database: deterministic, no probabilistic layer.
- **Inferred facts.** Things the engine derived. Confidence 0.0-1.0, decayed if
  not reinforced, promoted if evidence accumulates, pruned at the floor.

Both are queryable, both filter by confidence threshold. The probabilistic layer
only activates when you ask for it.

---

## Measured Performance

Every number below comes from `cargo bench -p trondb-core --bench engine_bench` on
an Apple M4 Pro (12 core, macOS), rustc 1.88, release profile with LTO. Criterion
reports [low mean high]; the middle figure is quoted. Reproduce with:

```bash
cargo bench -p trondb-core --bench engine_bench
```

### Inference cost

| Operation | Corpus | Latency |
|-----------|--------|---------|
| `INFER EDGES ... RETURNING TOP 10` | 5,000 candidates | 45 µs |
| `SEARCH ... NEAR VECTOR LIMIT 10` (dense HNSW) | 5,000 entities | 320 µs |

**These two are not like-for-like, and the gap is not evidence that inference is
cheaper than retrieval.** They differ in two ways that both favour INFER:

- INFER walks the index with `search_k = limit * 4`, so 40 candidates for a
  TOP 10 (`executor.rs`). SEARCH walks with `fetch_k = k`, so 10. INFER does the
  *wider* walk.
- INFER returns `from_id, to_id, edge_type, confidence` and never touches the
  store. SEARCH hydrates every result out of Fjall. The comparison is ten IDs
  against ten rows, and hydration dominates.

What can be said: generating and ranking ten confidence-scored candidate edges
costs 45 µs and needs no store round-trip. Cost scales with candidates rather
than corpus (`TOP 100` is 206 µs), and the confidence gate is free (47.6 µs at
`CONFIDENCE > 0.90`) because it applies during ranking rather than as a second
pass.

What cannot be said from these numbers is that putting inference in the engine
beats doing it above one. That needs a like-for-like harness, and it is
outstanding. See [Known Issues](#known-issues).

### Retrieval

| Operation | Corpus | Latency |
|-----------|--------|---------|
| Indexed field lookup | 100 entities | 25 µs |
| Full scan | 100 entities | 157 µs |
| Dense HNSW search, k=10 | 5,000 × 64-dim | 320 µs |
| Hybrid dense + sparse, RRF, k=10 | 5,000 × 64-dim | 301 µs |
| Dense search with `WHERE` pre-filter, k=10 | 5,000 × 64-dim | 500 µs |

Hybrid is not slower than dense alone: the two passes run in parallel and the
RRF merge is cheap next to the HNSW walk.

Pre-filtering is **slower**, not faster. At this selectivity (`genre = 'jazz'`
is about a quarter of the corpus) the field-index scan costs more than the HNSW
work it saves. It wins at high selectivity and loses at low, and the planner
does not estimate the crossover. Known gap, not a tuned result.

### Graph traversal

| Operation | Graph | Latency |
|-----------|-------|---------|
| `TRAVERSE` depth 1 | 2,000 nodes, 10,000 edges | 70 µs |
| `TRAVERSE` depth 2 | 2,000 nodes, 10,000 edges | 428 µs |
| `TRAVERSE` depth 3 | 2,000 nodes, 10,000 edges | 2.9 ms |

Fan-out is 5, so depth-3 visits much of the graph. Cost tracks nodes visited,
not depth.

### Writes: the known bottleneck

| Operation | Latency |
|-----------|---------|
| Single `INSERT` (entity + dense vector) | 8.7 ms |
| `CONFIRM` (promote inferred edge to durable) | 12.4 ms |

Two to three orders of magnitude slower than the read path, and architectural
rather than incidental: every mutation appends to the WAL and `fsync`s before
acknowledging, with no group commit.

How much that matters depends entirely on the workload. At roughly 115
entities/sec, a 10,000-row backfill takes minutes and a million-row migration is
out of the question. A single memory written per conversation turn is 8.7 ms and
nobody notices. Bulk ingest needs group commit before this engine is usable;
incremental retention does not.

**Proposing a derived edge costs 45 µs. Persisting one costs 12.4 ms.**

### What these numbers are not

- **Not a cross-engine comparison.** Nothing here was run against Postgres,
  Neo4j, Qdrant, LanceDB, or anything else. Published figures from other
  projects were measured on other hardware with other methodology and are not
  comparable to these. The comparison table below describes *scope and intent*,
  not measured performance.
- **Not at scale.** The largest corpus benchmarked is 5,000 vectors at 64
  dimensions. Latency at a million vectors is unmeasured and unclaimed.
- **Not distributed.** All figures are single-node, single-process. Replication,
  scatter-gather, and routing overhead are unmeasured.
- **Not a recall measurement.** These are latency figures only. Retrieval
  quality is measured separately, in the storage-tier table below.

## What TronDB Is (Today)

Vector + graph + a retention lifecycle the engine enforces, in one binary.

Not a relational OLTP database, not an OLAP engine. Those are scope choices,
not limitations.

### The one that matters

**Retention as declarative schema.** Decay curve, floor, prune threshold,
reinforcement and promotion, declared per edge type and enforced by the engine.
Nothing else appears to ship this at engine level. Everything below is in
service of it.

### The rest

- **Durable derived edges.** Not query-time computation. Stored in Fjall,
  WAL-logged, replicated, alive across restarts. Something has to persist for
  a lifecycle to act on.
- **Candidate admission gate.** `INFER` produces candidates, not edges. Four
  checks before anything becomes durable. Stops the graph filling with weak
  links, which is the failure mode of derived-edge systems.
- **Derivation is one signal, not a chain.** `INFER` ranks by vector similarity
  and nothing else: `confidence` is the raw cosine score of the first dense
  representation. There is no rule language and no entailment. If you want
  real logical inference, see TypeDB or RDFox. Naming this engine
  "inference-first" would overstate it.
- **Audit trail.** `EXPLAIN HISTORY` returns per-INFER candidate counts and the
  trigger. Thinner than it should be, and volatile: see
  [Known Issues](#known-issues).

### Closest existing systems

For the lifecycle, the comparison set is agent-memory systems rather than
databases. All of them do retention; all of them do it above the store:

| System | Retention approach | Where it lives |
|--------|--------------------|----------------|
| **Zep / Graphiti** | Temporal knowledge graph, fact validity windows, edge invalidation. Widely rated the strongest decay story. | Application layer |
| **MemoryBank** | Ebbinghaus-style decay; memories fade unless reinforced. Closest in spirit. | Hand-set dynamics in app code |
| **Generative Agents** | recency + importance + relevance blend | Importance is an ad-hoc LLM 1-10 rating with fixed weights, and it governs retrieval only, not forgetting |
| **Letta / MemGPT** | Paged memory with explicit eviction | Eviction driven by recency and capacity pressure, not by a value model |
| **Mem0** | extract → consolidate → retrieve | A pipeline, not storage |

The gap TronDB aims at is the right-hand column, not the middle one. None of
these push retention into the storage layer, so each carries its own scoring
pass and its own tuned constants.

For the derivation half, the comparison set is databases, and it is less
favourable:

| System | Overlap | The gap |
|--------|---------|---------|
| **Neo4j GDS** | `gds.knn.write` computes kNN over node embeddings and writes durable relationships with the similarity as a property. `topK` and `similarityCutoff` bound output; `randomSeed` makes it reproducible. This is most of `INFER`, shipping and mature. | Docs describe no decay, no reinforcement, no automatic pruning. Scores are computed once. A scheduled Cypher job gets you close. **Take this seriously: it is most of the derivation half for free.** |
| **CozoDB** | Transactional relational + graph + vector in one embeddable Rust engine, HNSW indices usable from Datalog, time travel | Datalog gives composable recursive derivation, which a single kNN signal cannot. No retention lifecycle. |
| **Vespa** | Closest on retrieval architecture: ANN + lexical + filtering + ranking in one engine | No graph layer, no typed edges, no lifecycle |

Also relevant: **TypeDB** and **RDFox** for real rule-based inference with
materialised derived facts and explanations, **XTDB** for bitemporal, **Kùzu**
for embedded graph plus vector, **Senzing**/**Splink** for probabilistic entity
linkage.

Everything else is a different job: Postgres for relational correctness, DuckDB
for analytics, Elasticsearch for document ranking, Pinecone/Qdrant/Milvus for
vector retrieval as a product, Stardog/TerminusDB for ontological reasoning,
FAISS for an ANN library.

TronDB sits alongside a system-of-record rather than replacing one.

### Storage tiers

Sizes and compression ratios are measured. Recall is measured: recall@10
against an exact cosine baseline over 5,000 clustered 384-dim vectors, 200
queries. Reproduce with:

```bash
cargo test -p trondb-core --test quantisation_recall -- --nocapture
```

| Tier | Encoding | Size (384-dim) | Compression | Recall@10 | Role |
|------|----------|---------------|-------------|-----------|------|
| Hot | Float32 | 1,536 bytes | 1x | 100% | Actively queried entities. Full HNSW index in memory. |
| Warm | PolarQuant (3-bit) | 153 bytes | 10.0x | **47%** | Recent but less active. First-pass shortlisting only. |
| Warm | PolarQuant (4-bit) | 201 bytes | 7.6x | **55%** | Higher-fidelity alternative for the same tier. |
| Cool | Int8 | 392 bytes | 3.9x | **98%** | Fallback warm tier. Simple min/max scalar quantisation. |
| Archive | Binary | 48 bytes | 32x | **21%** | Rarely touched. Sign-bit quantisation for coarse filtering. |

Data flows between tiers automatically based on access patterns. Entities are promoted on demand mid-query when accessed via FETCH.

Do not infer these from cosine fidelity. A vector can reconstruct at cosine
0.95 and still lose most of its top-10 neighbours to reordering, so a fidelity
assert on one reconstructed vector will read far more optimistically than the
table above. Of the tiers, only int8 lands where fidelity suggests it should.

**Use int8, not PolarQuant, when recall matters.** PolarQuant buys 2.6x more
compression and gives up roughly half the neighbours. Defensible as first-pass
shortlisting ahead of a hot-tier re-rank. Not defensible as a general demotion
target. The tier order above is size, not quality.

**On the name.** What is implemented is Walsh-Hadamard rotation followed by
per-coordinate Lloyd-Max scalar quantisation at 2-4 bits: training-free, and
applies to any embedding model. That is the first stage of TurboQuant
(arXiv:2504.19874). TurboQuant's second stage, a residual QJL bit that removes
the estimator's bias, is **not implemented**, and its absence is a better
explanation of the recall shortfall than anything in
[Known Issues](#known-issues). Note also that PolarQuant (arXiv:2502.02617) is
a *different* algorithm, storing a radius plus quantised angles; the name here
is inherited from an early draft and is wrong.

---

## How It Works

### Vectors and meaning

Every entity carries one or more vectors. Similar entities sit close together,
and the engine uses those distances to find relationships nobody stored
explicitly. An entity can have several representations, each a different lens:

- **Passthrough.** Caller provides pre-computed vectors at INSERT time
- **Managed.** The engine generates vectors from declared entity fields using a pluggable vectoriser (local ONNX model, network model server, or external API)
- **Composite.** Multiple fields amalgamated into a single representation

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

TronDB generates vectors as well as storing them. Three tiers:

| Tier | Type | Auth | Example |
|------|------|------|---------|
| 1 | Local ONNX | None | `MODEL_PATH '/models/bge.onnx'` |
| 2 | Network cluster | None | `VECTORISER 'network' ENDPOINT 'http://gpu-box:8080/embed'` |
| 3 | External API | Required | `VECTORISER 'external' ENDPOINT 'https://api.openai.com/...' AUTH 'env:OPENAI_API_KEY'` |

### Representation validity and mutation cascade

UPDATE marks any representation depending on a changed field as dirty and
queues recomputation. Dirty representations stay fetchable by ID but drop out
of SEARCH until recomputed. Staleness is detected via `recipe_hash`, a SHA-256
of model ID plus field names.

```
UPDATE field → detect affected representations → mark Dirty (WAL-logged)
    → background recompute via vectoriser → write new vector (WAL-logged)
    → transition to Clean → entity re-enters SEARCH results
```

### The two fabrics

Two internal layers that never mix:

- **Control Fabric.** Knows where everything is: tier location, node address, graph topology, edge metadata, confidence scores, representation state. Always RAM-resident via DashMap with a contention-minimised concurrent lookup path. Sub-microsecond access. This is the routing brain.
- **Data Fabric.** Holds actual entity bytes, vectors, and raw edges. Lives wherever the tier says. The control fabric tells you where to look before you look.

The engine never searches for something. It always knows where to go first.

### Edges and confidence

Every edge has a type, a direction, and a confidence score. Types are declared
before use, same discipline as collections: a name and a from/to pair is the
minimum. Decay is optional; without it confidence is fixed.

- **Structural edges** start at confidence 1.0 and stay there.
- **Inferred edges** (planned) start lower and change: reinforced when evidence supports them, decayed when they go stale, pruned when they fall below the floor.

```sql
-- Minimal (structural, no decay)
CREATE EDGE performs_at
  FROM acts TO venues;

-- With decay
CREATE EDGE visited
  FROM people TO venues
  DECAY EXPONENTIAL RATE 0.05 FLOOR 0.1 PRUNE 0.05;
```

### Querying

TronDB has a query language called TQL (TronDB Query Language). Current verbs:

| Verb | What it does |
|------|-------------|
| `FETCH` | Get entities by ID or field value. Deterministic. Fast. Supports ORDER BY, advanced WHERE (IN, LIKE, IS NULL, NOT). |
| `SEARCH` | Find entities by semantic similarity: explicit vector, sparse vector, or natural language text. Returns ranked results with similarity scores. |
| `TRAVERSE` | Walk the graph. BFS multi-hop (depth cap 10) with cycle detection. MATCH pattern syntax for directed/undirected edges and depth ranges. |
| `JOIN` | Cross-collection queries via structural (field match) or probabilistic (edge-based with CONFIDENCE threshold) joins. INNER/LEFT/RIGHT/FULL. |
| `INFER` | Propose new edges from vector similarity. Returns ranked candidates with confidence scores. |
| `CONFIRM` | Promote an inferred edge to confirmed status. |
| `DROP` | Remove collections or edge types with cascading cleanup across all subsystems. |

Natural language queries are encoded through the collection's vectoriser:

```sql
-- Vector search (explicit)
SEARCH venues NEAR VECTOR [0.1, 0.2, ...] LIMIT 10;

-- Natural language search (vectoriser encodes the query)
SEARCH venues NEAR 'live jazz in Bristol' USING semantic LIMIT 10;

-- Hybrid dense + sparse
SEARCH venues NEAR VECTOR [...] NEAR SPARSE [0:0.8, 42:0.3] LIMIT 10;

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

No hand-rolled LSM tree. [Fjall](https://github.com/fjall-rs/fjall) owns
durability for entity bytes, representations, and edge records. TronDB owns
everything above it: indexing, routing, WAL streaming, inference.

### Five index types

| Index | Structure | Query shape |
|-------|-----------|-------------|
| Location Table | DashMap (RAM) | Where is entity X? What tier, what node, what state? |
| HNSW | Graph (dense vectors, hnsw_rs) | What is semantically similar to X? |
| Sparse index | Inverted index (DashMap) | What matches these SPLADE sparse token weights? |
| Field index | Fjall LSM key space | Entities where field = value, or range (>, <, >=, <=) |
| Adjacency index | DashMap + backward index | Who is connected to X via this edge type? |

Dense and sparse search run in parallel and merge via reciprocal rank fusion (RRF, k=60) for hybrid queries. Field indexes act as pre-filters before vector search, and the planner applies this automatically when a `WHERE` clause is present alongside `NEAR`.

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

Stateless and horizontally scalable. Three levels at once:

- **Location-aware.** Consults a local Location Table replica (RAM, sub-microsecond) to find the owning node.
- **Load-aware.** Each node pushes health signals. The router routes away from degraded nodes when a replica exists.
- **Semantic.** Understands query verb and entity affinity. SEARCH prefers low HNSW p99. TRAVERSE prefers low queue depth.

```
routing_score = health_score * 0.40 + verb_fit * 0.30 + affinity_score * 0.30
```

### WAL format

MessagePack encoding. 16+ record types cover the full mutation surface. Semi-synchronous replication: writes are acknowledged after configurable replica ack count with timeout.

### HNSW topology stability

The HNSW graph topology is never modified under memory pressure. When a hot-tier entity is demoted to warm, its position in the graph is preserved via tombstone. The intent is that recall does not move with tier pressure; that has not been measured.

---

## Known Issues

Open defects, each with the measurement that exposed it.

### PolarQuant discards rotated coefficients when dimensions is not a power of two

`quantise_polar` rotates to `padded_dim` (next power of two) then quantises
only the first `dimensions` coefficients. Walsh-Hadamard exists to spread energy
uniformly, so the remainder is discarded. A 384-dim vector pads to 512 and loses
25% of its energy before a bit is quantised.

Reconstruction norm tracks this exactly. Unit-norm input:

|  dims | padded | discarded | recon norm | √(kept) | recall@10 |
|------:|-------:|----------:|-----------:|--------:|----------:|
|   256 |    256 |      0.0% |     0.9839 |  1.0000 |     70.4% |
|   320 |    512 |     37.5% |     0.7137 |  0.7906 |     54.1% |
|   384 |    512 |     25.0% |     0.8300 |  0.8660 |     59.5% |
|   448 |    512 |     12.5% |     0.9179 |  0.9354 |     66.1% |
|   512 |    512 |      0.0% |     0.9825 |  1.0000 |     69.3% |
|   768 |   1024 |     25.0% |     0.8274 |  0.8660 |     59.8% |
|  1024 |   1024 |      0.0% |     0.9824 |  1.0000 |     70.1% |

Norm follows √(kept) to within quantisation error; recall degrades
monotonically with the discarded fraction. Power-of-two dimensions average
69.9% recall@10, padded dimensions 58.6%. The source comment claiming energy
"concentrates in the original dimensions" is the inverse of what the rotation
does.

Every common dimension except 256, 512, 768 and 1024 pays this, including the
384 of `bge-small-en-v1.5`, the model used as this README's worked example.

```bash
cargo test -p trondb-core --test polar_padding_probe -- --nocapture
```

Fixing it does not reach the ~95% originally claimed: 3-bit PolarQuant on an
untruncated dimension measures ~70%. The truncation is ~11 points of the gap,
not all of it.

### `approx_inner_product_raw` loses recall to reconstruction-norm error

Reconstruction does not preserve norm: 0.83 mean against unit-norm input at 384
dims, ±0.054 per-vector spread. Cosine divides that error out per candidate; a
raw inner product cannot, so it ranks partly by reconstruction error. 27%
recall@10 against 47% for cosine over the same reconstructions. It is documented
as the fast path, and it is also the less accurate one. Renormalising the
reconstruction should close the gap.

### HNSW can miss reachable points (fixed, with a caveat)

`hnsw_rs` builds its layer generator with `StdRng::from_os_rng()` and exposes no
seeding API, so the same insertions build a different graph on every run. On a
small index that leaves some points unreachable from the entry point.

Measured over 500 independent index builds, before the fix:

| Index | k | Returned fewer than expected | Worst case |
|-------|---|------------------------------|------------|
| 2 orthogonal points | 2 | 5.4% | 1 of 2 |
| 2 near-identical points | 10 | 5.2% | 1 of 2 |
| 8 points | 8 | 7.6% | **1 of 8** |

Raising effort does not help. At k=8 the miss rate is 5.0% at ef=50 and 6.6% at
ef=800: the points are not reachable at any effort, so this is graph
connectivity rather than search budget.

`HnswIndex::search` now falls back to an exact scan when the walk comes back
short **and** the index holds at most 1,024 live entries. The fallback runs only
after the walk has already failed, so the normal path is unchanged, and a large
index never scans. All three cases above measure 0.0% after the fix. Seven tests
were flaking at roughly 25% of suite runs because of this; all pass now.

**The caveat:** the fallback bounds the damage, it does not fix the cause. Above
1,024 entries an unreachable point stays unreachable, and there it is silent:
no error, just a missing row. Measuring recall at realistic scale is the
outstanding work. Fixing it properly means seeding upstream, vendoring
`hnsw_rs`, or replacing it.

Reproduce:

```bash
cargo test -p trondb-core --test hnsw_determinism_probe -- --nocapture
```

### The central claim is untested

Two claims, neither measured.

**That engine-side retention beats application-side retention.** The argument is
that three hand-written implementations is evidence the layer is in the wrong
place. That is a real observation about developer cost, and it is not a
measurement. The experiment is a retention benchmark (LOCOMO, LongMemEval)
against Zep and Mem0 on the same corpus, plus the number that probably decides
it: tokens re-embedded and re-consolidated per conversation. Published figures
in that field are contested and self-reported, so the harness matters more than
the score.

**That derivation belongs in the engine.** The `INFER` against `SEARCH` figure
above is not like-for-like: `INFER` does a wider index walk and returns IDs,
`SEARCH` does a narrower walk and hydrates rows. The honest comparison is
against `gds.knn.write` plus a scheduled decay job, same corpus, same hardware.
Until it is run, this half is an assertion, and the previous framing of this
project as inference-first oversold it.

### Inference provenance is thinner than it should be, and volatile

`EXPLAIN INFER` returns three fields: mode, verb, from_id. There is no signal
chain, no routing detail, and no per-step confidence, because inference is a
single signal: `confidence` is the raw HNSW cosine similarity of the first dense
representation.

`EXPLAIN HISTORY` is the real audit surface and returns counts only
(`candidates_evaluated`, `candidates_above_threshold`, `trigger`). It reads from
an in-memory ring buffer, so it is lost on restart, and it does not return the
candidate list it stores.

For an engine that offers to tell you why, the why is currently a counter.

### The distributed layer has no end-to-end test

`tests/cluster/cluster_test.sh` starts three containers and waits for health.
Its two verification steps, running `test_queries.tql` through the router and
confirming the primary returns the same rows, are not implemented. It now exits
non-zero and says so rather than printing a pass.

Component-level tests exist (replication, scatter-gather, write forwarding,
location streaming) and there are two single-process loopback integration tests.
There is no genuine multi-process verification, so read the Phase 9 row in the
status table accordingly.

### Writes fsync individually with no group commit

`INSERT` 8.7 ms, `CONFIRM` 12.4 ms, both dominated by WAL `fsync`. No batching,
so bulk ingest runs at roughly 115 entities/sec.

### The planner does not estimate pre-filter selectivity

`SEARCH ... WHERE ... NEAR ...` pre-filters unconditionally. At low selectivity
that costs more than the HNSW work it avoids: 500 µs filtered against 320 µs
unfiltered. `NO_PREFILTER` is the manual override; the cost model does not
decide.

### Not measured at all

Distributed behaviour (replication lag, scatter-gather, router overhead), recall
under tier pressure, HNSW build time, crash-recovery time, anything past 5,000
vectors. No numbers. Do not read the claims above as covering them.

---

## Current Status

### What's built (Phases 1-16b)

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
| 16 | Query composition (WITHIN clause for graph-scoped vector search) | Done |
| 16b | PolarQuant quantisation (TurboQuant-style 3-bit encoding, Walsh-Hadamard rotation, Lloyd-Max centroids) | Done |

---

## Quick Start

Requires Rust 1.88 or newer (pinned in `rust-toolchain.toml`; the floor is set
by the optional ONNX vectoriser dependency) and `protoc` for the gRPC crates.

```bash
# macOS
brew install protobuf
# Debian/Ubuntu
sudo apt-get install -y protobuf-compiler
```

```bash
# Build
cargo build --workspace

# Run tests (740 unit + integration tests)
cargo test --workspace

# Measured tier recall, with the table printed
cargo test -p trondb-core --test quantisation_recall -- --nocapture

# Benchmarks
cargo bench -p trondb-core --bench engine_bench

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
CREATE EDGE performs_at FROM acts TO venues;

-- Insert an edge
INSERT EDGE performs_at FROM 'act1' TO 'v1';

-- Traverse (legacy)
TRAVERSE performs_at FROM 'act1' DEPTH 2;

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
| PolarQuant | Low-bit vector quantisation. Random rotation (Walsh-Hadamard) + Lloyd-Max optimal scalar quantisation. 10x compression at 3-bit, measured 47% recall@10. Implements the first stage of TurboQuant only. |
| WAL | Write-Ahead Log. Every mutation logged before ack. Basis for replication and recovery. |
