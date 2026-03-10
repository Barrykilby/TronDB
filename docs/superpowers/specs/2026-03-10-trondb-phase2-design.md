# TronDB Phase 2 Design Spec: Fjall-backed Entity Store with WAL

**Status**: Approved
**Date**: 2026-03-10
**Scope**: Durable entity persistence (Fjall) + Write-Ahead Log + Tokio async runtime
**Follows**: Phase 1 (TQL grammar — complete), PoC (in-memory store — complete)
**Design doc refs**: `trondb_design_v4.docx` Table 19 Phase 2, `trondb_wal_v1.docx` full spec

## Overview

Phase 2 replaces the in-memory DashMap store with Fjall (LSM-based, pure Rust) for durable entity persistence. A new `trondb-wal` crate implements the WAL format from the WAL design spec — MessagePack-encoded, length-prefixed, CRC32-verified records in fixed-size segment files. Tokio enters the codebase. The write path follows the ZFS SLOG pattern: WAL append → flush+fsync → apply to Fjall → ack.

Vectors, SEARCH, and HNSW are removed from execution. They return in Phase 4 on top of this persistent foundation. The TQL parser retains all grammar (Phase 1 is complete); only execution of vector operations is gated.

## Crate Structure

```
~/Projects/TronDB/
├── Cargo.toml                  # workspace root — adds trondb-wal
├── crates/
│   ├── trondb-wal/             # NEW — WAL record format, writer, segments, recovery
│   ├── trondb-core/            # MODIFIED — Fjall store, async engine, WAL integration
│   ├── trondb-tql/             # UNCHANGED — TQL parser
│   └── trondb-cli/             # MODIFIED — Tokio runtime, updated for async engine
```

**Dependency flow**: `trondb-cli` → `trondb-core` → `trondb-wal` + `trondb-tql`

## New Crate: `trondb-wal`

### WAL Record Format (per WAL spec §2)

Each record is written as:
```
[4-byte length prefix (big-endian u32)] [MessagePack body] [4-byte CRC32]
```

**Record envelope** (MessagePack map, all fields required):

| Field | Type | Description |
|-------|------|-------------|
| `lsn` | u64 | Log Sequence Number, monotonically increasing, no gaps |
| `ts` | i64 | Wall-clock timestamp, unix milliseconds |
| `tx_id` | u64 | Transaction ID, groups records in atomic operations |
| `record_type` | u8 | Enum discriminant (see below) |
| `schema_ver` | u32 | Collection schema version at write time (matches Entity.schema_version) |
| `collection` | String | Collection name |
| `payload` | Vec<u8> | Record-type-specific inner MessagePack |

**Record types** (full enum, Phase 2 active types marked):

| ID | Name | Phase 2 |
|----|------|---------|
| 0x01 | TX_BEGIN | ✓ |
| 0x02 | TX_COMMIT | ✓ |
| 0x03 | TX_ABORT | ✓ |
| 0x10 | ENTITY_WRITE | ✓ |
| 0x11 | ENTITY_DELETE | ✓ |
| 0x20 | REPR_WRITE | — (Phase 4) |
| 0x21 | REPR_DIRTY | — (Phase 4) |
| 0x30 | EDGE_WRITE | — (Phase 5) |
| 0x31 | EDGE_INFERRED | — (Phase 6) |
| 0x32 | EDGE_CONFIDENCE_UPDATE | — (Phase 8) |
| 0x33 | EDGE_CONFIRM | — (Phase 6) |
| 0x34 | EDGE_DELETE | — (Phase 5) |
| 0x40 | LOCATION_UPDATE | — (Phase 3) |
| 0x50 | SCHEMA_CREATE_COLLECTION | ✓ |
| 0x51 | SCHEMA_CREATE_EDGE_TYPE | — (Phase 5) |
| 0xFF | CHECKPOINT | ✓ |

### Segment Files (per WAL spec §6)

**64-byte file header:**
```rust
magic:      [u8; 8]  = b"TRONWAL1"
version:    u16      = 1
segment_id: u64
created_at: i64      // unix ms
reserved:   [u8; 38]
```

- Fixed size: 256 MiB default per segment
- Naming: `wal_{segment_id:016x}.twal`
- Active segment: exactly one, highest-numbered
- Rotation: when active segment reaches size limit, fsync+close, open new segment_id+1
- Retention: segments older than checkpoint LSN minus `wal_retention_ms` eligible for deletion
- Directory: configurable, default `{data_dir}/wal/`

### In-Memory WAL Buffer (per WAL spec §3)

```rust
pub struct WalBuffer {
    buffer:   VecDeque<WalRecord>,
    tail_lsn: u64,
    head_lsn: u64,
    capacity: usize,    // default 64 MiB
    flush_ms: u64,      // default 10ms
}
```

**Flush policy** — flush when ANY condition met:
1. Buffer byte size reaches capacity (64 MiB)
2. Flush interval elapses (10ms) — bounded lag
3. TX_COMMIT record appended — immediate flush+fsync, durability before ack
4. CHECKPOINT record written — flush+fsync before proceeding

### WAL Writer (async, Tokio)

```rust
pub struct WalWriter {
    buffer:     tokio::sync::Mutex<WalBuffer>,
    next_lsn:   AtomicU64,
    next_tx_id:  AtomicU64,
    segment:    tokio::sync::Mutex<WalSegment>,
    config:     WalConfig,
}

impl WalWriter {
    pub async fn open(config: WalConfig) -> Result<Self, WalError>;
    pub fn append(&self, record_type: RecordType, collection: &str, tx_id: u64, schema_ver: u16, payload: Vec<u8>) -> u64; // returns LSN
    pub async fn commit(&self, tx_id: u64) -> Result<u64, WalError>; // flush+fsync, returns commit LSN
    pub async fn checkpoint(&self) -> Result<u64, WalError>;
    pub fn next_tx_id(&self) -> u64;
}
```

**Important:** `append()` only pushes to the in-memory buffer — no I/O, no flush. It uses `blocking_lock()` on the buffer mutex, which is safe because the critical section is trivially fast (no I/O). Only `commit()` drains the buffer, writes to the segment file, and fsyncs. Buffer-full and timer-interval flushing (for bounded replica lag) are deferred to Phase 9 when replicas exist; in Phase 2 only commit-triggered flush matters.

Phase 2 defers: replica handles, gRPC streaming, replica ack waiting, background flush task. `commit()` returns after local fsync only.

### CHECKPOINT Record Payload

The CHECKPOINT record (0xFF) payload is a MessagePack map:
```rust
{
    "checkpoint_lsn": u64,  // LSN up to which all records have been applied to Fjall
}
```

In Phase 2, `checkpoint()` writes the CHECKPOINT record after ensuring all preceding records are applied to Fjall. The `snapshot_path` field (for Location Table snapshots) is deferred to Phase 3.

### Write Path Failure Semantics

If Fjall apply fails after WAL commit, the write is still durable in the WAL and will be applied on next recovery. The error is surfaced to the caller but the write is not lost. This is the standard WAL durability guarantee.

### Crash Recovery (per WAL spec §5)

Recovery sequence:
1. **Find last CHECKPOINT** — scan segment files in reverse for 0xFF record
2. **Replay from checkpoint LSN** — read all records with LSN > checkpoint_lsn in order
3. **CRC32 verify** each record; corrupt mid-WAL record = log error + skip; corrupt tail = expected after crash, discard
4. **Transaction handling** — TX_COMMIT: apply all buffered records for tx_id. TX_ABORT: discard. No TX_COMMIT found: discard (ARIES-style)
5. **Apply to Fjall** — replay ENTITY_WRITE/DELETE and SCHEMA_CREATE_COLLECTION against Fjall store
6. **Resume** — LSN counter resumes from head_lsn + 1

```rust
pub struct WalRecovery;

impl WalRecovery {
    pub async fn recover(wal_dir: &Path, store: &FjallStore) -> Result<u64, WalError>; // returns recovered head LSN
}
```

### Configuration

```rust
pub struct WalConfig {
    pub wal_dir:                PathBuf,
    pub segment_size_bytes:     usize,   // default 256 MiB
    pub buffer_capacity_bytes:  usize,   // default 64 MiB
    pub flush_interval_ms:      u64,     // default 10ms
    pub checkpoint_interval_ms: u64,     // default 60_000ms
    pub retention_ms:           u64,     // default 3_600_000ms
}
```

## Modified Crate: `trondb-core`

### Store Rewrite — Fjall Replaces DashMap

The `Store` struct is rewritten to use Fjall. Fjall terminology: `Database` is the top-level handle (directory), `Keyspace` is a column family within it (one per TronDB collection).

```rust
pub struct FjallStore {
    db: fjall::Database,
    meta: fjall::Keyspace,  // _meta keyspace for collection metadata
}
```

- One Fjall `Keyspace` per TronDB collection
- Entities serialised to MessagePack via `rmp-serde`, keyed by `LogicalId` bytes
- Collection metadata (name, dimensions, field defs, schema version) stored in `_meta` partition
- `Store::new()` becomes `FjallStore::open(data_dir)` — takes a path
- `CREATE COLLECTION` syntax and dimension validation remain unchanged for forward compatibility with Phase 4

**Key schema per partition:**
- Entity key: `entity:{logical_id}` → MessagePack-serialised Entity (minus representations for Phase 2)

**Meta partition key schema:**
- `collection:{name}` → MessagePack-serialised Collection struct

### Engine Becomes Async

```rust
pub struct Engine {
    executor: Executor,
    wal: WalWriter,
    config: EngineConfig,
}

pub struct EngineConfig {
    pub data_dir: PathBuf,
    pub wal: WalConfig,
}

impl Engine {
    pub async fn open(config: EngineConfig) -> Result<Self, EngineError>;
    pub async fn execute_tql(&self, input: &str) -> Result<QueryResult, EngineError>;
    pub fn collections(&self) -> Vec<String>;
}
```

**Write path** (INSERT, CREATE COLLECTION):
1. Parse TQL → AST → Plan
2. Begin WAL transaction (`TX_BEGIN`)
3. Append mutation record(s) (`ENTITY_WRITE` / `SCHEMA_CREATE_COLLECTION`)
4. Commit WAL transaction (`TX_COMMIT`) — flush+fsync
5. Apply to Fjall store
6. Return result

**Read path** (FETCH):
1. Parse TQL → AST → Plan
2. Read directly from Fjall
3. Return result

### What Gets Removed

- `index.rs` — VectorIndex, brute-force cosine similarity. Deleted entirely.
- SEARCH execution in `executor.rs` — returns `EngineError::UnsupportedOperation("SEARCH requires vector index (Phase 4)")`
- SEARCH plan in `planner.rs` — same gating

### What Stays

- TQL parser — all verbs remain parseable (grammar is Phase 1, complete)
- Entity, LogicalId, Value types — Representation struct stays defined but unused
- FETCH, INSERT, CREATE COLLECTION execution
- EXPLAIN — reports plan without executing, works for all statement types
- CLI REPL

### Error Changes

New error variants:
```rust
pub enum EngineError {
    // ... existing variants ...
    #[error("WAL error: {0}")]
    Wal(#[from] WalError),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("unsupported operation: {0}")]
    UnsupportedOperation(String),
}
```

## Modified Crate: `trondb-cli`

- `#[tokio::main]` on `async fn main()`
- REPL loop calls `engine.execute_tql(input).await` directly — rustyline blocks the current thread but this is fine for a single-threaded CLI
- New dot-command: `.data` — prints data directory and WAL directory paths
- Startup message includes data dir
- Data dir default: `./trondb_data` (relative to CWD), overridable via `--data-dir` CLI arg

## New Dependencies

| Crate | Purpose | Workspace |
|-------|---------|-----------|
| `fjall` | LSM-based persistent entity store | ✓ |
| `rmp-serde` | MessagePack for WAL records + entity serialisation | ✓ |
| `crc32fast` | CRC32 checksums on WAL records | ✓ |
| `tokio` | Async runtime (features: rt-multi-thread, sync, fs, time, io-util, macros) | ✓ |

## What Phase 2 Validates

1. Entities survive process restart (Fjall persistence)
2. WAL records are written, flushed, and fsynced before write ack
3. Crash recovery replays WAL and rebuilds consistent Fjall state
4. Checkpoints bound recovery time
5. Write path matches ZFS SLOG pattern: WAL is fast durability guarantee, Fjall is the store
6. Tokio runtime integrated without breaking TQL pipeline
7. Collection metadata persists across restarts

## What Phase 2 Does Not Address

- Vectors, HNSW, SEARCH (Phase 4)
- Location Table with tier descriptors and state machine (Phase 3)
- Edges of any kind (Phase 5+)
- gRPC WAL streaming to replicas (Phase 9)
- Representation persistence (Phase 4)
- Vector compression (Phase 7)
- Temporal queries (v3 Phase 8)
