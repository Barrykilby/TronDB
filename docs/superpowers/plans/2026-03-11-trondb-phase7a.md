# Phase 7a: Tiered Storage + Vector Quantisation — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add warm/cold storage tiers with Int8/Binary vector quantisation, a background migration pipeline, and automatic promotion/demotion driven by access patterns.

**Architecture:** All tiers use Fjall as the backend — tiers are logical, distinguished by partition naming (`warm.{collection}`, `archive.{collection}`) and vector encoding (Int8, Binary). The Location Table (already built) tracks tier/encoding per representation. A new `TierMigrator` background task handles demotion; promotion is triggered on access during FETCH.

**Tech Stack:** Rust 2021, Tokio, Fjall (LSM), DashMap, hnsw_rs, MessagePack (rmp-serde)

**Spec:** `docs/superpowers/specs/2026-03-11-trondb-phase7a-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/trondb-core/src/quantise.rs` | Create | Int8/Binary quantisation, dequantisation, serialisation |
| `crates/trondb-core/src/index.rs` | Modify | HNSW tombstone removal, rebuild, tombstone tracking |
| `crates/trondb-core/src/location.rs` | Modify | Add `last_accessed` to LocationDescriptor (serde(default)) |
| `crates/trondb-core/src/store.rs` | Modify | Tiered partition methods: open, read, write, delete, count |
| `crates/trondb-core/src/executor.rs` | Modify | Tier-aware FETCH path, DEMOTE/PROMOTE/EXPLAIN TIERS handlers |
| `crates/trondb-core/src/lib.rs` | Modify | Engine methods: tier ops, HNSW remove/insert, location_table |
| `crates/trondb-core/src/planner.rs` | Modify | Demote, Promote, ExplainTiers plan variants |
| `crates/trondb-tql/src/token.rs` | Modify | DEMOTE, PROMOTE, TIERS, WARM, ARCHIVE tokens |
| `crates/trondb-tql/src/ast.rs` | Modify | DemoteStmt, PromoteStmt, ExplainTiersStmt, TierTarget |
| `crates/trondb-tql/src/parser.rs` | Modify | Parse DEMOTE/PROMOTE/EXPLAIN TIERS statements |
| `crates/trondb-wal/src/record.rs` | Modify | TierMigration = 0x70 record type |
| `crates/trondb-routing/src/config.rs` | Modify | TierConfig struct |
| `crates/trondb-routing/src/migrator.rs` | Create | TierMigrator background task: demotion cycle, promotion |
| `crates/trondb-routing/src/node.rs` | Modify | HealthSignal warm/archive counts |
| `crates/trondb-routing/src/health.rs` | Modify | HealthSignal warm/archive fields |
| `crates/trondb-routing/src/eviction.rs` | Modify | Bridge eviction to demotion |
| `crates/trondb-routing/src/router.rs` | Modify | Wire TierMigrator, handle Demote/Promote plans |
| `crates/trondb-routing/src/lib.rs` | Modify | Export migrator module |
| `CLAUDE.md` | Modify | Phase 7a documentation |

---

## Chunk 1: Foundation (Tasks 1-4)

### Task 1: Vector quantisation module

**Files:**
- Create: `crates/trondb-core/src/quantise.rs`
- Modify: `crates/trondb-core/src/lib.rs` (add `pub mod quantise;`)

- [ ] **Step 1: Write Int8 quantisation tests**

In `crates/trondb-core/src/quantise.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantise_int8_roundtrip() {
        let vector = vec![1.0, 0.5, 0.0, -0.5, -1.0];
        let q = quantise_int8(&vector);
        let restored = dequantise_int8(&q);
        for (a, b) in vector.iter().zip(restored.iter()) {
            assert!((a - b).abs() < 0.01, "expected {a}, got {b}");
        }
    }

    #[test]
    fn quantise_int8_constant_vector() {
        let vector = vec![3.0, 3.0, 3.0];
        let q = quantise_int8(&vector);
        let restored = dequantise_int8(&q);
        for v in &restored {
            assert!((v - 3.0).abs() < 0.01);
        }
    }

    #[test]
    fn quantise_int8_serialisation() {
        let vector = vec![1.0, -1.0, 0.5];
        let q = quantise_int8(&vector);
        let bytes = q.to_bytes();
        let q2 = QuantisedInt8::from_bytes(&bytes).unwrap();
        assert_eq!(q.min, q2.min);
        assert_eq!(q.max, q2.max);
        assert_eq!(q.data, q2.data);
    }

    #[test]
    fn quantise_binary_basic() {
        let vector = vec![1.0, -0.5, 0.0, -1.0, 0.5, -0.1, 0.9, -0.9];
        let q = quantise_binary(&vector);
        assert_eq!(q.dimensions, 8);
        // 1.0→1, -0.5→0, 0.0→1, -1.0→0, 0.5→1, -0.1→0, 0.9→1, -0.9→0
        // bits: 1 0 1 0 1 0 1 0 = 0xAA
        assert_eq!(q.data, vec![0xAA]);
    }

    #[test]
    fn quantise_binary_non_aligned() {
        // 3 dimensions → 1 byte, last 5 bits padded with 0
        let vector = vec![1.0, -1.0, 1.0];
        let q = quantise_binary(&vector);
        assert_eq!(q.dimensions, 3);
        // bits: 1 0 1 0 0 0 0 0 = 0xA0
        assert_eq!(q.data, vec![0xA0]);
    }

    #[test]
    fn quantise_binary_serialisation() {
        let vector = vec![1.0, -1.0, 0.5, -0.5];
        let q = quantise_binary(&vector);
        let bytes = q.to_bytes();
        let q2 = QuantisedBinary::from_bytes(&bytes, 4).unwrap();
        assert_eq!(q.data, q2.data);
        assert_eq!(q.dimensions, q2.dimensions);
    }

    #[test]
    fn quantise_int8_empty_vector() {
        let vector: Vec<f32> = vec![];
        let q = quantise_int8(&vector);
        assert!(q.data.is_empty());
        let restored = dequantise_int8(&q);
        assert!(restored.is_empty());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p trondb-core -- quantise`
Expected: FAIL — module doesn't exist yet

- [ ] **Step 3: Implement quantisation module**

```rust
use crate::error::EngineError;

// ---------------------------------------------------------------------------
// Int8 Scalar Quantisation
// ---------------------------------------------------------------------------

pub struct QuantisedInt8 {
    pub min: f32,
    pub max: f32,
    pub data: Vec<u8>,
}

pub fn quantise_int8(vector: &[f32]) -> QuantisedInt8 {
    if vector.is_empty() {
        return QuantisedInt8 { min: 0.0, max: 0.0, data: Vec::new() };
    }

    let min = vector.iter().copied().fold(f32::INFINITY, f32::min);
    let max = vector.iter().copied().fold(f32::NEG_INFINITY, f32::max);

    if (max - min).abs() < f32::EPSILON {
        // Constant vector — all zeros, min carries the value
        return QuantisedInt8 {
            min,
            max,
            data: vec![0u8; vector.len()],
        };
    }

    let scale = 255.0 / (max - min);
    let data: Vec<u8> = vector
        .iter()
        .map(|&v| ((v - min) * scale).round().clamp(0.0, 255.0) as u8)
        .collect();

    QuantisedInt8 { min, max, data }
}

pub fn dequantise_int8(q: &QuantisedInt8) -> Vec<f32> {
    if q.data.is_empty() {
        return Vec::new();
    }

    if (q.max - q.min).abs() < f32::EPSILON {
        return vec![q.min; q.data.len()];
    }

    let range = q.max - q.min;
    q.data
        .iter()
        .map(|&v| (v as f32 / 255.0) * range + q.min)
        .collect()
}

impl QuantisedInt8 {
    /// Serialise: [min: f32 LE][max: f32 LE][data: u8...]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8 + self.data.len());
        buf.extend_from_slice(&self.min.to_le_bytes());
        buf.extend_from_slice(&self.max.to_le_bytes());
        buf.extend_from_slice(&self.data);
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, EngineError> {
        if bytes.len() < 8 {
            return Err(EngineError::Storage("Int8 data too short".into()));
        }
        let min = f32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let max = f32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let data = bytes[8..].to_vec();
        Ok(Self { min, max, data })
    }
}

// ---------------------------------------------------------------------------
// Binary Quantisation
// ---------------------------------------------------------------------------

pub struct QuantisedBinary {
    pub data: Vec<u8>,
    pub dimensions: usize,
}

pub fn quantise_binary(vector: &[f32]) -> QuantisedBinary {
    let dimensions = vector.len();
    let byte_count = (dimensions + 7) / 8;
    let mut data = vec![0u8; byte_count];

    for (i, &v) in vector.iter().enumerate() {
        if v >= 0.0 {
            let byte_idx = i / 8;
            let bit_idx = 7 - (i % 8); // MSB-first
            data[byte_idx] |= 1 << bit_idx;
        }
    }

    QuantisedBinary { data, dimensions }
}

impl QuantisedBinary {
    pub fn to_bytes(&self) -> Vec<u8> {
        self.data.clone()
    }

    pub fn from_bytes(bytes: &[u8], dimensions: usize) -> Result<Self, EngineError> {
        let expected = (dimensions + 7) / 8;
        if bytes.len() < expected {
            return Err(EngineError::Storage("Binary data too short".into()));
        }
        Ok(Self {
            data: bytes[..expected].to_vec(),
            dimensions,
        })
    }
}
```

Add to `crates/trondb-core/src/lib.rs` after the existing `pub mod` lines:
```rust
pub mod quantise;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-core -- quantise`
Expected: all 7 tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/quantise.rs crates/trondb-core/src/lib.rs
git commit -m "feat: add Int8/Binary vector quantisation module"
```

---

### Task 2: HNSW tombstone removal + rebuild

**Files:**
- Modify: `crates/trondb-core/src/index.rs`

- [ ] **Step 1: Write tombstone removal tests**

Add to the existing `#[cfg(test)] mod tests` in `crates/trondb-core/src/index.rs`:

```rust
#[test]
fn remove_excludes_from_search() {
    let idx = HnswIndex::new(3);
    idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
    idx.insert(&make_id("e2"), &[0.0, 1.0, 0.0]);
    idx.insert(&make_id("e3"), &[0.9, 0.1, 0.0]);

    idx.remove(&make_id("e1"));

    let results = idx.search(&[1.0, 0.0, 0.0], 3);
    assert!(results.iter().all(|(id, _)| id != &make_id("e1")));
    assert_eq!(idx.len(), 2);
}

#[test]
fn remove_nonexistent_is_noop() {
    let idx = HnswIndex::new(3);
    idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
    idx.remove(&make_id("nonexistent"));
    assert_eq!(idx.len(), 1);
}

#[test]
fn tombstone_count_tracking() {
    let idx = HnswIndex::new(3);
    idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
    idx.insert(&make_id("e2"), &[0.0, 1.0, 0.0]);
    assert_eq!(idx.tombstone_count(), 0);
    idx.remove(&make_id("e1"));
    assert_eq!(idx.tombstone_count(), 1);
    assert!(idx.needs_rebuild()); // 1 tombstone, 1 live = 50% > 10%
}

#[test]
fn rebuild_clears_tombstones() {
    let idx = HnswIndex::new(3);
    idx.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
    idx.insert(&make_id("e2"), &[0.0, 1.0, 0.0]);
    idx.insert(&make_id("e3"), &[0.5, 0.5, 0.0]);
    idx.remove(&make_id("e1"));

    // Collect live data for rebuild
    let live_data: Vec<(LogicalId, Vec<f32>)> = vec![
        (make_id("e2"), vec![0.0, 1.0, 0.0]),
        (make_id("e3"), vec![0.5, 0.5, 0.0]),
    ];
    idx.rebuild(&live_data);

    assert_eq!(idx.len(), 2);
    assert_eq!(idx.tombstone_count(), 0);
    assert!(!idx.needs_rebuild());

    let results = idx.search(&[0.0, 1.0, 0.0], 2);
    assert_eq!(results[0].0, make_id("e2"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p trondb-core -- index::tests`
Expected: FAIL — `remove`, `tombstone_count`, `needs_rebuild`, `rebuild` don't exist

- [ ] **Step 3: Implement tombstone removal and rebuild**

Add a `tombstones` field to `HnswIndex` (line 24-30 of `index.rs`):

```rust
pub struct HnswIndex {
    inner: Mutex<Hnsw<'static, f32, DistCosine>>,
    dimensions: usize,
    id_to_idx: DashMap<LogicalId, usize>,
    idx_to_id: DashMap<usize, LogicalId>,
    next_idx: AtomicUsize,
    tombstones: DashMap<LogicalId, ()>,
    tombstone_count: AtomicUsize,
}
```

Update `HnswIndex::new()` to initialise the new fields:
```rust
tombstones: DashMap::new(),
tombstone_count: AtomicUsize::new(0),
```

Add methods after `dimensions()` (before the closing `}` of the impl block):

```rust
/// Mark an entity as removed. The HNSW graph retains the point
/// but it is excluded from search results.
pub fn remove(&self, id: &LogicalId) {
    if self.id_to_idx.contains_key(id) && !self.tombstones.contains_key(id) {
        self.tombstones.insert(id.clone(), ());
        self.tombstone_count.fetch_add(1, Ordering::SeqCst);
    }
}

pub fn tombstone_count(&self) -> usize {
    self.tombstone_count.load(Ordering::SeqCst)
}

/// Returns true when tombstone count exceeds 10% of live entries.
pub fn needs_rebuild(&self) -> bool {
    let live = self.len();
    let dead = self.tombstone_count();
    if live == 0 {
        return dead > 0;
    }
    dead * 10 > live // dead/total > 10% approximation
}

/// Rebuild the HNSW index from live data. Clears all tombstones.
/// Call this while holding an external read-consistency guarantee.
pub fn rebuild(&self, live_data: &[(LogicalId, Vec<f32>)]) {
    let hnsw = Hnsw::new(
        MAX_NB_CONNECTION,
        MAX_ELEMENTS,
        MAX_LAYER,
        EF_CONSTRUCTION,
        DistCosine,
    );

    self.id_to_idx.clear();
    self.idx_to_id.clear();
    self.tombstones.clear();
    self.tombstone_count.store(0, Ordering::SeqCst);
    self.next_idx.store(0, Ordering::SeqCst);

    for (id, vector) in live_data {
        let idx = self.next_idx.fetch_add(1, Ordering::SeqCst);
        self.id_to_idx.insert(id.clone(), idx);
        self.idx_to_id.insert(idx, id.clone());
        hnsw.insert((vector.as_slice(), idx));
    }

    *self.inner.lock().unwrap() = hnsw;
}
```

Modify `search()` to filter tombstones (change the `filter_map` closure):

```rust
.filter_map(|n| {
    let idx = n.d_id;
    let similarity = 1.0 - n.distance;
    self.idx_to_id.get(&idx).and_then(|id| {
        if self.tombstones.contains_key(id.value()) {
            None
        } else {
            Some((id.clone(), similarity))
        }
    })
})
```

Modify `len()` to subtract tombstones:

```rust
pub fn len(&self) -> usize {
    self.id_to_idx.len().saturating_sub(self.tombstone_count())
}
```

Modify `is_empty()`:

```rust
pub fn is_empty(&self) -> bool {
    self.len() == 0
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-core -- index::tests`
Expected: all tests pass (existing + 4 new)

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/index.rs
git commit -m "feat: add HNSW tombstone removal and periodic rebuild"
```

---

### Task 3: LocationDescriptor `last_accessed` field

**Files:**
- Modify: `crates/trondb-core/src/location.rs`

- [ ] **Step 1: Write test for last_accessed**

Add to the `#[cfg(test)] mod tests` in `crates/trondb-core/src/location.rs`:

```rust
#[test]
fn last_accessed_defaults_to_zero() {
    let lt = LocationTable::new();
    let key = make_key("e1", 0);
    lt.register(key.clone(), LocationDescriptor {
        tier: Tier::Fjall,
        node_address: NodeAddress::localhost(),
        state: LocState::Clean,
        version: 1,
        encoding: Encoding::Float32,
        last_accessed: 0,
    });
    let desc = lt.get(&key).unwrap();
    assert_eq!(desc.last_accessed, 0);
}

#[test]
fn last_accessed_survives_snapshot_roundtrip() {
    let lt = LocationTable::new();
    let key = make_key("e1", 0);
    lt.register(key.clone(), LocationDescriptor {
        tier: Tier::Fjall,
        node_address: NodeAddress::localhost(),
        state: LocState::Clean,
        version: 1,
        encoding: Encoding::Float32,
        last_accessed: 1710000000,
    });

    let snap = lt.snapshot(100).unwrap();
    let (restored, lsn) = LocationTable::restore(&snap).unwrap();
    assert_eq!(lsn, 100);
    let desc = restored.get(&key).unwrap();
    assert_eq!(desc.last_accessed, 1710000000);
}

#[test]
fn update_last_accessed() {
    let lt = LocationTable::new();
    let key = make_key("e1", 0);
    lt.register(key.clone(), LocationDescriptor {
        tier: Tier::Fjall,
        node_address: NodeAddress::localhost(),
        state: LocState::Clean,
        version: 1,
        encoding: Encoding::Float32,
        last_accessed: 0,
    });
    lt.touch(&key, 1710000000);
    let desc = lt.get(&key).unwrap();
    assert_eq!(desc.last_accessed, 1710000000);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p trondb-core -- location::tests`
Expected: FAIL — `last_accessed` field doesn't exist, `touch()` method doesn't exist

- [ ] **Step 3: Add `last_accessed` field and `touch()` method**

In `crates/trondb-core/src/location.rs`, modify `LocationDescriptor` (around line 84-91):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationDescriptor {
    pub tier: Tier,
    pub node_address: NodeAddress,
    pub state: LocState,
    pub version: u64,
    pub encoding: Encoding,
    #[serde(default)]
    pub last_accessed: u64,
}
```

Add `touch()` and `iter()` methods to `LocationTable` impl (after `update_tier`):

```rust
/// Update last_accessed timestamp for a representation key.
pub fn touch(&self, key: &ReprKey, timestamp: u64) {
    if let Some(mut desc) = self.descriptors.get_mut(key) {
        desc.last_accessed = timestamp;
    }
}

/// Iterate over all entries in the location table.
pub fn iter(&self) -> impl Iterator<Item = dashmap::mapref::multiple::RefMulti<'_, ReprKey, LocationDescriptor>> {
    self.descriptors.iter()
}
```

Update all existing `LocationDescriptor` constructions in the file's tests to include `last_accessed: 0`. Search for `LocationDescriptor {` in the test module and add the field.

Also update the `LocationDescriptor` constructions in `executor.rs` — every place that creates a `LocationDescriptor` needs `last_accessed: 0`. Search `executor.rs` for `LocationDescriptor {` and add the field to each occurrence. There are approximately 4 occurrences (in the INSERT handler around lines 340-375).

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-core`
Expected: all tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/location.rs crates/trondb-core/src/executor.rs
git commit -m "feat: add last_accessed timestamp to LocationDescriptor"
```

---

### Task 4: FjallStore tiered partition methods

**Files:**
- Modify: `crates/trondb-core/src/store.rs`

- [ ] **Step 1: Write tiered store tests**

Add to the `#[cfg(test)] mod tests` in `crates/trondb-core/src/store.rs` (or create one if none exists — check the file first). These tests require a tempdir for Fjall:

```rust
#[cfg(test)]
mod tier_tests {
    use super::*;
    use crate::location::Tier;
    use crate::types::LogicalId;

    fn temp_store() -> (FjallStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        (store, dir)
    }

    #[test]
    fn write_and_read_warm_tier() {
        let (store, _dir) = temp_store();
        let id = LogicalId::from_string("e1");
        let data = b"int8 quantised data";

        store.write_tiered("venues", &id, Tier::NVMe, data).unwrap();
        let result = store.read_tiered("venues", &id, Tier::NVMe).unwrap();
        assert_eq!(result.as_deref(), Some(data.as_slice()));
    }

    #[test]
    fn write_and_read_archive_tier() {
        let (store, _dir) = temp_store();
        let id = LogicalId::from_string("e1");
        let data = b"binary quantised data";

        store.write_tiered("venues", &id, Tier::Archive, data).unwrap();
        let result = store.read_tiered("venues", &id, Tier::Archive).unwrap();
        assert_eq!(result.as_deref(), Some(data.as_slice()));
    }

    #[test]
    fn read_nonexistent_returns_none() {
        let (store, _dir) = temp_store();
        let id = LogicalId::from_string("nope");
        let result = store.read_tiered("venues", &id, Tier::NVMe).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn delete_from_tier() {
        let (store, _dir) = temp_store();
        let id = LogicalId::from_string("e1");
        store.write_tiered("venues", &id, Tier::NVMe, b"data").unwrap();
        store.delete_from_tier("venues", &id, Tier::NVMe).unwrap();
        let result = store.read_tiered("venues", &id, Tier::NVMe).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn tier_entity_count() {
        let (store, _dir) = temp_store();
        let id1 = LogicalId::from_string("e1");
        let id2 = LogicalId::from_string("e2");
        store.write_tiered("venues", &id1, Tier::NVMe, b"data1").unwrap();
        store.write_tiered("venues", &id2, Tier::NVMe, b"data2").unwrap();
        assert_eq!(store.tier_entity_count("venues", Tier::NVMe).unwrap(), 2);
        assert_eq!(store.tier_entity_count("venues", Tier::Archive).unwrap(), 0);
    }

    #[test]
    fn tier_partition_names() {
        assert_eq!(FjallStore::tier_partition_name("venues", Tier::Fjall), "venues");
        assert_eq!(FjallStore::tier_partition_name("venues", Tier::NVMe), "warm.venues");
        assert_eq!(FjallStore::tier_partition_name("venues", Tier::Archive), "archive.venues");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p trondb-core -- tier_tests`
Expected: FAIL — methods don't exist

- [ ] **Step 3: Implement tiered store methods**

Add to `FjallStore` impl in `crates/trondb-core/src/store.rs`:

```rust
/// Get the Fjall partition name for a given collection and tier.
pub fn tier_partition_name(collection: &str, tier: Tier) -> String {
    match tier {
        Tier::Fjall | Tier::Ram => collection.to_string(),
        Tier::NVMe => format!("warm.{collection}"),
        Tier::Archive => format!("archive.{collection}"),
    }
}

/// Read entity data from a specific tier's partition.
pub fn read_tiered(
    &self,
    collection: &str,
    entity_id: &LogicalId,
    tier: Tier,
) -> Result<Option<Vec<u8>>, EngineError> {
    let part_name = Self::tier_partition_name(collection, tier);
    let partition = self
        .keyspace
        .open_partition(&part_name, Default::default())
        .map_err(|e| EngineError::Storage(e.to_string()))?;

    let key = format!("entity:{}", entity_id.as_str());
    match partition.get(key.as_bytes()) {
        Ok(Some(val)) => Ok(Some(val.to_vec())),
        Ok(None) => Ok(None),
        Err(e) => Err(EngineError::Storage(e.to_string())),
    }
}

/// Write entity data to a specific tier's partition.
pub fn write_tiered(
    &self,
    collection: &str,
    entity_id: &LogicalId,
    tier: Tier,
    data: &[u8],
) -> Result<(), EngineError> {
    let part_name = Self::tier_partition_name(collection, tier);
    let partition = self
        .keyspace
        .open_partition(&part_name, Default::default())
        .map_err(|e| EngineError::Storage(e.to_string()))?;

    let key = format!("entity:{}", entity_id.as_str());
    partition
        .insert(key.as_bytes(), data)
        .map_err(|e| EngineError::Storage(e.to_string()))?;

    self.keyspace
        .persist(fjall::PersistMode::SyncAll)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    Ok(())
}

/// Delete entity from a specific tier's partition.
pub fn delete_from_tier(
    &self,
    collection: &str,
    entity_id: &LogicalId,
    tier: Tier,
) -> Result<(), EngineError> {
    let part_name = Self::tier_partition_name(collection, tier);
    let partition = self
        .keyspace
        .open_partition(&part_name, Default::default())
        .map_err(|e| EngineError::Storage(e.to_string()))?;

    let key = format!("entity:{}", entity_id.as_str());
    partition
        .remove(key.as_bytes())
        .map_err(|e| EngineError::Storage(e.to_string()))?;

    self.keyspace
        .persist(fjall::PersistMode::SyncAll)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    Ok(())
}

/// Count entities in a specific tier's partition for a collection.
pub fn tier_entity_count(
    &self,
    collection: &str,
    tier: Tier,
) -> Result<usize, EngineError> {
    let part_name = Self::tier_partition_name(collection, tier);
    let partition = self
        .keyspace
        .open_partition(&part_name, Default::default())
        .map_err(|e| EngineError::Storage(e.to_string()))?;

    let prefix = b"entity:";
    let count = partition.prefix(prefix).count();
    Ok(count)
}
```

Add the import for `Tier` at the top of `store.rs`:
```rust
use crate::location::Tier;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-core -- tier_tests`
Expected: all 6 tests pass

- [ ] **Step 5: Run full workspace tests**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/src/store.rs
git commit -m "feat: add tiered partition methods to FjallStore"
```

---

## Chunk 2: Engine + TQL + WAL (Tasks 5-8)

### Task 5: Engine tier operation methods

**Files:**
- Modify: `crates/trondb-core/src/lib.rs`

- [ ] **Step 1: Add Engine methods for tier operations**

Add to the `Engine` impl block in `crates/trondb-core/src/lib.rs`, after the existing `wal_writer()` method:

```rust
/// Read entity data from a specific tier's partition.
pub fn read_tiered(
    &self,
    collection: &str,
    entity_id: &LogicalId,
    tier: Tier,
) -> Result<Option<Vec<u8>>, EngineError> {
    self.executor.store.read_tiered(collection, entity_id, tier)
}

/// Write entity data to a specific tier's partition.
pub fn write_tiered(
    &self,
    collection: &str,
    entity_id: &LogicalId,
    tier: Tier,
    data: &[u8],
) -> Result<(), EngineError> {
    self.executor.store.write_tiered(collection, entity_id, tier, data)
}

/// Delete entity from a specific tier's partition.
pub fn delete_from_tier(
    &self,
    collection: &str,
    entity_id: &LogicalId,
    tier: Tier,
) -> Result<(), EngineError> {
    self.executor.store.delete_from_tier(collection, entity_id, tier)
}

/// Count entities in a specific tier for a collection.
pub fn tier_entity_count(
    &self,
    collection: &str,
    tier: Tier,
) -> Result<usize, EngineError> {
    self.executor.store.tier_entity_count(collection, tier)
}

/// Remove an entity from the HNSW index (tombstone).
pub fn remove_from_hnsw(&self, collection: &str, entity_id: &LogicalId) {
    if let Some(idx) = self.executor.indexes.get(collection) {
        idx.remove(entity_id);
    }
}

/// Insert a vector into the HNSW index for a collection.
pub fn insert_into_hnsw(&self, collection: &str, entity_id: &LogicalId, vector: &[f32]) {
    if let Some(idx) = self.executor.indexes.get(collection) {
        idx.insert(entity_id, vector);
    }
}

```

Note: `Engine` already exposes `location()` → `&LocationTable` (via `self.executor.location()`). No need to add a duplicate.

Add the necessary imports at the top of `lib.rs`:
```rust
use crate::location::Tier;
```

Note: This requires `executor.store`, `executor.indexes`, and `executor.location` to be accessible from `Engine`. Check if `Executor`'s fields are `pub` or `pub(crate)`. If they are private, change `store`, `indexes`, and `location` to `pub(crate)` in `executor.rs` (around line 29-39).

- [ ] **Step 2: Run tests**

Run: `cargo test --workspace`
Expected: all tests pass (no new tests — this exposes existing functionality)

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-core/src/lib.rs crates/trondb-core/src/executor.rs
git commit -m "feat: expose Engine tier ops, HNSW remove/insert, location_table"
```

---

### Task 6: TQL tokens + AST + parser for DEMOTE/PROMOTE/EXPLAIN TIERS

**Files:**
- Modify: `crates/trondb-tql/src/token.rs`
- Modify: `crates/trondb-tql/src/ast.rs`
- Modify: `crates/trondb-tql/src/parser.rs`

- [ ] **Step 1: Add tokens**

In `crates/trondb-tql/src/token.rs`, add before the `// Identifiers` comment:

```rust
#[token("DEMOTE", priority = 10, ignore(ascii_case))]
Demote,

#[token("PROMOTE", priority = 10, ignore(ascii_case))]
Promote,

#[token("TIERS", priority = 10, ignore(ascii_case))]
Tiers,

#[token("WARM", priority = 10, ignore(ascii_case))]
Warm,

#[token("ARCHIVE", priority = 10, ignore(ascii_case))]
Archive,
```

- [ ] **Step 2: Add AST types**

In `crates/trondb-tql/src/ast.rs`, add the new statement types and enum:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum TierTarget {
    Warm,
    Archive,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DemoteStmt {
    pub entity_id: String,
    pub collection: String,
    pub target_tier: TierTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromoteStmt {
    pub entity_id: String,
    pub collection: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainTiersStmt {
    pub collection: String,
}
```

Add variants to the `Statement` enum:

```rust
Demote(DemoteStmt),
Promote(PromoteStmt),
ExplainTiers(ExplainTiersStmt),
```

- [ ] **Step 3: Write parser tests**

Add to parser tests in `crates/trondb-tql/src/parser.rs`:

```rust
#[test]
fn parse_demote_warm() {
    let stmt = parse("DEMOTE 'entity1' FROM venues TO WARM;").unwrap();
    match stmt {
        Statement::Demote(d) => {
            assert_eq!(d.entity_id, "entity1");
            assert_eq!(d.collection, "venues");
            assert_eq!(d.target_tier, TierTarget::Warm);
        }
        _ => panic!("expected Demote"),
    }
}

#[test]
fn parse_demote_archive() {
    let stmt = parse("DEMOTE 'entity1' FROM venues TO ARCHIVE;").unwrap();
    match stmt {
        Statement::Demote(d) => {
            assert_eq!(d.target_tier, TierTarget::Archive);
        }
        _ => panic!("expected Demote"),
    }
}

#[test]
fn parse_promote() {
    let stmt = parse("PROMOTE 'entity1' FROM venues;").unwrap();
    match stmt {
        Statement::Promote(p) => {
            assert_eq!(p.entity_id, "entity1");
            assert_eq!(p.collection, "venues");
        }
        _ => panic!("expected Promote"),
    }
}

#[test]
fn parse_explain_tiers() {
    let stmt = parse("EXPLAIN TIERS venues;").unwrap();
    match stmt {
        Statement::ExplainTiers(e) => {
            assert_eq!(e.collection, "venues");
        }
        _ => panic!("expected ExplainTiers"),
    }
}
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cargo test -p trondb-tql`
Expected: FAIL — parse functions not implemented

- [ ] **Step 5: Implement parser**

In `crates/trondb-tql/src/parser.rs`, add to the `parse_statement()` match (around the dispatch):

```rust
Token::Demote => self.parse_demote(),
Token::Promote => self.parse_promote(),
```

Modify the `Token::Explain` arm. Currently it parses `EXPLAIN` then recursively parses the inner statement. Change to check for `TIERS` first:

```rust
Token::Explain => {
    self.advance(); // consume EXPLAIN
    if self.peek() == Some(&Token::Tiers) {
        self.advance(); // consume TIERS
        let collection = self.expect_ident()?;
        self.expect(&Token::Semicolon)?;
        Ok(Statement::ExplainTiers(ExplainTiersStmt { collection }))
    } else {
        // Existing EXPLAIN logic — parse inner statement and wrap
        let inner = self.parse_statement()?;
        Ok(Statement::Explain(Box::new(inner)))
    }
}
```

Add the parsing methods:

```rust
fn parse_demote(&mut self) -> Result<Statement, ParseError> {
    self.advance(); // consume DEMOTE
    let entity_id = self.expect_string_lit()?;
    self.expect(&Token::From)?;
    let collection = self.expect_ident()?;
    self.expect(&Token::To)?;

    let target_tier = match self.peek() {
        Some(Token::Warm) => {
            self.advance();
            TierTarget::Warm
        }
        Some(Token::Archive) => {
            self.advance();
            TierTarget::Archive
        }
        _ => return Err(ParseError::InvalidSyntax("expected WARM or ARCHIVE".into())),
    };

    self.expect(&Token::Semicolon)?;
    Ok(Statement::Demote(DemoteStmt {
        entity_id,
        collection,
        target_tier,
    }))
}

fn parse_promote(&mut self) -> Result<Statement, ParseError> {
    self.advance(); // consume PROMOTE
    let entity_id = self.expect_string_lit()?;
    self.expect(&Token::From)?;
    let collection = self.expect_ident()?;
    self.expect(&Token::Semicolon)?;
    Ok(Statement::Promote(PromoteStmt {
        entity_id,
        collection,
    }))
}
```

Add the necessary imports at the top of the parser file:
```rust
use crate::ast::{DemoteStmt, PromoteStmt, ExplainTiersStmt, TierTarget};
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p trondb-tql`
Expected: all tests pass (existing + 4 new)

- [ ] **Step 7: Commit**

```bash
git add crates/trondb-tql/src/token.rs crates/trondb-tql/src/ast.rs crates/trondb-tql/src/parser.rs
git commit -m "feat(tql): add DEMOTE, PROMOTE, EXPLAIN TIERS syntax"
```

---

### Task 7: Planner + executor — Demote, Promote, ExplainTiers plans

**Files:**
- Modify: `crates/trondb-core/src/planner.rs`
- Modify: `crates/trondb-core/src/executor.rs`

- [ ] **Step 1: Add plan variants**

In `crates/trondb-core/src/planner.rs`, add new plan structs:

```rust
#[derive(Debug, Clone)]
pub struct DemotePlan {
    pub entity_id: String,
    pub collection: String,
    pub target_tier: trondb_tql::ast::TierTarget,
}

#[derive(Debug, Clone)]
pub struct PromotePlan {
    pub entity_id: String,
    pub collection: String,
}

#[derive(Debug, Clone)]
pub struct ExplainTiersPlan {
    pub collection: String,
}
```

Add to the `Plan` enum:

```rust
Demote(DemotePlan),
Promote(PromotePlan),
ExplainTiers(ExplainTiersPlan),
```

Add planner arms in the `plan()` function's match on `Statement`:

```rust
Statement::Demote(d) => Ok(Plan::Demote(DemotePlan {
    entity_id: d.entity_id.clone(),
    collection: d.collection.clone(),
    target_tier: d.target_tier.clone(),
})),
Statement::Promote(p) => Ok(Plan::Promote(PromotePlan {
    entity_id: p.entity_id.clone(),
    collection: p.collection.clone(),
})),
Statement::ExplainTiers(e) => Ok(Plan::ExplainTiers(ExplainTiersPlan {
    collection: e.collection.clone(),
})),
```

- [ ] **Step 2: Add executor handlers**

In `crates/trondb-core/src/executor.rs`, add match arms in `execute()` for the new plan variants. These are stub handlers — the actual tier migration logic lives in `trondb-routing` and will be wired in Task 11. The executor handles the `ExplainTiers` variant directly since it only reads data:

```rust
Plan::Demote(_) | Plan::Promote(_) => {
    // Handled by the routing layer (TierMigrator / SemanticRouter)
    let start = std::time::Instant::now();
    Ok(QueryResult {
        columns: vec!["status".to_string()],
        rows: vec![Row {
            values: HashMap::from([("status".into(), Value::String("OK".into()))]),
            score: None,
        }],
        stats: QueryStats {
            elapsed: start.elapsed(),
            entities_scanned: 0,
            mode: QueryMode::Deterministic,
            tier: "Routing".into(),
        },
    })
}
Plan::ExplainTiers(ref p) => {
    let start = std::time::Instant::now();
    let hot = self.store.tier_entity_count(&p.collection, Tier::Fjall).unwrap_or(0);
    let warm = self.store.tier_entity_count(&p.collection, Tier::NVMe).unwrap_or(0);
    let archive = self.store.tier_entity_count(&p.collection, Tier::Archive).unwrap_or(0);

    let mut rows = Vec::new();
    for (tier_name, count) in [("Hot", hot), ("Warm", warm), ("Archive", archive)] {
        rows.push(Row {
            values: HashMap::from([
                ("tier".into(), Value::String(tier_name.into())),
                ("entity_count".into(), Value::Int(count as i64)),
            ]),
            score: None,
        });
    }

    Ok(QueryResult {
        columns: vec!["tier".to_string(), "entity_count".to_string()],
        rows,
        stats: QueryStats {
            elapsed: start.elapsed(),
            entities_scanned: 0,
            mode: QueryMode::Deterministic,
            tier: "Fjall".into(),
        },
    })
}
```

Also add the EXPLAIN arms for the new plan variants (in the `Plan::Explain` match):

```rust
Plan::Demote(_) => {
    props.push(("operation", "Demote".into()));
    props.push(("tier", "Routing".into()));
}
Plan::Promote(_) => {
    props.push(("operation", "Promote".into()));
    props.push(("tier", "Routing".into()));
}
Plan::ExplainTiers(_) => {
    props.push(("operation", "ExplainTiers".into()));
}
```

Add the import for `Tier`:
```rust
use crate::location::Tier;  // if not already imported
```

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-core/src/planner.rs crates/trondb-core/src/executor.rs
git commit -m "feat(core): add Demote, Promote, ExplainTiers plan variants"
```

---

### Task 8: WAL TierMigration record type

**Files:**
- Modify: `crates/trondb-wal/src/record.rs`

- [ ] **Step 1: Add TierMigration record type**

In `crates/trondb-wal/src/record.rs`, add to the `RecordType` enum (after `AffinityGroupRemove = 0x62`):

```rust
TierMigration = 0x70,
```

- [ ] **Step 2: Run tests**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-wal/src/record.rs
git commit -m "feat(wal): add TierMigration record type (0x70)"
```

---

## Chunk 3: Routing Layer (Tasks 9-12)

### Task 9: TierConfig + HealthSignal extensions

**Files:**
- Modify: `crates/trondb-routing/src/config.rs`
- Modify: `crates/trondb-routing/src/health.rs`
- Modify: `crates/trondb-routing/src/node.rs`

- [ ] **Step 1: Add TierConfig**

In `crates/trondb-routing/src/config.rs`, add after the existing `ColocationConfig`:

```rust
#[derive(Debug, Clone)]
pub struct TierConfig {
    /// Max entities in hot tier per collection (default: 100_000)
    pub hot_capacity: usize,
    /// Max entities in warm tier per collection (default: 1_000_000)
    pub warm_capacity: usize,
    /// Seconds idle before hot → warm demotion (default: 86400 = 24h)
    pub demote_after_secs: u64,
    /// Seconds idle in warm before warm → archive (default: 604800 = 7d)
    pub archive_after_secs: u64,
    /// Auto-promote warm → hot on FETCH (default: true)
    pub promote_on_access: bool,
    /// Max entities to migrate per cycle (default: 100)
    pub migration_batch_size: usize,
    /// Migration cycle interval in ms (default: 60_000)
    pub migration_interval_ms: u64,
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            hot_capacity: 100_000,
            warm_capacity: 1_000_000,
            demote_after_secs: 86_400,
            archive_after_secs: 604_800,
            promote_on_access: true,
            migration_batch_size: 100,
            migration_interval_ms: 60_000,
        }
    }
}
```

- [ ] **Step 2: Add warm/archive counts to HealthSignal**

In `crates/trondb-routing/src/health.rs`, add two fields to `HealthSignal` (after `status`):

```rust
pub warm_entity_count: u64,
pub archive_entity_count: u64,
```

Update `HealthSignal` constructions in all test code and in `node.rs`. Search for `HealthSignal {` and add `warm_entity_count: 0, archive_entity_count: 0` to each occurrence.

In `crates/trondb-routing/src/node.rs`, update `LocalNode::health_snapshot()` to populate the new fields. Add after the existing `hot_entity_count` line:

```rust
warm_entity_count: self.engine.tier_entity_count("", trondb_core::location::Tier::NVMe).unwrap_or(0) as u64,
archive_entity_count: self.engine.tier_entity_count("", trondb_core::location::Tier::Archive).unwrap_or(0) as u64,
```

Note: This passes an empty collection name which won't return meaningful counts across all collections. For Phase 7a, this is acceptable — per-collection tier counts are available via `EXPLAIN TIERS`. A future phase can aggregate across collections.

Also update `SimulatedNode::health_snapshot()` in the same file to include the new fields with `0` defaults.

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-routing/src/config.rs crates/trondb-routing/src/health.rs crates/trondb-routing/src/node.rs
git commit -m "feat: add TierConfig, warm/archive counts in HealthSignal"
```

---

### Task 10: TierMigrator — demotion cycle

**Files:**
- Create: `crates/trondb-routing/src/migrator.rs`
- Modify: `crates/trondb-routing/src/lib.rs` (add `pub mod migrator;`)

- [ ] **Step 1: Write demotion tests**

In `crates/trondb-routing/src/migrator.rs`:

```rust
use std::sync::Arc;
use std::time::SystemTime;

use trondb_core::location::{Encoding, LocState, Tier};
use trondb_core::quantise::{quantise_binary, quantise_int8};
use trondb_core::types::LogicalId;
use trondb_core::Engine;
use trondb_wal::record::RecordType;

use crate::affinity::AffinityIndex;
use crate::config::{ColocationConfig, TierConfig};
use crate::error::RouterError;
use crate::eviction::{select_eviction_candidates, LruTracker};
use crate::node::EntityId;

/// Payload for TierMigration WAL records.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct TierMigrationPayload {
    pub entity_id: String,
    pub collection: String,
    pub from_tier: Tier,
    pub to_tier: Tier,
    pub encoding: Encoding,
}

pub struct TierMigrator {
    config: TierConfig,
    pub(crate) engine: Arc<Engine>,
    lru: Arc<std::sync::Mutex<LruTracker>>,
    affinity: Arc<AffinityIndex>,
}

impl TierMigrator {
    pub fn new(
        config: TierConfig,
        engine: Arc<Engine>,
        lru: Arc<std::sync::Mutex<LruTracker>>,
        affinity: Arc<AffinityIndex>,
    ) -> Self {
        Self { config, engine, lru, affinity }
    }

    /// Run one demotion cycle: hot → warm for idle entities.
    pub async fn demote_hot_to_warm(&self, collection: &str) -> Result<usize, RouterError> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let threshold = now.saturating_sub(self.config.demote_after_secs);

        // Find hot-tier entities with last_accessed < threshold
        let location = self.engine.location();
        let mut candidates: Vec<EntityId> = Vec::new();

        // Iterate location table for hot-tier entities
        for entry in location.iter() {
            let (key, desc) = entry.pair();
            if desc.tier == Tier::Fjall
                && desc.state == LocState::Clean
                && desc.last_accessed < threshold
            {
                candidates.push(LogicalId::from_string(key.entity_id.as_str()));
            }
        }

        if candidates.is_empty() {
            return Ok(0);
        }

        // Sort by eviction priority (ungrouped LRU first)
        let lru_guard = self.lru.lock().unwrap();
        let sorted = select_eviction_candidates(
            &candidates,
            candidates.len().min(self.config.migration_batch_size),
            &self.affinity,
            &lru_guard,
        );
        drop(lru_guard);

        let mut migrated = 0;
        for entity_id in sorted.iter().take(self.config.migration_batch_size) {
            match self.demote_single(collection, entity_id, Tier::Fjall, Tier::NVMe).await {
                Ok(()) => migrated += 1,
                Err(e) => {
                    tracing::warn!("demotion failed for {}: {e}", entity_id.as_str());
                }
            }
        }
        Ok(migrated)
    }

    /// Run one demotion cycle: warm → archive for idle entities.
    pub async fn demote_warm_to_archive(&self, collection: &str) -> Result<usize, RouterError> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let threshold = now.saturating_sub(self.config.archive_after_secs);

        let location = self.engine.location();
        let mut candidates: Vec<EntityId> = Vec::new();

        for entry in location.iter() {
            let (key, desc) = entry.pair();
            if desc.tier == Tier::NVMe
                && desc.state == LocState::Clean
                && desc.last_accessed < threshold
            {
                candidates.push(LogicalId::from_string(key.entity_id.as_str()));
            }
        }

        let mut migrated = 0;
        for entity_id in candidates.iter().take(self.config.migration_batch_size) {
            match self.demote_single(collection, entity_id, Tier::NVMe, Tier::Archive).await {
                Ok(()) => migrated += 1,
                Err(e) => {
                    tracing::warn!("warm→archive demotion failed for {}: {e}", entity_id.as_str());
                }
            }
        }
        Ok(migrated)
    }

    pub(crate) async fn demote_single(
        &self,
        collection: &str,
        entity_id: &EntityId,
        from_tier: Tier,
        to_tier: Tier,
    ) -> Result<(), RouterError> {
        let location = self.engine.location();
        let repr_key = trondb_core::location::ReprKey {
            entity_id: entity_id.clone(),
            repr_index: 0,
        };

        // Step a: Mark as migrating
        location
            .transition(&repr_key, LocState::Migrating)
            .map_err(RouterError::Engine)?;

        // Step b: WAL log intent
        let wal = self.engine.wal_writer();
        let target_encoding = match to_tier {
            Tier::NVMe => Encoding::Int8,
            Tier::Archive => Encoding::Binary,
            _ => Encoding::Float32,
        };
        let payload = rmp_serde::to_vec_named(&TierMigrationPayload {
            entity_id: entity_id.as_str().to_owned(),
            collection: collection.to_owned(),
            from_tier,
            to_tier,
            encoding: target_encoding,
        })
        .map_err(|e| RouterError::Engine(trondb_core::error::EngineError::Storage(e.to_string())))?;

        let tx_id = wal.next_tx_id();
        wal.append(RecordType::TierMigration, collection, tx_id, 0, payload);
        wal.commit(tx_id)
            .await
            .map_err(|e| RouterError::Engine(trondb_core::error::EngineError::Storage(e.to_string())))?;

        // Step c: Read from source tier
        let data = self
            .engine
            .read_tiered(collection, entity_id, from_tier)
            .map_err(RouterError::Engine)?
            .ok_or_else(|| {
                RouterError::Engine(trondb_core::error::EngineError::EntityNotFound(
                    entity_id.as_str().to_owned(),
                ))
            })?;

        // Step d: Transcode and write to target tier
        match to_tier {
            Tier::NVMe => {
                // Deserialise Float32 vector from source data
                let vector: Vec<f32> = rmp_serde::from_slice(&data)
                    .map_err(|e| RouterError::Engine(trondb_core::error::EngineError::Storage(e.to_string())))?;
                let quantised = quantise_int8(&vector);
                self.engine
                    .write_tiered(collection, entity_id, Tier::NVMe, &quantised.to_bytes())
                    .map_err(RouterError::Engine)?;
            }
            Tier::Archive => {
                // Source is Int8 — read and convert to binary
                let int8 = trondb_core::quantise::QuantisedInt8::from_bytes(&data)
                    .map_err(RouterError::Engine)?;
                let float_vec = trondb_core::quantise::dequantise_int8(&int8);
                let binary = quantise_binary(&float_vec);
                self.engine
                    .write_tiered(collection, entity_id, Tier::Archive, &binary.to_bytes())
                    .map_err(RouterError::Engine)?;
            }
            _ => {}
        }

        // Step e: Remove from HNSW (only hot → warm)
        if from_tier == Tier::Fjall {
            self.engine.remove_from_hnsw(collection, entity_id);
        }

        // Step f: Update location table
        let (target_encoding, target_tier) = match to_tier {
            Tier::NVMe => (Encoding::Int8, Tier::NVMe),
            Tier::Archive => (Encoding::Binary, Tier::Archive),
            _ => (Encoding::Float32, to_tier),
        };
        location
            .update_tier(&repr_key, target_tier, target_encoding)
            .map_err(RouterError::Engine)?;

        // Step g: Complete migration
        location
            .transition(&repr_key, LocState::Clean)
            .map_err(RouterError::Engine)?;

        // Step h: Delete from source
        self.engine
            .delete_from_tier(collection, entity_id, from_tier)
            .map_err(RouterError::Engine)?;

        Ok(())
    }

    /// Promote a warm-tier entity back to hot.
    pub async fn promote_to_hot(
        &self,
        collection: &str,
        entity_id: &EntityId,
        vector: Vec<f32>,
    ) -> Result<(), RouterError> {
        let location = self.engine.location();
        let repr_key = trondb_core::location::ReprKey {
            entity_id: entity_id.clone(),
            repr_index: 0,
        };

        // Write Float32 to hot partition
        let data = rmp_serde::to_vec_named(&vector)
            .map_err(|e| RouterError::Engine(trondb_core::error::EngineError::Storage(e.to_string())))?;
        self.engine
            .write_tiered(collection, entity_id, Tier::Fjall, &data)
            .map_err(RouterError::Engine)?;

        // Insert into HNSW
        self.engine.insert_into_hnsw(collection, entity_id, &vector);

        // WAL log
        let wal = self.engine.wal_writer();
        let payload = rmp_serde::to_vec_named(&TierMigrationPayload {
            entity_id: entity_id.as_str().to_owned(),
            collection: collection.to_owned(),
            from_tier: Tier::NVMe,
            to_tier: Tier::Fjall,
            encoding: Encoding::Float32,
        })
        .map_err(|e| RouterError::Engine(trondb_core::error::EngineError::Storage(e.to_string())))?;
        let tx_id = wal.next_tx_id();
        wal.append(RecordType::TierMigration, collection, tx_id, 0, payload);
        wal.commit(tx_id)
            .await
            .map_err(|e| RouterError::Engine(trondb_core::error::EngineError::Storage(e.to_string())))?;

        // Update location table
        location
            .update_tier(&repr_key, Tier::Fjall, Encoding::Float32)
            .map_err(RouterError::Engine)?;

        // Delete from warm
        self.engine
            .delete_from_tier(collection, entity_id, Tier::NVMe)
            .map_err(RouterError::Engine)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_migration_payload_serialisation() {
        let payload = TierMigrationPayload {
            entity_id: "e1".to_owned(),
            collection: "venues".to_owned(),
            from_tier: Tier::Fjall,
            to_tier: Tier::NVMe,
            encoding: Encoding::Int8,
        };
        let bytes = rmp_serde::to_vec_named(&payload).unwrap();
        let restored: TierMigrationPayload = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(restored.entity_id, "e1");
        assert_eq!(restored.to_tier, Tier::NVMe);
    }
}
```

Add to `crates/trondb-routing/src/lib.rs`:
```rust
pub mod migrator;
```

- [ ] **Step 2: Run tests**

Run: `cargo test --workspace`
Expected: all tests pass (the unit test for payload serialisation passes; integration tests for actual demotion/promotion come in Task 13)

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-routing/src/migrator.rs crates/trondb-routing/src/lib.rs
git commit -m "feat: add TierMigrator — demotion cycle, promotion, WAL logging"
```

---

### Task 11: Wire Demote/Promote through SemanticRouter

**Files:**
- Modify: `crates/trondb-routing/src/router.rs`

- [ ] **Step 1: Handle Demote/Promote in route_and_execute**

In `crates/trondb-routing/src/router.rs`, in the `route_and_execute` method, add match arms before the general dispatch (alongside the existing `Plan::CreateAffinityGroup` and `Plan::AlterEntityDropAffinity` arms):

```rust
Plan::Demote(ref d) => {
    let entity_id = EntityId::from_string(&d.entity_id);
    let target_tier = match d.target_tier {
        trondb_tql::ast::TierTarget::Warm => trondb_core::location::Tier::NVMe,
        trondb_tql::ast::TierTarget::Archive => trondb_core::location::Tier::Archive,
    };
    let from_tier = match target_tier {
        trondb_core::location::Tier::NVMe => trondb_core::location::Tier::Fjall,
        trondb_core::location::Tier::Archive => trondb_core::location::Tier::NVMe,
        _ => trondb_core::location::Tier::Fjall,
    };

    if let Some(migrator) = &self.migrator {
        migrator
            .demote_single(&d.collection, &entity_id, from_tier, target_tier)
            .await?;
    }

    let start = std::time::Instant::now();
    return Ok(trondb_core::result::QueryResult {
        columns: vec!["status".to_string()],
        rows: vec![trondb_core::result::Row {
            values: std::collections::HashMap::from([
                ("status".into(), trondb_core::types::Value::String("OK".into())),
            ]),
            score: None,
        }],
        stats: trondb_core::result::QueryStats {
            elapsed: start.elapsed(),
            entities_scanned: 0,
            mode: trondb_core::result::QueryMode::Deterministic,
            tier: "Routing".into(),
        },
    });
}
Plan::Promote(ref p) => {
    let entity_id = EntityId::from_string(&p.entity_id);

    if let Some(migrator) = &self.migrator {
        // Read warm data, dequantise, promote
        let warm_data = migrator.engine.read_tiered(
            &p.collection,
            &entity_id,
            trondb_core::location::Tier::NVMe,
        ).map_err(RouterError::Engine)?;

        if let Some(data) = warm_data {
            let int8 = trondb_core::quantise::QuantisedInt8::from_bytes(&data)
                .map_err(RouterError::Engine)?;
            let vector = trondb_core::quantise::dequantise_int8(&int8);
            migrator.promote_to_hot(&p.collection, &entity_id, vector).await?;
        } else {
            return Err(RouterError::Engine(trondb_core::error::EngineError::EntityNotFound(
                p.entity_id.clone(),
            )));
        }
    }

    let start = std::time::Instant::now();
    return Ok(trondb_core::result::QueryResult {
        columns: vec!["status".to_string()],
        rows: vec![trondb_core::result::Row {
            values: std::collections::HashMap::from([
                ("status".into(), trondb_core::types::Value::String("OK".into())),
            ]),
            score: None,
        }],
        stats: trondb_core::result::QueryStats {
            elapsed: start.elapsed(),
            entities_scanned: 0,
            mode: trondb_core::result::QueryMode::Deterministic,
            tier: "Routing".into(),
        },
    });
}
```

The `SemanticRouter` struct needs a new field:
```rust
migrator: Option<Arc<TierMigrator>>,
```

Update `SemanticRouter::new()` and `SemanticRouter::with_wal()` to accept and store the migrator (or pass `None` initially and add a setter method `set_migrator()`).

Add the import:
```rust
use crate::migrator::TierMigrator;
```

Note: `demote_single` needs to be made `pub` in `migrator.rs` (it was `async fn` in Task 10).

- [ ] **Step 2: Run tests**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-routing/src/router.rs crates/trondb-routing/src/migrator.rs
git commit -m "feat: wire Demote/Promote plans through SemanticRouter"
```

---

### Task 12: Wire CLI + TierMigrator construction

**Files:**
- Modify: `crates/trondb-cli/src/main.rs`
- Modify: `crates/trondb-routing/src/router.rs` (if needed for TierMigrator background loop)

- [ ] **Step 1: Create TierMigrator in CLI and pass to router**

In `crates/trondb-cli/src/main.rs`, after the `SemanticRouter` construction, create the `TierMigrator` and wire it:

```rust
use trondb_routing::config::TierConfig;
use trondb_routing::migrator::TierMigrator;

// After router creation:
let lru = Arc::new(std::sync::Mutex::new(trondb_routing::eviction::LruTracker::new()));
let tier_config = TierConfig::default();
let migrator = Arc::new(TierMigrator::new(
    tier_config,
    engine.clone(),
    lru,
    router.affinity_index().clone(),
));
router.set_migrator(migrator);
```

This requires `SemanticRouter` to expose:
- `affinity() -> &Arc<AffinityIndex>` (may already exist or need adding)
- `set_migrator(migrator: Arc<TierMigrator>)` (add to router.rs)

- [ ] **Step 2: Run tests**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 3: Verify CLI builds**

Run: `cargo build -p trondb-cli`
Expected: compiles without errors

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-cli/src/main.rs crates/trondb-routing/src/router.rs
git commit -m "feat(cli): wire TierMigrator into REPL"
```

---

## Chunk 4: Integration + Polish (Tasks 13-14)

### Task 13: Integration tests — demotion, promotion, explain tiers

**Files:**
- Modify: `crates/trondb-routing/src/lib.rs` (integration tests)

- [ ] **Step 1: EXPLAIN TIERS test**

Add integration test in `crates/trondb-routing/src/lib.rs`:

```rust
#[tokio::test]
async fn explain_tiers_shows_distribution() {
    // Setup engine + router + create collection + insert entities
    // Run EXPLAIN TIERS collection;
    // Assert result has 3 rows (Hot, Warm, Archive) with entity counts
    // Initially all should be in Hot
}
```

- [ ] **Step 2: Manual DEMOTE test**

```rust
#[tokio::test]
async fn demote_moves_entity_to_warm() {
    // Setup engine + router + migrator
    // INSERT entity
    // DEMOTE entity FROM collection TO WARM;
    // EXPLAIN TIERS → hot count should decrease by 1, warm increase by 1
    // FETCH entity → should still return data (dequantised)
}
```

- [ ] **Step 3: Manual PROMOTE test**

```rust
#[tokio::test]
async fn promote_moves_entity_from_warm_to_hot() {
    // Setup engine + router + migrator
    // INSERT entity, DEMOTE to warm
    // PROMOTE entity FROM collection;
    // EXPLAIN TIERS → back to hot
    // SEARCH should find the entity again
}
```

- [ ] **Step 4: Quantisation roundtrip test**

```rust
#[tokio::test]
async fn demote_promote_preserves_vector_approximately() {
    // INSERT with known vector [1.0, 0.5, -0.5, -1.0]
    // DEMOTE to warm (Float32 → Int8)
    // PROMOTE back to hot (Int8 → Float32)
    // FETCH and compare — should be close to original (within Int8 precision)
}
```

- [ ] **Step 5: Run all tests**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-routing/src/lib.rs
git commit -m "test: add integration tests — tier demotion, promotion, explain tiers"
```

---

### Task 14: Update CLAUDE.md + clippy pass

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update CLAUDE.md**

Change the header from "Phase 6: Routing Intelligence" to "Phase 7a: Tiered Storage + Vector Quantisation".

Add tiered storage section after the routing intelligence bullets:

```markdown
- Tiered Storage: warm (Int8) and archive (Binary) tiers with automatic migration
  - Vector quantisation: Int8 scalar quantisation (~75% size reduction), Binary (~97%)
  - Tier partitions: {collection} (hot), warm.{collection}, archive.{collection}
  - TierMigrator: background demotion cycle (hot→warm→archive) based on access patterns
  - Promotion on access: warm→hot auto-promotion on FETCH (dequantise + HNSW re-insert)
  - HNSW tombstone removal + periodic rebuild (10% threshold)
  - TQL: DEMOTE/PROMOTE entities, EXPLAIN TIERS for distribution
  - WAL: TierMigration record (0x70) for crash recovery
  - LocationDescriptor.last_accessed for durable access tracking
```

Remove "All vectors are Float32 (gated until Phase 7)" since Phase 7a implements this.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: no warnings

- [ ] **Step 3: Run full test suite**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: update CLAUDE.md for Phase 7a — tiered storage + vector quantisation"
```

---

## Summary

| Task | Description | Files | Complexity |
|------|-------------|-------|------------|
| 1 | Vector quantisation module | 1 new, 1 modified | Simple |
| 2 | HNSW tombstone removal + rebuild | 1 modified | Medium |
| 3 | LocationDescriptor last_accessed | 2 modified | Simple |
| 4 | FjallStore tiered partition methods | 1 modified | Medium |
| 5 | Engine tier operation methods | 2 modified | Simple |
| 6 | TQL tokens + AST + parser | 3 modified | Medium |
| 7 | Planner + executor plan variants | 2 modified | Medium |
| 8 | WAL TierMigration record type | 1 modified | Simple |
| 9 | TierConfig + HealthSignal extensions | 3 modified | Simple |
| 10 | TierMigrator — demotion + promotion | 1 new, 1 modified | Complex |
| 11 | Wire Demote/Promote through router | 2 modified | Medium |
| 12 | Wire CLI + TierMigrator construction | 2 modified | Medium |
| 13 | Integration tests | 1 modified | Medium |
| 14 | CLAUDE.md + clippy | 1 modified | Simple |
