# TronDB

Inference-first storage engine. Phase 7b: Graph Depth, Entity Lifecycle, Range Queries, Edge Decay.

## Project Structure

- `crates/trondb-wal/` — Write-Ahead Log: record types (MessagePack), segment files, buffer, async writer, crash recovery
- `crates/trondb-core/` — Engine: types, Fjall-backed store, Location Table (DashMap), HNSW index (hnsw_rs), edges (AdjacencyIndex), planner, async executor. Depends on trondb-wal + trondb-tql.
- `crates/trondb-tql/` — TQL parser (logos lexer + recursive descent). No engine dependency.
- `crates/trondb-cli/` — Interactive REPL binary (Tokio + rustyline). Depends on all crates.
- Routing Intelligence: SemanticRouter with health signals, co-location, semantic routing
  - `crates/trondb-routing/` — Routing layer: health signals, co-location, semantic router. Depends on trondb-core + trondb-wal + trondb-tql.
  - NodeHandle trait: LocalNode (in-process), SimulatedNode (tests), RemoteNode (Phase 6b)
  - HealthSignal + HealthCache: load score computation (RAM 0.35, queue 0.30, CPU 0.20, HNSW 0.10, lag 0.05)
  - AffinityIndex: explicit groups (WAL-logged, Fjall-persisted) + implicit co-occurrence (RAM-only)
  - Candidate scoring: health (40%) + verb fit (30%) + entity affinity (30%)
  - Soft eviction: ungrouped LRU → implicit groups → explicit groups
  - Background tasks: health polling (200ms), implicit affinity promotion (30s)
  - TQL: COLLOCATE WITH, AFFINITY GROUP, CREATE AFFINITY GROUP, ALTER ENTITY DROP AFFINITY GROUP
  - EXPLAIN shows routing section (selected node, score breakdown, candidates)
- Tiered Storage: warm (Int8) and archive (Binary) tiers with automatic migration
  - Vector quantisation: Int8 scalar quantisation (~75% size reduction), Binary (~97%)
  - Tier partitions: {collection} (hot), warm.{collection}, archive.{collection}
  - TierMigrator: background demotion cycle (hot→warm→archive) based on access patterns
  - Promotion on access: warm→hot auto-promotion on FETCH (dequantise + HNSW re-insert)
  - HNSW tombstone removal + periodic rebuild (10% threshold)
  - TQL: DEMOTE/PROMOTE entities, EXPLAIN TIERS for distribution
  - WAL: TierMigration record (0x70) for crash recovery
  - LocationDescriptor.last_accessed for durable access tracking

## Conventions

- Rust 2021 edition, async (Tokio runtime)
- Tests: `cargo test --workspace`
- Run REPL: `cargo run -p trondb-cli`
- Run REPL with custom data dir: `cargo run -p trondb-cli -- --data-dir /path/to/data`
- TQL is case-insensitive for keywords, case-sensitive for identifiers
- LogicalId is a String wrapper (user-provided or UUID v4)
- Persistence: Fjall (LSM-based). Data dir default: ./trondb_data
- WAL: MessagePack records, CRC32 verified, segment files. Dir: {data_dir}/wal/
- Write path: WAL append → flush+fsync → apply to Fjall + Location Table + HNSW index + AdjacencyIndex → ack
- Location Table: DashMap-backed control fabric, always in RAM
  - Tracks LocationDescriptor per representation (tier, state, encoding, node address)
  - WAL-logged via RecordType::LocationUpdate (0x40)
  - Periodically snapshotted to {data_dir}/location_table.snap
  - Restored from snapshot + WAL replay on startup
  - State machine: Clean → Dirty → Recomputing → Clean; Clean → Migrating → Clean
- HNSW Index: per-collection vector index, always in RAM (control fabric)
  - Uses hnsw_rs with DistCosine (cosine similarity)
  - Rebuilt from Fjall on startup (no persistence)
  - SEARCH returns results with similarity scores, filtered by confidence threshold
  - QueryMode::Probabilistic for SEARCH, QueryMode::Deterministic for FETCH
- Edges: schema-first structural edges with AdjacencyIndex
  - Edge types declared via CREATE EDGE, stored in Fjall
  - Edges stored in Fjall per-type partitions (edges.{type})
  - AdjacencyIndex (DashMap) in RAM for fast TRAVERSE, rebuilt from Fjall on startup
  - AdjacencyIndex backward index for efficient reverse edge lookup
  - Edge created_at timestamp (u64 millis) for decay computation
  - Structural edges have confidence=1.0, no decay
  - TRAVERSE: BFS multi-hop (depth cap 10), cycle detection via HashSet, forward-only
  - Edge confidence decay: lazy computation on TRAVERSE, exponential/linear/step functions
  - DecaySweeper background task: periodic edge pruning (60s interval)
  - CREATE EDGE DECAY syntax: EXPONENTIAL/LINEAR/STEP with RATE/FLOOR/PRUNE
  - Entity deletion: cascading 9-step cleanup (WAL, HNSW, field index, sparse index, location table, Fjall, edges, tiered storage)
  - DELETE 'id' FROM collection syntax for entity removal
- Field Index: Fjall-backed sortable byte encoding, compound/partial indexes
  - One Fjall partition per declared index (fidx.{collection}.{index_name})
  - Sortable encoding: Text=UTF-8, Int=sign-bit-flipped BE, Float=IEEE754 manipulated, Bool=0x00/0x01
  - Compound keys: null-byte separated, prefix scan on leading fields
  - Range queries: Gt/Lt/Gte/Lte → FieldIndexRange strategy, uses lookup_range()
- Sparse Vector Index: RAM-resident inverted index (DashMap), SPLADE-style sparse vectors
  - SparseIndex rebuilt from Fjall on startup
  - Inner product scoring, min_weight=0.001 filter
- Hybrid SEARCH: RRF merge (k=60, 1-based ranking) combines dense + sparse results
- ScalarPreFilter: WHERE + SEARCH optimisation via field index narrowing (post-filter, 4x over-fetch)
- Planner: schema-aware strategy selection (Hnsw, Sparse, Hybrid, FieldIndexLookup, ScalarPreFilter)
- CREATE COLLECTION: block syntax with REPRESENTATION, FIELD, INDEX declarations
- INSERT: named representation vectors (REPRESENTATION name VECTOR/SPARSE)
- EXPLAIN: shows strategy, index names, pre-filter details
