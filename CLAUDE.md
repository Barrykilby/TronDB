# TronDB

Inference-first storage engine. Phase 5a: Indexes + Structural edges + TRAVERSE.

## Project Structure

- `crates/trondb-wal/` — Write-Ahead Log: record types (MessagePack), segment files, buffer, async writer, crash recovery
- `crates/trondb-core/` — Engine: types, Fjall-backed store, Location Table (DashMap), HNSW index (hnsw_rs), edges (AdjacencyIndex), planner, async executor. Depends on trondb-wal + trondb-tql.
- `crates/trondb-tql/` — TQL parser (logos lexer + recursive descent). No engine dependency.
- `crates/trondb-cli/` — Interactive REPL binary (Tokio + rustyline). Depends on all crates.

## Conventions

- Rust 2021 edition, async (Tokio runtime)
- Tests: `cargo test --workspace`
- Run REPL: `cargo run -p trondb-cli`
- Run REPL with custom data dir: `cargo run -p trondb-cli -- --data-dir /path/to/data`
- TQL is case-insensitive for keywords, case-sensitive for identifiers
- All vectors are Float32 (gated until Phase 7)
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
  - Structural edges have confidence=1.0, no decay
  - TRAVERSE returns connected entities (single-hop, DEPTH > 1 gated)
  - DecayConfig fields defined but not driven until Phase 6
- Field Index: Fjall-backed sortable byte encoding, compound/partial indexes
  - One Fjall partition per declared index (fidx.{collection}.{index_name})
  - Sortable encoding: Text=UTF-8, Int=sign-bit-flipped BE, Float=IEEE754 manipulated, Bool=0x00/0x01
  - Compound keys: null-byte separated, prefix scan on leading fields
- Sparse Vector Index: RAM-resident inverted index (DashMap), SPLADE-style sparse vectors
  - SparseIndex rebuilt from Fjall on startup
  - Inner product scoring, min_weight=0.001 filter
- Hybrid SEARCH: RRF merge (k=60, 1-based ranking) combines dense + sparse results
- ScalarPreFilter: WHERE + SEARCH optimisation via field index narrowing (post-filter, 4x over-fetch)
- Planner: schema-aware strategy selection (Hnsw, Sparse, Hybrid, FieldIndexLookup, ScalarPreFilter)
- CREATE COLLECTION: block syntax with REPRESENTATION, FIELD, INDEX declarations
- INSERT: named representation vectors (REPRESENTATION name VECTOR/SPARSE)
- EXPLAIN: shows strategy, index names, pre-filter details
