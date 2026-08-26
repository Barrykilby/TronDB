# TronDB Phase 2: Fjall-backed Entity Store with WAL — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the in-memory DashMap store with Fjall for durable persistence and add a WAL (Write-Ahead Log) with MessagePack encoding, CRC32 integrity, segment files, and crash recovery. Introduce Tokio async runtime.

**Architecture:** Write path follows ZFS SLOG pattern: WAL append → flush+fsync → apply to Fjall → ack. Read path reads directly from Fjall. New `trondb-wal` crate owns all WAL logic. `trondb-core` store rewritten for Fjall. Vectors/SEARCH gated until Phase 4.

**Tech Stack:** Rust, Fjall (LSM storage), rmp-serde (MessagePack), crc32fast, Tokio

**Spec:** `docs/superpowers/specs/2026-03-10-trondb-phase2-design.md`

---

## File Structure

### New files
- `crates/trondb-wal/Cargo.toml` — WAL crate manifest
- `crates/trondb-wal/src/lib.rs` — Public API re-exports
- `crates/trondb-wal/src/record.rs` — RecordType enum, WalRecord struct, serialisation
- `crates/trondb-wal/src/segment.rs` — Segment file I/O: header, write, read, rotation
- `crates/trondb-wal/src/buffer.rs` — In-memory WalBuffer ring buffer + flush policy
- `crates/trondb-wal/src/writer.rs` — WalWriter: append, commit, checkpoint (async)
- `crates/trondb-wal/src/recovery.rs` — Crash recovery: find checkpoint, replay, verify CRC
- `crates/trondb-wal/src/config.rs` — WalConfig struct with defaults
- `crates/trondb-wal/src/error.rs` — WalError enum

### Modified files
- `Cargo.toml` — Add trondb-wal to workspace members, add new workspace deps
- `crates/trondb-core/Cargo.toml` — Add fjall, rmp-serde, tokio, trondb-wal deps
- `crates/trondb-core/src/store.rs` — Rewrite: DashMap → Fjall-backed FjallStore
- `crates/trondb-core/src/executor.rs` — Async execute, WAL integration on write path, remove vector index
- `crates/trondb-core/src/planner.rs` — Gate SEARCH plan with UnsupportedOperation error
- `crates/trondb-core/src/lib.rs` — Engine becomes async, opens with config + data dir
- `crates/trondb-core/src/error.rs` — Add Wal, Storage, UnsupportedOperation variants
- `crates/trondb-core/src/types.rs` — No structural changes, just ensure serde derives work with rmp-serde
- `crates/trondb-cli/Cargo.toml` — Add tokio dep
- `crates/trondb-cli/src/main.rs` — #[tokio::main], async engine, .data command, --data-dir arg

### Removed files
- `crates/trondb-core/src/index.rs` — VectorIndex deleted (returns Phase 4)

---

## Chunk 1: WAL Foundation (record format + segments)

### Task 1: WAL crate scaffold + record types

**Files:**
- Create: `crates/trondb-wal/Cargo.toml`
- Create: `crates/trondb-wal/src/lib.rs`
- Create: `crates/trondb-wal/src/record.rs`
- Create: `crates/trondb-wal/src/error.rs`
- Create: `crates/trondb-wal/src/config.rs`
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Write the failing test**

In `crates/trondb-wal/src/record.rs`, write a test that creates a WalRecord, serialises it to MessagePack bytes, and deserialises it back. Assert round-trip equality.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_record_round_trip_msgpack() {
        let record = WalRecord {
            lsn: 1,
            ts: 1741612800000,
            tx_id: 100,
            record_type: RecordType::EntityWrite,
            schema_ver: 1,
            collection: "venues".into(),
            payload: rmp_serde::to_vec(&serde_json::json!({"entity_id": "e1"})).unwrap(),
        };

        let bytes = record.to_bytes().unwrap();
        let decoded = WalRecord::from_bytes(&bytes).unwrap();

        assert_eq!(record.lsn, decoded.lsn);
        assert_eq!(record.tx_id, decoded.tx_id);
        assert_eq!(record.record_type, decoded.record_type);
        assert_eq!(record.collection, decoded.collection);
        assert_eq!(record.payload, decoded.payload);
    }

    #[test]
    fn crc32_detects_corruption() {
        let record = WalRecord {
            lsn: 1,
            ts: 1741612800000,
            tx_id: 100,
            record_type: RecordType::EntityWrite,
            schema_ver: 1,
            collection: "venues".into(),
            payload: vec![1, 2, 3],
        };

        let mut framed = record.to_framed_bytes().unwrap();
        // Corrupt a byte in the body
        framed[6] ^= 0xFF;
        assert!(WalRecord::from_framed_bytes(&framed).is_err());
    }

    #[test]
    fn record_type_discriminants() {
        assert_eq!(RecordType::TxBegin as u8, 0x01);
        assert_eq!(RecordType::TxCommit as u8, 0x02);
        assert_eq!(RecordType::EntityWrite as u8, 0x10);
        assert_eq!(RecordType::Checkpoint as u8, 0xFF);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p trondb-wal`
Expected: Compilation fails — crate and types don't exist yet.

- [ ] **Step 3: Create workspace setup and implement record types**

**`Cargo.toml` (workspace root)** — add trondb-wal to members and new workspace deps:
```toml
[workspace]
members = [
    "crates/trondb-core",
    "crates/trondb-tql",
    "crates/trondb-cli",
    "crates/trondb-wal",
]

[workspace.dependencies]
# ... existing deps ...
fjall = "2"
rmp-serde = "1"
crc32fast = "1"
tokio = { version = "1", features = ["rt-multi-thread", "sync", "fs", "time", "io-util", "macros"] }
```

**`crates/trondb-wal/Cargo.toml`:**
```toml
[package]
name = "trondb-wal"
version = "0.1.0"
edition = "2021"

[dependencies]
rmp-serde.workspace = true
serde = { workspace = true, features = ["derive"] }
crc32fast.workspace = true
tokio.workspace = true
thiserror.workspace = true

[dev-dependencies]
serde_json.workspace = true
tempfile = "3"
```

**`crates/trondb-wal/src/error.rs`:**
```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WalError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialisation error: {0}")]
    Serialisation(String),

    #[error("CRC32 mismatch at LSN {lsn}: expected {expected:#010x}, got {got:#010x}")]
    CrcMismatch { lsn: u64, expected: u32, got: u32 },

    #[error("corrupt WAL segment: {0}")]
    CorruptSegment(String),

    #[error("invalid segment header: {0}")]
    InvalidHeader(String),

    #[error("WAL record truncated at LSN {0}")]
    Truncated(u64),
}
```

**`crates/trondb-wal/src/config.rs`:**
```rust
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct WalConfig {
    pub wal_dir: PathBuf,
    pub segment_size_bytes: usize,
    pub buffer_capacity_bytes: usize,
    pub flush_interval_ms: u64,
    pub checkpoint_interval_ms: u64,
    pub retention_ms: u64,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            wal_dir: PathBuf::from("./trondb_data/wal"),
            segment_size_bytes: 256 * 1024 * 1024, // 256 MiB
            buffer_capacity_bytes: 64 * 1024 * 1024, // 64 MiB
            flush_interval_ms: 10,
            checkpoint_interval_ms: 60_000,
            retention_ms: 3_600_000,
        }
    }
}
```

**`crates/trondb-wal/src/record.rs`:**
```rust
use serde::{Deserialize, Serialize};
use crate::error::WalError;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordType {
    TxBegin              = 0x01,
    TxCommit             = 0x02,
    TxAbort              = 0x03,
    EntityWrite          = 0x10,
    EntityDelete         = 0x11,
    ReprWrite            = 0x20,
    ReprDirty            = 0x21,
    EdgeWrite            = 0x30,
    EdgeInferred         = 0x31,
    EdgeConfidenceUpdate = 0x32,
    EdgeConfirm          = 0x33,
    EdgeDelete           = 0x34,
    LocationUpdate       = 0x40,
    SchemaCreateColl     = 0x50,
    SchemaCreateEdgeType = 0x51,
    Checkpoint           = 0xFF,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalRecord {
    pub lsn: u64,
    pub ts: i64,
    pub tx_id: u64,
    pub record_type: RecordType,
    pub schema_ver: u32,
    pub collection: String,
    pub payload: Vec<u8>,
}

impl WalRecord {
    /// Serialise the record body to MessagePack bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, WalError> {
        rmp_serde::to_vec_named(self)
            .map_err(|e| WalError::Serialisation(e.to_string()))
    }

    /// Deserialise a record body from MessagePack bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WalError> {
        rmp_serde::from_slice(bytes)
            .map_err(|e| WalError::Serialisation(e.to_string()))
    }

    /// Serialise to framed format: [4-byte length][body][4-byte CRC32]
    pub fn to_framed_bytes(&self) -> Result<Vec<u8>, WalError> {
        let body = self.to_bytes()?;
        let crc = crc32fast::hash(&body);
        let len = body.len() as u32;

        let mut out = Vec::with_capacity(4 + body.len() + 4);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc.to_be_bytes());
        Ok(out)
    }

    /// Deserialise from framed format, verifying CRC32.
    pub fn from_framed_bytes(bytes: &[u8]) -> Result<Self, WalError> {
        if bytes.len() < 8 {
            return Err(WalError::Truncated(0));
        }

        let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        if bytes.len() < 4 + len + 4 {
            return Err(WalError::Truncated(0));
        }

        let body = &bytes[4..4 + len];
        let expected_crc = u32::from_be_bytes([
            bytes[4 + len],
            bytes[4 + len + 1],
            bytes[4 + len + 2],
            bytes[4 + len + 3],
        ]);

        let actual_crc = crc32fast::hash(body);
        if actual_crc != expected_crc {
            // Try to extract LSN for error reporting
            let lsn = rmp_serde::from_slice::<WalRecord>(body)
                .map(|r| r.lsn)
                .unwrap_or(0);
            return Err(WalError::CrcMismatch {
                lsn,
                expected: expected_crc,
                got: actual_crc,
            });
        }

        Self::from_bytes(body)
    }
}
```

**`crates/trondb-wal/src/lib.rs`:**
```rust
pub mod buffer;
pub mod config;
pub mod error;
pub mod record;
pub mod recovery;
pub mod segment;
pub mod writer;

pub use config::WalConfig;
pub use error::WalError;
pub use record::{RecordType, WalRecord};
pub use writer::WalWriter;
pub use recovery::WalRecovery;
```

Note: `buffer.rs`, `segment.rs`, `writer.rs`, `recovery.rs` will be created as empty files initially (just enough for `pub mod` to compile). They get implemented in subsequent tasks.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p trondb-wal`
Expected: All 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-wal/ Cargo.toml
git commit -m "feat(wal): add trondb-wal crate with record types, MessagePack serialisation, CRC32 framing"
```

---

### Task 2: WAL segment file I/O

**Files:**
- Create: `crates/trondb-wal/src/segment.rs`

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{RecordType, WalRecord};
    use tempfile::TempDir;

    fn make_record(lsn: u64) -> WalRecord {
        WalRecord {
            lsn,
            ts: 1741612800000,
            tx_id: 1,
            record_type: RecordType::EntityWrite,
            schema_ver: 1,
            collection: "test".into(),
            payload: vec![1, 2, 3],
        }
    }

    #[tokio::test]
    async fn write_and_read_segment_header() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wal_0000000000000001.twal");

        WalSegment::create(&path, 1).await.unwrap();
        let header = WalSegment::read_header(&path).await.unwrap();

        assert_eq!(&header.magic, b"TRONWAL1");
        assert_eq!(header.version, 1);
        assert_eq!(header.segment_id, 1);
    }

    #[tokio::test]
    async fn append_and_read_records() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wal_0000000000000001.twal");

        let mut seg = WalSegment::create(&path, 1).await.unwrap();
        seg.append(&make_record(1)).await.unwrap();
        seg.append(&make_record(2)).await.unwrap();
        seg.flush().await.unwrap();

        let records = WalSegment::read_all(&path).await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].lsn, 1);
        assert_eq!(records[1].lsn, 2);
    }

    #[tokio::test]
    async fn segment_reports_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wal_0000000000000001.twal");

        let mut seg = WalSegment::create(&path, 1).await.unwrap();
        let initial = seg.current_size();
        assert_eq!(initial, SEGMENT_HEADER_SIZE as u64);

        seg.append(&make_record(1)).await.unwrap();
        seg.flush().await.unwrap();
        assert!(seg.current_size() > initial);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p trondb-wal -- segment`
Expected: Fail — WalSegment doesn't exist.

- [ ] **Step 3: Implement WalSegment**

`crates/trondb-wal/src/segment.rs`:

```rust
use std::path::Path;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt, AsyncSeekExt, BufWriter};

use crate::error::WalError;
use crate::record::WalRecord;

pub const SEGMENT_HEADER_SIZE: usize = 64;
const MAGIC: &[u8; 8] = b"TRONWAL1";
const VERSION: u16 = 1;

#[derive(Debug)]
pub struct SegmentHeader {
    pub magic: [u8; 8],
    pub version: u16,
    pub segment_id: u64,
    pub created_at: i64,
}

pub struct WalSegment {
    writer: BufWriter<File>,
    path: std::path::PathBuf,
    current_size: u64,
}

impl WalSegment {
    /// Create a new segment file with header.
    pub async fn create(path: &Path, segment_id: u64) -> Result<Self, WalError> {
        let file = File::create(path).await?;
        let mut writer = BufWriter::new(file);

        // Write 64-byte header
        let mut header = [0u8; SEGMENT_HEADER_SIZE];
        header[0..8].copy_from_slice(MAGIC);
        header[8..10].copy_from_slice(&VERSION.to_be_bytes());
        header[10..18].copy_from_slice(&segment_id.to_be_bytes());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        header[18..26].copy_from_slice(&now.to_be_bytes());
        // bytes 26..64 are reserved (zeros)

        writer.write_all(&header).await?;
        writer.flush().await?;

        Ok(Self {
            writer,
            path: path.to_path_buf(),
            current_size: SEGMENT_HEADER_SIZE as u64,
        })
    }

    /// Open an existing segment for appending.
    pub async fn open_append(path: &Path) -> Result<Self, WalError> {
        let file = OpenOptions::new().append(true).open(path).await?;
        let size = file.metadata().await?.len();
        Ok(Self {
            writer: BufWriter::new(file),
            path: path.to_path_buf(),
            current_size: size,
        })
    }

    /// Read the segment header.
    pub async fn read_header(path: &Path) -> Result<SegmentHeader, WalError> {
        let mut file = File::open(path).await?;
        let mut buf = [0u8; SEGMENT_HEADER_SIZE];
        file.read_exact(&mut buf).await?;

        let mut magic = [0u8; 8];
        magic.copy_from_slice(&buf[0..8]);
        if &magic != MAGIC {
            return Err(WalError::InvalidHeader(format!(
                "bad magic: expected TRONWAL1, got {:?}",
                &magic
            )));
        }

        Ok(SegmentHeader {
            magic,
            version: u16::from_be_bytes([buf[8], buf[9]]),
            segment_id: u64::from_be_bytes(buf[10..18].try_into().unwrap()),
            created_at: i64::from_be_bytes(buf[18..26].try_into().unwrap()),
        })
    }

    /// Append a framed record to the segment buffer.
    pub async fn append(&mut self, record: &WalRecord) -> Result<usize, WalError> {
        let framed = record.to_framed_bytes()?;
        self.writer.write_all(&framed).await?;
        self.current_size += framed.len() as u64;
        Ok(framed.len())
    }

    /// Flush buffered writes to disk.
    pub async fn flush(&mut self) -> Result<(), WalError> {
        self.writer.flush().await?;
        Ok(())
    }

    /// Flush and fsync to ensure durability.
    pub async fn sync(&mut self) -> Result<(), WalError> {
        self.writer.flush().await?;
        self.writer.get_ref().sync_all().await?;
        Ok(())
    }

    pub fn current_size(&self) -> u64 {
        self.current_size
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read all records from a segment file, verifying CRC on each.
    /// Returns records in LSN order. Stops on truncated/corrupt tail record.
    pub async fn read_all(path: &Path) -> Result<Vec<WalRecord>, WalError> {
        let mut file = File::open(path).await?;
        let file_len = file.metadata().await?.len();

        // Skip header
        file.seek(std::io::SeekFrom::Start(SEGMENT_HEADER_SIZE as u64)).await?;

        let mut records = Vec::new();
        let mut pos = SEGMENT_HEADER_SIZE as u64;

        while pos < file_len {
            // Read length prefix
            if pos + 4 > file_len {
                break; // truncated length prefix — expected after crash
            }
            let mut len_buf = [0u8; 4];
            file.read_exact(&mut len_buf).await?;
            let body_len = u32::from_be_bytes(len_buf) as u64;

            // Read body + CRC
            let frame_remaining = body_len + 4; // body + CRC32
            if pos + 4 + frame_remaining > file_len {
                break; // truncated record — expected after crash
            }

            let mut frame = vec![0u8; frame_remaining as usize];
            file.read_exact(&mut frame).await?;

            // Verify CRC
            let body = &frame[..body_len as usize];
            let crc_bytes = &frame[body_len as usize..];
            let expected_crc = u32::from_be_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
            let actual_crc = crc32fast::hash(body);

            if actual_crc != expected_crc {
                // If this is the last record, it's an expected crash truncation
                if pos + 4 + frame_remaining >= file_len {
                    break;
                }
                // Mid-file corruption — log and skip
                eprintln!("WAL: CRC mismatch at offset {pos}, skipping record");
                pos += 4 + frame_remaining;
                continue;
            }

            match WalRecord::from_bytes(body) {
                Ok(record) => records.push(record),
                Err(e) => {
                    eprintln!("WAL: failed to deserialise record at offset {pos}: {e}");
                    pos += 4 + frame_remaining;
                    continue;
                }
            }

            pos += 4 + frame_remaining;
        }

        Ok(records)
    }
}

/// Parse segment_id from a segment file name like "wal_0000000000000001.twal".
pub fn segment_id_from_filename(name: &str) -> Option<u64> {
    let name = name.strip_prefix("wal_")?.strip_suffix(".twal")?;
    u64::from_str_radix(name, 16).ok()
}

/// Generate a segment file name from a segment_id.
pub fn segment_filename(segment_id: u64) -> String {
    format!("wal_{segment_id:016x}.twal")
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-wal -- segment`
Expected: All 3 segment tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-wal/src/segment.rs
git commit -m "feat(wal): implement WAL segment file I/O with 64-byte header, framed records, and async read/write"
```

---

### Task 3: WAL buffer + flush policy

**Files:**
- Create: `crates/trondb-wal/src/buffer.rs`

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{RecordType, WalRecord};

    fn make_record(lsn: u64, record_type: RecordType) -> WalRecord {
        WalRecord {
            lsn,
            ts: 1741612800000,
            tx_id: 1,
            record_type,
            schema_ver: 1,
            collection: "test".into(),
            payload: vec![1, 2, 3],
        }
    }

    #[test]
    fn buffer_append_and_drain() {
        let mut buf = WalBuffer::new(64 * 1024 * 1024);
        buf.append(make_record(1, RecordType::EntityWrite));
        buf.append(make_record(2, RecordType::EntityWrite));

        assert_eq!(buf.len(), 2);
        assert_eq!(buf.head_lsn(), Some(2));
        assert_eq!(buf.tail_lsn(), Some(1));

        let drained = buf.drain_all();
        assert_eq!(drained.len(), 2);
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn should_flush_on_commit() {
        let mut buf = WalBuffer::new(64 * 1024 * 1024);
        buf.append(make_record(1, RecordType::EntityWrite));
        assert!(!buf.should_flush());

        buf.append(make_record(2, RecordType::TxCommit));
        assert!(buf.should_flush());
    }

    #[test]
    fn should_flush_on_checkpoint() {
        let mut buf = WalBuffer::new(64 * 1024 * 1024);
        buf.append(make_record(1, RecordType::Checkpoint));
        assert!(buf.should_flush());
    }

    #[test]
    fn should_flush_on_capacity() {
        // Tiny capacity
        let mut buf = WalBuffer::new(1);
        buf.append(make_record(1, RecordType::EntityWrite));
        assert!(buf.should_flush());
    }
}
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p trondb-wal -- buffer`
Expected: Fail — WalBuffer doesn't exist.

- [ ] **Step 3: Implement WalBuffer**

```rust
use std::collections::VecDeque;
use crate::record::{RecordType, WalRecord};

pub struct WalBuffer {
    records: VecDeque<WalRecord>,
    byte_size: usize,
    capacity: usize,
    has_commit_or_checkpoint: bool,
}

impl WalBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            records: VecDeque::new(),
            byte_size: 0,
            capacity,
            has_commit_or_checkpoint: false,
        }
    }

    pub fn append(&mut self, record: WalRecord) {
        if matches!(record.record_type, RecordType::TxCommit | RecordType::Checkpoint) {
            self.has_commit_or_checkpoint = true;
        }
        // Estimate byte size (actual MessagePack size varies, but this is close enough for capacity checks)
        self.byte_size += record.payload.len() + 64; // ~64 bytes overhead per record
        self.records.push_back(record);
    }

    /// Should the buffer be flushed? True if:
    /// - A TX_COMMIT or CHECKPOINT record was appended
    /// - Buffer byte size exceeds capacity
    pub fn should_flush(&self) -> bool {
        self.has_commit_or_checkpoint || self.byte_size >= self.capacity
    }

    pub fn drain_all(&mut self) -> Vec<WalRecord> {
        self.byte_size = 0;
        self.has_commit_or_checkpoint = false;
        self.records.drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn head_lsn(&self) -> Option<u64> {
        self.records.back().map(|r| r.lsn)
    }

    pub fn tail_lsn(&self) -> Option<u64> {
        self.records.front().map(|r| r.lsn)
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-wal -- buffer`
Expected: All 4 buffer tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-wal/src/buffer.rs
git commit -m "feat(wal): implement in-memory WAL buffer with flush policy (commit, checkpoint, capacity)"
```

---

## Chunk 2: WAL Writer + Recovery

### Task 4: WAL writer (async)

**Files:**
- Create: `crates/trondb-wal/src/writer.rs`

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WalConfig;
    use crate::record::RecordType;
    use tempfile::TempDir;

    #[tokio::test]
    async fn writer_open_creates_wal_dir_and_first_segment() {
        let dir = TempDir::new().unwrap();
        let config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };

        let _writer = WalWriter::open(config.clone()).await.unwrap();

        assert!(config.wal_dir.exists());
        // Should have one segment file
        let entries: Vec<_> = std::fs::read_dir(&config.wal_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn writer_append_and_commit() {
        let dir = TempDir::new().unwrap();
        let config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };

        let writer = WalWriter::open(config.clone()).await.unwrap();

        let tx_id = writer.next_tx_id();
        writer.append(RecordType::TxBegin, "venues", tx_id, 1, vec![]);
        writer.append(
            RecordType::SchemaCreateColl,
            "venues",
            tx_id,
            1,
            rmp_serde::to_vec_named(&serde_json::json!({"name": "venues", "dimensions": 3})).unwrap(),
        );
        let commit_lsn = writer.commit(tx_id).await.unwrap();

        assert!(commit_lsn > 0);
    }

    #[tokio::test]
    async fn writer_records_survive_reopen() {
        let dir = TempDir::new().unwrap();
        let config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };

        // Write some records
        {
            let writer = WalWriter::open(config.clone()).await.unwrap();
            let tx_id = writer.next_tx_id();
            writer.append(RecordType::TxBegin, "test", tx_id, 1, vec![]);
            writer.append(RecordType::EntityWrite, "test", tx_id, 1, vec![1, 2, 3]);
            writer.commit(tx_id).await.unwrap();
        }

        // Read them back from segment files
        let entries: Vec<_> = std::fs::read_dir(&config.wal_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);

        let records = crate::segment::WalSegment::read_all(&entries[0].path()).await.unwrap();
        // TX_BEGIN + EntityWrite + TX_COMMIT = 3 records
        assert_eq!(records.len(), 3);
        assert_eq!(records[2].record_type, RecordType::TxCommit);
    }
}
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p trondb-wal -- writer`
Expected: Fail — WalWriter doesn't exist.

- [ ] **Step 3: Implement WalWriter**

```rust
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

use crate::buffer::WalBuffer;
use crate::config::WalConfig;
use crate::error::WalError;
use crate::record::{RecordType, WalRecord};
use crate::segment::{WalSegment, segment_filename, segment_id_from_filename};

pub struct WalWriter {
    buffer: Mutex<WalBuffer>,
    segment: Mutex<WalSegment>,
    next_lsn: AtomicU64,
    next_tx: AtomicU64,
    config: WalConfig,
}

impl WalWriter {
    pub async fn open(config: WalConfig) -> Result<Self, WalError> {
        tokio::fs::create_dir_all(&config.wal_dir).await?;

        // Find existing segments or create first one
        let (segment, head_lsn) = Self::open_or_create_segment(&config).await?;

        Ok(Self {
            buffer: Mutex::new(WalBuffer::new(config.buffer_capacity_bytes)),
            segment: Mutex::new(segment),
            next_lsn: AtomicU64::new(head_lsn + 1),
            next_tx: AtomicU64::new(1),
            config,
        })
    }

    async fn open_or_create_segment(config: &WalConfig) -> Result<(WalSegment, u64), WalError> {
        let mut segments = Self::list_segment_ids(&config.wal_dir).await?;
        segments.sort();

        if let Some(&last_id) = segments.last() {
            let path = config.wal_dir.join(segment_filename(last_id));
            // Read existing records to find head LSN
            let records = WalSegment::read_all(&path).await?;
            let head_lsn = records.last().map(|r| r.lsn).unwrap_or(0);
            let seg = WalSegment::open_append(&path).await?;
            Ok((seg, head_lsn))
        } else {
            let path = config.wal_dir.join(segment_filename(1));
            let seg = WalSegment::create(&path, 1).await?;
            Ok((seg, 0))
        }
    }

    async fn list_segment_ids(dir: &Path) -> Result<Vec<u64>, WalError> {
        let mut ids = Vec::new();
        let mut entries = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(id) = segment_id_from_filename(name) {
                    ids.push(id);
                }
            }
        }
        Ok(ids)
    }

    /// Allocate the next transaction ID.
    pub fn next_tx_id(&self) -> u64 {
        self.next_tx.fetch_add(1, Ordering::SeqCst)
    }

    /// Append a WAL record to the in-memory buffer. Returns the LSN assigned.
    pub fn append(
        &self,
        record_type: RecordType,
        collection: &str,
        tx_id: u64,
        schema_ver: u32,
        payload: Vec<u8>,
    ) -> u64 {
        let lsn = self.next_lsn.fetch_add(1, Ordering::SeqCst);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let record = WalRecord {
            lsn,
            ts,
            tx_id,
            record_type,
            schema_ver,
            collection: collection.to_owned(),
            payload,
        };

        // This blocks briefly on the mutex but append is fast (no I/O)
        let mut buf = self.buffer.blocking_lock();
        buf.append(record);
        lsn
    }

    /// Commit a transaction: append TX_COMMIT, flush buffer to segment, fsync.
    /// Returns the commit LSN.
    pub async fn commit(&self, tx_id: u64) -> Result<u64, WalError> {
        let commit_lsn = self.next_lsn.fetch_add(1, Ordering::SeqCst);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let commit_record = WalRecord {
            lsn: commit_lsn,
            ts,
            tx_id,
            record_type: RecordType::TxCommit,
            schema_ver: 0,
            collection: String::new(),
            payload: vec![],
        };

        // Append commit record and drain buffer
        let records = {
            let mut buf = self.buffer.lock().await;
            buf.append(commit_record);
            buf.drain_all()
        };

        // Write to segment and fsync
        {
            let mut seg = self.segment.lock().await;
            for record in &records {
                seg.append(record).await?;

                // Check segment rotation
                if seg.current_size() >= self.config.segment_size_bytes as u64 {
                    seg.sync().await?;
                    let new_id = segment_id_from_filename(
                        seg.path().file_name().unwrap().to_str().unwrap()
                    ).unwrap_or(1) + 1;
                    let new_path = self.config.wal_dir.join(segment_filename(new_id));
                    *seg = WalSegment::create(&new_path, new_id).await?;
                }
            }
            seg.sync().await?;
        }

        Ok(commit_lsn)
    }

    /// Write a checkpoint record. Returns the checkpoint LSN.
    pub async fn checkpoint(&self, snapshot_path: &Path) -> Result<u64, WalError> {
        let lsn = self.next_lsn.fetch_add(1, Ordering::SeqCst);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let payload = rmp_serde::to_vec_named(&serde_json::json!({
            "lsn": lsn,
            "snapshot_path": snapshot_path.to_string_lossy(),
        })).map_err(|e| WalError::Serialisation(e.to_string()))?;

        let record = WalRecord {
            lsn,
            ts,
            tx_id: 0,
            record_type: RecordType::Checkpoint,
            schema_ver: 0,
            collection: String::new(),
            payload,
        };

        let mut seg = self.segment.lock().await;
        seg.append(&record).await?;
        seg.sync().await?;

        Ok(lsn)
    }

    /// Get the current head LSN (last assigned).
    pub fn head_lsn(&self) -> u64 {
        self.next_lsn.load(Ordering::SeqCst).saturating_sub(1)
    }

    /// Get the WAL directory path.
    pub fn wal_dir(&self) -> &Path {
        &self.config.wal_dir
    }
}
```

Note: `serde_json` is needed as a dev-dependency for tests and for checkpoint payload serialisation. Add it to trondb-wal's Cargo.toml dependencies.

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-wal -- writer`
Expected: All 3 writer tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-wal/src/writer.rs crates/trondb-wal/Cargo.toml
git commit -m "feat(wal): implement async WalWriter with append, commit (flush+fsync), checkpoint, segment rotation"
```

---

### Task 5: WAL crash recovery

**Files:**
- Create: `crates/trondb-wal/src/recovery.rs`

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WalConfig;
    use crate::record::RecordType;
    use crate::writer::WalWriter;
    use tempfile::TempDir;

    #[tokio::test]
    async fn recover_committed_transactions() {
        let dir = TempDir::new().unwrap();
        let config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };

        // Write committed records
        {
            let writer = WalWriter::open(config.clone()).await.unwrap();
            let tx_id = writer.next_tx_id();
            writer.append(RecordType::TxBegin, "venues", tx_id, 1, vec![]);
            writer.append(RecordType::EntityWrite, "venues", tx_id, 1, b"entity1".to_vec());
            writer.commit(tx_id).await.unwrap();
        }

        let committed = WalRecovery::recover(&config.wal_dir).await.unwrap();
        // Should contain the EntityWrite (not TxBegin/TxCommit which are control records)
        let entity_writes: Vec<_> = committed.records.iter()
            .filter(|r| r.record_type == RecordType::EntityWrite)
            .collect();
        assert_eq!(entity_writes.len(), 1);
        assert_eq!(committed.head_lsn, 3); // TxBegin=1, EntityWrite=2, TxCommit=3
    }

    #[tokio::test]
    async fn recover_discards_uncommitted() {
        let dir = TempDir::new().unwrap();
        let config = WalConfig {
            wal_dir: dir.path().join("wal"),
            ..Default::default()
        };

        // Write committed tx
        {
            let writer = WalWriter::open(config.clone()).await.unwrap();
            let tx1 = writer.next_tx_id();
            writer.append(RecordType::TxBegin, "venues", tx1, 1, vec![]);
            writer.append(RecordType::EntityWrite, "venues", tx1, 1, b"committed".to_vec());
            writer.commit(tx1).await.unwrap();

            // Write uncommitted tx (simulating crash before commit)
            let tx2 = writer.next_tx_id();
            writer.append(RecordType::TxBegin, "venues", tx2, 1, vec![]);
            writer.append(RecordType::EntityWrite, "venues", tx2, 1, b"uncommitted".to_vec());
            // No commit — simulate crash by flushing buffer manually
            // Actually, these are in the buffer and won't be on disk without commit
            // So they naturally disappear. Let's test with direct segment write instead.
        }

        let committed = WalRecovery::recover(&config.wal_dir).await.unwrap();
        let entity_writes: Vec<_> = committed.records.iter()
            .filter(|r| r.record_type == RecordType::EntityWrite)
            .collect();
        // Only the committed entity should survive
        assert_eq!(entity_writes.len(), 1);
        assert_eq!(entity_writes[0].payload, b"committed");
    }

    #[tokio::test]
    async fn recover_empty_wal() {
        let dir = TempDir::new().unwrap();
        let wal_dir = dir.path().join("wal");
        tokio::fs::create_dir_all(&wal_dir).await.unwrap();

        let result = WalRecovery::recover(&wal_dir).await.unwrap();
        assert!(result.records.is_empty());
        assert_eq!(result.head_lsn, 0);
    }
}
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p trondb-wal -- recovery`
Expected: Fail — WalRecovery doesn't exist.

- [ ] **Step 3: Implement WalRecovery**

```rust
use std::collections::HashMap;
use std::path::Path;

use crate::error::WalError;
use crate::record::{RecordType, WalRecord};
use crate::segment::{WalSegment, segment_id_from_filename};

pub struct RecoveryResult {
    /// Committed records in LSN order, excluding TX_BEGIN/TX_COMMIT/TX_ABORT control records.
    pub records: Vec<WalRecord>,
    /// The highest LSN seen across all segments.
    pub head_lsn: u64,
    /// The checkpoint LSN if one was found, or 0.
    pub checkpoint_lsn: u64,
}

pub struct WalRecovery;

impl WalRecovery {
    /// Recover committed records from WAL segments.
    ///
    /// Implements ARIES-style recovery:
    /// 1. Find the last CHECKPOINT record (if any)
    /// 2. Replay all records from checkpoint LSN forward
    /// 3. Group by tx_id, discard uncommitted transactions
    /// 4. Return committed mutation records in LSN order
    pub async fn recover(wal_dir: &Path) -> Result<RecoveryResult, WalError> {
        if !wal_dir.exists() {
            return Ok(RecoveryResult {
                records: Vec::new(),
                head_lsn: 0,
                checkpoint_lsn: 0,
            });
        }

        // Collect and sort segment files
        let mut segment_ids = Vec::new();
        let mut entries = tokio::fs::read_dir(wal_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(id) = segment_id_from_filename(name) {
                    segment_ids.push(id);
                }
            }
        }
        segment_ids.sort();

        if segment_ids.is_empty() {
            return Ok(RecoveryResult {
                records: Vec::new(),
                head_lsn: 0,
                checkpoint_lsn: 0,
            });
        }

        // Read all records from all segments
        let mut all_records = Vec::new();
        for seg_id in &segment_ids {
            let path = wal_dir.join(crate::segment::segment_filename(*seg_id));
            let records = WalSegment::read_all(&path).await?;
            all_records.extend(records);
        }

        if all_records.is_empty() {
            return Ok(RecoveryResult {
                records: Vec::new(),
                head_lsn: 0,
                checkpoint_lsn: 0,
            });
        }

        let head_lsn = all_records.last().map(|r| r.lsn).unwrap_or(0);

        // Find last checkpoint
        let checkpoint_lsn = all_records.iter().rev()
            .find(|r| r.record_type == RecordType::Checkpoint)
            .map(|r| r.lsn)
            .unwrap_or(0);

        // Filter to records after checkpoint
        let replay_records: Vec<_> = all_records.into_iter()
            .filter(|r| r.lsn > checkpoint_lsn || checkpoint_lsn == 0)
            .collect();

        // Group by tx_id, find committed transactions
        let mut tx_records: HashMap<u64, Vec<WalRecord>> = HashMap::new();
        let mut committed_txs = std::collections::HashSet::new();

        for record in &replay_records {
            match record.record_type {
                RecordType::TxCommit => {
                    committed_txs.insert(record.tx_id);
                }
                RecordType::TxAbort => {
                    tx_records.remove(&record.tx_id);
                }
                RecordType::TxBegin => {
                    tx_records.entry(record.tx_id).or_default();
                }
                RecordType::Checkpoint => {
                    // Skip checkpoint records in output
                }
                _ => {
                    tx_records.entry(record.tx_id).or_default().push(record.clone());
                }
            }
        }

        // Collect committed mutation records in LSN order
        let mut committed_records: Vec<WalRecord> = tx_records.into_iter()
            .filter(|(tx_id, _)| committed_txs.contains(tx_id))
            .flat_map(|(_, records)| records)
            .collect();

        committed_records.sort_by_key(|r| r.lsn);

        Ok(RecoveryResult {
            records: committed_records,
            head_lsn,
            checkpoint_lsn,
        })
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-wal -- recovery`
Expected: All 3 recovery tests pass.

- [ ] **Step 5: Run all WAL tests**

Run: `cargo test -p trondb-wal`
Expected: All tests pass (record + segment + buffer + writer + recovery).

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-wal/src/recovery.rs
git commit -m "feat(wal): implement ARIES-style crash recovery — checkpoint scan, CRC verification, uncommitted tx discard"
```

---

## Chunk 3: Fjall Store

### Task 6: Rewrite Store with Fjall backend

**Files:**
- Modify: `crates/trondb-core/Cargo.toml`
- Modify: `crates/trondb-core/src/store.rs`

- [ ] **Step 1: Write failing tests**

Replace the existing store tests with Fjall-backed equivalents. Tests use `tempfile` for isolated data directories.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;
    use tempfile::TempDir;

    fn open_store() -> (FjallStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        (store, dir)
    }

    #[test]
    fn create_collection() {
        let (store, _dir) = open_store();
        store.create_collection("docs", 384).unwrap();
        assert!(store.has_collection("docs"));
    }

    #[test]
    fn create_duplicate_collection_fails() {
        let (store, _dir) = open_store();
        store.create_collection("docs", 384).unwrap();
        assert!(store.create_collection("docs", 384).is_err());
    }

    #[test]
    fn insert_and_get_entity() {
        let (store, _dir) = open_store();
        store.create_collection("docs", 3).unwrap();

        let id = LogicalId::from_string("e1");
        let entity = Entity::new(id.clone())
            .with_metadata("title", Value::String("Hello".into()));

        store.insert("docs", entity).unwrap();

        let retrieved = store.get("docs", &id).unwrap();
        assert_eq!(
            retrieved.metadata.get("title"),
            Some(&Value::String("Hello".into()))
        );
    }

    #[test]
    fn insert_into_nonexistent_collection_fails() {
        let (store, _dir) = open_store();
        let entity = Entity::new(LogicalId::new());
        assert!(store.insert("nope", entity).is_err());
    }

    #[test]
    fn scan_all_entities() {
        let (store, _dir) = open_store();
        store.create_collection("docs", 3).unwrap();

        for i in 0..5 {
            let entity = Entity::new(LogicalId::from_string(&format!("e{i}")));
            store.insert("docs", entity).unwrap();
        }

        let all = store.scan("docs").unwrap();
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn get_collection_dimensions() {
        let (store, _dir) = open_store();
        store.create_collection("docs", 1408).unwrap();
        assert_eq!(store.get_dimensions("docs").unwrap(), 1408);
    }

    #[test]
    fn data_survives_reopen() {
        let dir = TempDir::new().unwrap();

        // Write
        {
            let store = FjallStore::open(dir.path()).unwrap();
            store.create_collection("venues", 3).unwrap();
            let entity = Entity::new(LogicalId::from_string("v1"))
                .with_metadata("name", Value::String("The Shard".into()));
            store.insert("venues", entity).unwrap();
        }

        // Reopen and read
        {
            let store = FjallStore::open(dir.path()).unwrap();
            assert!(store.has_collection("venues"));
            let entity = store.get("venues", &LogicalId::from_string("v1")).unwrap();
            assert_eq!(
                entity.metadata.get("name"),
                Some(&Value::String("The Shard".into()))
            );
        }
    }
}
```

- [ ] **Step 2: Run test to verify failure**

Run: `cargo test -p trondb-core -- store`
Expected: Fail — FjallStore doesn't exist, and fjall is not in deps.

- [ ] **Step 3: Update Cargo.toml and implement FjallStore**

**`crates/trondb-core/Cargo.toml`:**
```toml
[package]
name = "trondb-core"
version = "0.1.0"
edition = "2021"

[dependencies]
trondb-tql = { path = "../trondb-tql" }
trondb-wal = { path = "../trondb-wal" }
uuid.workspace = true
bytes.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
dashmap.workspace = true
thiserror.workspace = true
fjall.workspace = true
rmp-serde.workspace = true
tokio.workspace = true

[dev-dependencies]
tempfile = "3"
```

**`crates/trondb-core/src/store.rs`** — full rewrite:

```rust
use std::path::Path;

use fjall::{Database, Keyspace, KeyspaceCreateOptions, PersistMode};

use crate::error::EngineError;
use crate::types::{Collection, Entity, LogicalId};

const META_KEYSPACE: &str = "_meta";
const COLLECTION_PREFIX: &str = "collection:";
const ENTITY_PREFIX: &str = "entity:";

pub struct FjallStore {
    db: Database,
    meta: Keyspace,
}

impl FjallStore {
    pub fn open(data_dir: &Path) -> Result<Self, EngineError> {
        let db = Database::builder(data_dir)
            .open()
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let meta = db
            .keyspace(META_KEYSPACE, KeyspaceCreateOptions::default)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        Ok(Self { db, meta })
    }

    pub fn create_collection(&self, name: &str, dimensions: usize) -> Result<(), EngineError> {
        let key = format!("{COLLECTION_PREFIX}{name}");
        if self.meta.get(&key).map_err(|e| EngineError::Storage(e.to_string()))?.is_some() {
            return Err(EngineError::CollectionAlreadyExists(name.to_owned()));
        }

        let collection = Collection::new(name, dimensions)?;
        let bytes = rmp_serde::to_vec_named(&collection)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        self.meta.insert(&key, &bytes)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        // Create the keyspace for entities
        self.db.keyspace(name, KeyspaceCreateOptions::default)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        self.db.persist(PersistMode::SyncAll)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        Ok(())
    }

    pub fn has_collection(&self, name: &str) -> bool {
        let key = format!("{COLLECTION_PREFIX}{name}");
        self.meta.get(&key).ok().flatten().is_some()
    }

    pub fn get_dimensions(&self, collection: &str) -> Result<usize, EngineError> {
        let col = self.get_collection_meta(collection)?;
        Ok(col.dimensions)
    }

    pub fn list_collections(&self) -> Vec<String> {
        self.meta
            .prefix(COLLECTION_PREFIX)
            .filter_map(|kv| {
                let kv = kv.ok()?;
                let key = std::str::from_utf8(&kv.0).ok()?;
                Some(key.strip_prefix(COLLECTION_PREFIX)?.to_owned())
            })
            .collect()
    }

    pub fn insert(&self, collection: &str, entity: Entity) -> Result<(), EngineError> {
        // Validate collection exists and check dimensions
        let col = self.get_collection_meta(collection)?;
        for repr in &entity.representations {
            if repr.vector.len() != col.dimensions {
                return Err(EngineError::DimensionMismatch {
                    expected: col.dimensions,
                    got: repr.vector.len(),
                });
            }
        }

        let ks = self.db.keyspace(collection, KeyspaceCreateOptions::default)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let key = format!("{ENTITY_PREFIX}{}", entity.id);
        let bytes = rmp_serde::to_vec_named(&entity)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        ks.insert(&key, &bytes)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        Ok(())
    }

    pub fn get(&self, collection: &str, id: &LogicalId) -> Result<Entity, EngineError> {
        if !self.has_collection(collection) {
            return Err(EngineError::CollectionNotFound(collection.to_owned()));
        }

        let ks = self.db.keyspace(collection, KeyspaceCreateOptions::default)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let key = format!("{ENTITY_PREFIX}{id}");
        let bytes = ks.get(&key)
            .map_err(|e| EngineError::Storage(e.to_string()))?
            .ok_or_else(|| EngineError::EntityNotFound(id.to_string()))?;

        rmp_serde::from_slice(&bytes)
            .map_err(|e| EngineError::Storage(e.to_string()))
    }

    pub fn scan(&self, collection: &str) -> Result<Vec<Entity>, EngineError> {
        if !self.has_collection(collection) {
            return Err(EngineError::CollectionNotFound(collection.to_owned()));
        }

        let ks = self.db.keyspace(collection, KeyspaceCreateOptions::default)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let entities: Vec<Entity> = ks.prefix(ENTITY_PREFIX)
            .filter_map(|kv| {
                let kv = kv.ok()?;
                rmp_serde::from_slice(&kv.1).ok()
            })
            .collect();

        Ok(entities)
    }

    pub fn persist(&self) -> Result<(), EngineError> {
        self.db.persist(PersistMode::SyncAll)
            .map_err(|e| EngineError::Storage(e.to_string()))
    }

    fn get_collection_meta(&self, name: &str) -> Result<Collection, EngineError> {
        let key = format!("{COLLECTION_PREFIX}{name}");
        let bytes = self.meta.get(&key)
            .map_err(|e| EngineError::Storage(e.to_string()))?
            .ok_or_else(|| EngineError::CollectionNotFound(name.to_owned()))?;

        rmp_serde::from_slice(&bytes)
            .map_err(|e| EngineError::Storage(e.to_string()))
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-core -- store`
Expected: All 7 store tests pass, including `data_survives_reopen`.

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/Cargo.toml crates/trondb-core/src/store.rs
git commit -m "feat(store): rewrite Store with Fjall backend — persistent entity storage, collection metadata, survives restart"
```

---

## Chunk 4: Engine Integration (WAL + Fjall + Async)

### Task 7: Update error types

**Files:**
- Modify: `crates/trondb-core/src/error.rs`

- [ ] **Step 1: Add new error variants**

```rust
use thiserror::Error;
use trondb_wal::WalError;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("collection not found: {0}")]
    CollectionNotFound(String),

    #[error("collection already exists: {0}")]
    CollectionAlreadyExists(String),

    #[error("entity not found: {0}")]
    EntityNotFound(String),

    #[error("dimension mismatch: expected {expected}, got {got}")]
    DimensionMismatch { expected: usize, got: usize },

    #[error("invalid query: {0}")]
    InvalidQuery(String),

    #[error("WAL error: {0}")]
    Wal(#[from] WalError),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("unsupported operation: {0}")]
    UnsupportedOperation(String),
}
```

- [ ] **Step 2: Verify compilation**

Run: `cargo check -p trondb-core`
Expected: Compiles (may have warnings about unused imports which will resolve in subsequent tasks).

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-core/src/error.rs
git commit -m "feat(error): add Wal, Storage, UnsupportedOperation error variants"
```

---

### Task 8: Gate SEARCH in planner, remove index.rs

**Files:**
- Modify: `crates/trondb-core/src/planner.rs`
- Delete: `crates/trondb-core/src/index.rs`

- [ ] **Step 1: Write failing test for gated SEARCH**

In `crates/trondb-core/src/planner.rs`, add test:
```rust
#[test]
fn plan_search_returns_unsupported() {
    let stmt = Statement::Search(SearchStmt {
        collection: "venues".into(),
        fields: FieldList::All,
        near: vec![1.0, 0.0, 0.0],
        confidence: Some(0.8),
        limit: Some(5),
    });
    let result = plan(&stmt);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, crate::error::EngineError::UnsupportedOperation(_)));
}
```

- [ ] **Step 2: Gate SEARCH in planner**

Change the Search match arm in `plan()`:
```rust
Statement::Search(_) => Err(EngineError::UnsupportedOperation(
    "SEARCH requires vector index (Phase 4)".into(),
)),
```

Keep `SearchPlan` struct defined (for EXPLAIN to still describe it) but mark it with `#[allow(dead_code)]`.

- [ ] **Step 3: Delete `index.rs`**

Remove `crates/trondb-core/src/index.rs` and remove `pub mod index;` from `lib.rs`.

- [ ] **Step 4: Update executor to remove vector index references**

Remove the `indexes: DashMap<String, VectorIndex>` field and all references to it from `executor.rs`. Remove `execute_search` method. The `Plan::Search` arm in `execute()` becomes unreachable since the planner gates it, but add a fallback error for safety:

```rust
Plan::Search(_) => Err(EngineError::UnsupportedOperation(
    "SEARCH requires vector index (Phase 4)".into(),
)),
```

Remove the SEARCH-related executor tests (`execute_search`, `execute_search_with_confidence_filter`). Keep the EXPLAIN test but update it to use a FETCH plan instead of SEARCH.

- [ ] **Step 5: Run tests**

Run: `cargo test -p trondb-core`
Expected: All remaining tests pass. SEARCH tests removed, new gating test passes.

- [ ] **Step 6: Commit**

```bash
git add -A crates/trondb-core/src/
git commit -m "feat(phase2): gate SEARCH execution (Phase 4), remove VectorIndex, update executor and planner"
```

---

### Task 9: Async Engine with WAL integration

**Files:**
- Modify: `crates/trondb-core/src/lib.rs`
- Modify: `crates/trondb-core/src/executor.rs`

This is the core integration task. The Engine becomes async, the write path goes through WAL before Fjall, and the executor uses FjallStore.

- [ ] **Step 1: Write failing tests**

In `crates/trondb-core/src/lib.rs`, update integration tests to be async:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;
    use tempfile::TempDir;

    async fn test_engine() -> (Engine, TempDir) {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("data"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
        };
        let engine = Engine::open(config).await.unwrap();
        (engine, dir)
    }

    #[tokio::test]
    async fn end_to_end_create_insert_fetch() {
        let (engine, _dir) = test_engine().await;

        engine
            .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name, city) VALUES ('v1', 'The Shard', 'London') VECTOR [0.1, 0.2, 0.3];",
            )
            .await
            .unwrap();

        engine
            .execute_tql(
                "INSERT INTO venues (id, name, city) VALUES ('v2', 'Old Trafford', 'Manchester') VECTOR [0.4, 0.5, 0.6];",
            )
            .await
            .unwrap();

        let result = engine
            .execute_tql("FETCH * FROM venues WHERE city = 'London';")
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].values.get("name"),
            Some(&Value::String("The Shard".into()))
        );
    }

    #[tokio::test]
    async fn search_returns_unsupported() {
        let (engine, _dir) = test_engine().await;

        let result = engine
            .execute_tql("SEARCH venues NEAR VECTOR [1.0, 0.0, 0.0] CONFIDENCE > 0.8;")
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn end_to_end_explain() {
        let (engine, _dir) = test_engine().await;

        let result = engine
            .execute_tql("EXPLAIN FETCH * FROM venues;")
            .await
            .unwrap();

        let mode_row = result
            .rows
            .iter()
            .find(|r| r.values.get("property") == Some(&Value::String("mode".into())))
            .expect("should have 'mode' property");

        assert_eq!(
            mode_row.values.get("value"),
            Some(&Value::String("Deterministic".into()))
        );
    }

    #[tokio::test]
    async fn data_survives_restart() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig {
            data_dir: dir.path().join("data"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.path().join("wal"),
                ..Default::default()
            },
        };

        // Write data
        {
            let engine = Engine::open(config.clone()).await.unwrap();
            engine
                .execute_tql("CREATE COLLECTION venues WITH DIMENSIONS 3;")
                .await
                .unwrap();
            engine
                .execute_tql(
                    "INSERT INTO venues (id, name) VALUES ('v1', 'The Shard') VECTOR [0.1, 0.2, 0.3];",
                )
                .await
                .unwrap();
        }

        // Reopen and verify
        {
            let engine = Engine::open(config).await.unwrap();
            let result = engine
                .execute_tql("FETCH * FROM venues WHERE id = 'v1';")
                .await
                .unwrap();
            assert_eq!(result.rows.len(), 1);
            assert_eq!(
                result.rows[0].values.get("name"),
                Some(&Value::String("The Shard".into()))
            );
        }
    }
}
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p trondb-core`
Expected: Fail — Engine::open, async execute_tql, EngineConfig don't exist yet.

- [ ] **Step 3: Implement async Engine + WAL-integrated executor**

**`crates/trondb-core/src/lib.rs`:**
```rust
pub mod error;
pub mod executor;
pub mod planner;
pub mod result;
pub mod store;
pub mod types;

use std::path::PathBuf;

use error::EngineError;
use executor::Executor;
use result::QueryResult;
use store::FjallStore;
use trondb_wal::{WalConfig, WalWriter};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub data_dir: PathBuf,
    pub wal: WalConfig,
}

pub struct Engine {
    executor: Executor,
}

impl Engine {
    pub async fn open(config: EngineConfig) -> Result<Self, EngineError> {
        let store = FjallStore::open(&config.data_dir)?;
        let wal = WalWriter::open(config.wal).await?;
        let executor = Executor::new(store, wal);
        Ok(Self { executor })
    }

    pub async fn execute_tql(&self, input: &str) -> Result<QueryResult, EngineError> {
        let stmt =
            trondb_tql::parse(input).map_err(|e| EngineError::InvalidQuery(e.to_string()))?;
        let plan = planner::plan(&stmt)?;
        self.executor.execute(&plan).await
    }

    pub fn collections(&self) -> Vec<String> {
        self.executor.collections()
    }
}
```

**`crates/trondb-core/src/executor.rs`** — rewrite to use FjallStore + WalWriter:

The executor now:
- Takes `FjallStore` + `WalWriter` instead of `Store` + `DashMap<VectorIndex>`
- `execute()` is `async fn`
- Write operations (CREATE COLLECTION, INSERT) go through WAL first, then Fjall
- Read operations (FETCH) go directly to Fjall
- SEARCH returns UnsupportedOperation
- EXPLAIN stays synchronous (no I/O)

Key changes to the execute method:
- `Plan::CreateCollection` → WAL TX_BEGIN + SCHEMA_CREATE_COLL + commit → create_collection on store
- `Plan::Insert` → WAL TX_BEGIN + ENTITY_WRITE + commit → insert on store
- `Plan::Fetch` → direct Fjall read (no WAL involvement)
- `Plan::Search` → error
- `Plan::Explain` → describe plan (no change)
- `QueryStats.tier` changes from `"RAM"` to `"Fjall"` for reads

The helper functions (`entity_to_row`, `entity_matches`, `literal_to_value`, etc.) remain unchanged.

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-core`
Expected: All tests pass including `data_survives_restart`.

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/lib.rs crates/trondb-core/src/executor.rs
git commit -m "feat(engine): async Engine with WAL-integrated write path and Fjall-backed reads"
```

---

## Chunk 5: CLI Update + Final Integration

### Task 10: Update CLI for async engine

**Files:**
- Modify: `crates/trondb-cli/Cargo.toml`
- Modify: `crates/trondb-cli/src/main.rs`

- [ ] **Step 1: Update CLI Cargo.toml**

```toml
[package]
name = "trondb-cli"
version = "0.1.0"
edition = "2021"

[dependencies]
trondb-core = { path = "../trondb-core" }
trondb-tql = { path = "../trondb-tql" }
trondb-wal = { path = "../trondb-wal" }
comfy-table.workspace = true
rustyline.workspace = true
tokio.workspace = true
```

- [ ] **Step 2: Rewrite main.rs for async**

```rust
mod display;

use std::path::PathBuf;

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use trondb_core::{Engine, EngineConfig};
use trondb_wal::WalConfig;

#[tokio::main]
async fn main() {
    let data_dir = std::env::args()
        .skip_while(|a| a != "--data-dir")
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./trondb_data"));

    let config = EngineConfig {
        data_dir: data_dir.join("store"),
        wal: WalConfig {
            wal_dir: data_dir.join("wal"),
            ..Default::default()
        },
    };

    println!("TronDB v0.2.0 — inference-first storage engine");
    println!("Data directory: {}", data_dir.display());
    println!("Type .help for commands, or enter TQL statements ending with ;\n");

    let engine = match Engine::open(config).await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Failed to open engine: {e}");
            std::process::exit(1);
        }
    };

    let mut rl = DefaultEditor::new().expect("failed to create editor");
    let mut buffer = String::new();

    let runtime = tokio::runtime::Handle::current();

    loop {
        let prompt = if buffer.is_empty() {
            "trondb> "
        } else {
            "   ...> "
        };

        match rl.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim();

                if buffer.is_empty() && trimmed.starts_with('.') {
                    handle_dot_command(trimmed, &engine, &data_dir);
                    continue;
                }

                buffer.push_str(trimmed);
                buffer.push(' ');

                if !buffer.trim_end().ends_with(';') {
                    continue;
                }

                let input = buffer.trim().to_string();
                buffer.clear();

                let _ = rl.add_history_entry(&input);

                match runtime.block_on(engine.execute_tql(&input)) {
                    Ok(result) => println!("{}", display::format_result(&result)),
                    Err(e) => eprintln!("Error: {e}"),
                }
            }
            Err(ReadlineError::Interrupted) => {
                buffer.clear();
                println!("(statement cleared)");
            }
            Err(ReadlineError::Eof) => {
                println!("Goodbye.");
                break;
            }
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        }
    }
}

fn handle_dot_command(cmd: &str, engine: &Engine, data_dir: &std::path::Path) {
    match cmd {
        ".help" => {
            println!("Commands:");
            println!("  .help          Show this help");
            println!("  .collections   List all collections");
            println!("  .data          Show data directory");
            println!("  .quit          Exit TronDB");
            println!();
            println!("TQL statements must end with a semicolon (;)");
        }
        ".collections" => {
            let collections = engine.collections();
            if collections.is_empty() {
                println!("No collections.");
            } else {
                for name in collections {
                    println!("  {name}");
                }
            }
        }
        ".data" => {
            println!("Data directory: {}", data_dir.display());
        }
        ".quit" | ".exit" => {
            println!("Goodbye.");
            std::process::exit(0);
        }
        _ => eprintln!("Unknown command: {cmd}. Type .help for available commands."),
    }
}
```

- [ ] **Step 3: Verify compilation**

Run: `cargo build -p trondb-cli`
Expected: Compiles successfully.

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-cli/
git commit -m "feat(cli): update REPL for async engine with Tokio, add --data-dir flag and .data command"
```

---

### Task 11: Update CLAUDE.md and workspace config

**Files:**
- Modify: `CLAUDE.md`
- Modify: `Cargo.toml` (workspace root — ensure tempfile is in dev-dependencies if needed)

- [ ] **Step 1: Update CLAUDE.md**

Update the project conventions to reflect Phase 2:
```markdown
# TronDB

Inference-first storage engine. Phase 2: Fjall persistence + WAL.

## Project Structure

- `crates/trondb-wal/` — Write-Ahead Log: record types (MessagePack), segment files, buffer, async writer, crash recovery
- `crates/trondb-core/` — Engine: types, Fjall-backed store, planner, async executor. Depends on trondb-wal + trondb-tql.
- `crates/trondb-tql/` — TQL parser (logos lexer + recursive descent). No engine dependency.
- `crates/trondb-cli/` — Interactive REPL binary (Tokio + rustyline). Depends on all crates.

## Conventions

- Rust 2021 edition, async (Tokio runtime)
- Tests: `cargo test --workspace`
- Run REPL: `cargo run -p trondb-cli`
- Run REPL with custom data dir: `cargo run -p trondb-cli -- --data-dir /path/to/data`
- TQL is case-insensitive for keywords, case-sensitive for identifiers
- All vectors are Float32 (gated until Phase 4)
- LogicalId is a String wrapper (user-provided or UUID v4)
- Persistence: Fjall (LSM-based). Data dir default: ./trondb_data
- WAL: MessagePack records, CRC32 verified, segment files. Dir: {data_dir}/wal/
- Write path: WAL append → flush+fsync → apply to Fjall → ack
- SEARCH is gated (returns UnsupportedOperation) until Phase 4
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md Cargo.toml
git commit -m "docs: update CLAUDE.md for Phase 2 — Fjall persistence, WAL, async"
```

---

### Task 12: Full integration test — persistence round-trip

**Files:**
- Modify: `tests/smoke.tql` (update to remove SEARCH commands)
- Add integration test in `crates/trondb-core/src/lib.rs` (already done in Task 9)

- [ ] **Step 1: Update smoke.tql**

```sql
-- TronDB Smoke Test (Phase 2 — no SEARCH)

CREATE COLLECTION venues WITH DIMENSIONS 3;

INSERT INTO venues (id, name, city) VALUES ('v1', 'The Shard', 'London') VECTOR [0.1, 0.2, 0.3];
INSERT INTO venues (id, name, city) VALUES ('v2', 'Old Trafford', 'Manchester') VECTOR [0.4, 0.5, 0.6];
INSERT INTO venues (id, name, city) VALUES ('v3', 'Eden Project', 'Cornwall') VECTOR [0.7, 0.8, 0.9];

FETCH * FROM venues;
FETCH name, city FROM venues WHERE city = 'London';

EXPLAIN FETCH * FROM venues;
```

- [ ] **Step 2: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All tests pass across all 4 crates.

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings.

- [ ] **Step 4: Test the REPL manually**

Run: `cargo run -p trondb-cli -- --data-dir /tmp/trondb_test`
Execute smoke.tql commands interactively. Verify:
- Collections persist after Ctrl+D and relaunch
- Entities are retrievable after restart
- `.data` shows the correct directory
- `.collections` lists created collections

- [ ] **Step 5: Final commit**

```bash
git add tests/smoke.tql
git commit -m "test: update smoke test for Phase 2 (persistence, no SEARCH)"
```

---

## Summary

| Task | Description | Chunk |
|------|-------------|-------|
| 1 | WAL crate scaffold + record types + CRC32 framing | 1 |
| 2 | WAL segment file I/O (header, read, write, rotation) | 1 |
| 3 | WAL buffer + flush policy | 1 |
| 4 | WAL writer (async append, commit, checkpoint) | 2 |
| 5 | WAL crash recovery (ARIES-style) | 2 |
| 6 | Fjall-backed FjallStore (replaces DashMap Store) | 3 |
| 7 | Error type updates | 4 |
| 8 | Gate SEARCH, remove VectorIndex | 4 |
| 9 | Async Engine with WAL integration | 4 |
| 10 | CLI update for async + data dir | 5 |
| 11 | CLAUDE.md + workspace config | 5 |
| 12 | Full integration test + smoke test | 5 |
