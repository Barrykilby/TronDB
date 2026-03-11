# TronDB Phase 5a — Indexes Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add field indexes, sparse vector indexes, hybrid SEARCH with RRF merge, and ScalarPreFilter to TronDB, replacing the simple CREATE COLLECTION syntax with full schema declarations.

**Architecture:** New RAM-resident SparseIndex (DashMap inverted index) alongside existing HNSW for dense vectors. Fjall-backed FieldIndex with sortable byte encoding for deterministic lookups. Planner gains schema awareness for strategy selection. RRF merges dense+sparse results for hybrid SEARCH.

**Tech Stack:** Rust 2021, Fjall (LSM), DashMap, hnsw_rs, logos lexer, Tokio async runtime. No new external dependencies.

**Spec:** `docs/superpowers/specs/2026-03-11-trondb-phase5a-design.md`

---

## File Structure

**New files:**
- `crates/trondb-core/src/field_index.rs` — Sortable byte encoding + FieldIndex (Fjall-backed per-index partition)
- `crates/trondb-core/src/sparse_index.rs` — SparseIndex (RAM inverted index for SPLADE vectors)
- `crates/trondb-core/src/hybrid.rs` — RRF merge function

**Modified files (trondb-tql crate):**
- `crates/trondb-tql/src/token.rs` — 14 new keyword tokens + Colon punctuation
- `crates/trondb-tql/src/ast.rs` — Expanded CreateCollectionStmt, SearchStmt, InsertStmt; new RepresentationDecl, FieldDecl, IndexDecl, VectorLiteral, Metric
- `crates/trondb-tql/src/parser.rs` — New parse methods for block CREATE COLLECTION, SEARCH with sparse/hybrid/WHERE, INSERT with named representations

**Modified files (trondb-core crate):**
- `crates/trondb-core/src/types.rs` — VectorData enum, FieldType expansion (Text/DateTime/EntityRef), Representation gains name+VectorData, CollectionSchema/StoredRepresentation/StoredField/StoredIndex/Metric types, remove old Collection/FieldDef
- `crates/trondb-core/src/location.rs` — Add Encoding::Sparse variant
- `crates/trondb-core/src/error.rs` — 6 new error variants
- `crates/trondb-core/src/store.rs` — CollectionSchema storage/retrieval, field index partition creation
- `crates/trondb-core/src/planner.rs` — SearchStrategy/FetchStrategy/PreFilter enums, expanded plan types, plan() takes schemas param
- `crates/trondb-core/src/executor.rs` — New fields (sparse_indexes, field_indexes, schemas), expanded CREATE COLLECTION/INSERT/FETCH/SEARCH/EXPLAIN paths
- `crates/trondb-core/src/lib.rs` — Module declarations, startup rebuild for SparseIndex/FieldIndex/schemas, updated integration tests
- `CLAUDE.md` — Updated for Phase 5a

---

## Chunk 1: Foundation

### Task 1: Core Foundation Types

**Files:**
- Modify: `crates/trondb-core/src/types.rs`
- Modify: `crates/trondb-core/src/location.rs`

These are pure data types added alongside existing types. No existing code changes, no breakage.

- [ ] **Step 1: Write tests for new types**

Add to `crates/trondb-core/src/types.rs` tests module:

```rust
#[test]
fn vector_data_dense() {
    let v = VectorData::Dense(vec![1.0, 2.0, 3.0]);
    match &v {
        VectorData::Dense(d) => assert_eq!(d.len(), 3),
        _ => panic!("expected Dense"),
    }
}

#[test]
fn vector_data_sparse() {
    let v = VectorData::Sparse(vec![(1, 0.8), (42, 0.5)]);
    match &v {
        VectorData::Sparse(s) => {
            assert_eq!(s.len(), 2);
            assert_eq!(s[0].0, 1);
        }
        _ => panic!("expected Sparse"),
    }
}

#[test]
fn metric_default_is_cosine() {
    let m = Metric::default();
    assert_eq!(m, Metric::Cosine);
}

#[test]
fn collection_schema_round_trip() {
    let schema = CollectionSchema {
        name: "test".into(),
        representations: vec![StoredRepresentation {
            name: "identity".into(),
            model: Some("jina-v4".into()),
            dimensions: Some(1024),
            metric: Metric::Cosine,
            sparse: false,
        }],
        fields: vec![StoredField {
            name: "status".into(),
            field_type: FieldType::Text,
        }],
        indexes: vec![StoredIndex {
            name: "idx_status".into(),
            fields: vec!["status".into()],
            partial_condition: None,
        }],
    };
    let bytes = rmp_serde::to_vec_named(&schema).unwrap();
    let restored: CollectionSchema = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(restored.name, "test");
    assert_eq!(restored.representations.len(), 1);
    assert_eq!(restored.fields.len(), 1);
    assert_eq!(restored.indexes.len(), 1);
}

#[test]
fn schema_field_type_variants() {
    assert_ne!(FieldType::Text, FieldType::Int);
    assert_ne!(FieldType::DateTime, FieldType::Bool);
    assert_eq!(
        FieldType::EntityRef("venues".into()),
        FieldType::EntityRef("venues".into())
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p trondb-core -- types::tests::vector_data 2>&1 | head -5`
Expected: compilation error (types don't exist yet)

- [ ] **Step 3: Implement new types**

Add to `crates/trondb-core/src/types.rs` before the existing `FieldType` section:

```rust
// ---------------------------------------------------------------------------
// VectorData — dense or sparse vector payload
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VectorData {
    Dense(Vec<f32>),
    Sparse(Vec<(u32, f32)>),
}

// ---------------------------------------------------------------------------
// Schema types — Phase 5a collection schema
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Metric {
    Cosine,
    InnerProduct,
}

impl Default for Metric {
    fn default() -> Self {
        Metric::Cosine
    }
}

/// Field type for schema declarations. Uses the canonical name `FieldType`
/// from the spec. The old `FieldType` in types.rs is renamed to `LegacyFieldType`
/// in this same task to avoid collision. `LegacyFieldType` is removed in Task 8.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FieldType {
    Text,
    DateTime,
    Bool,
    Int,
    Float,
    EntityRef(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionSchema {
    pub name: String,
    pub representations: Vec<StoredRepresentation>,
    pub fields: Vec<StoredField>,
    pub indexes: Vec<StoredIndex>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredRepresentation {
    pub name: String,
    pub model: Option<String>,
    pub dimensions: Option<usize>,
    pub metric: Metric,
    pub sparse: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredField {
    pub name: String,
    pub field_type: FieldType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredIndex {
    pub name: String,
    pub fields: Vec<String>,
    pub partial_condition: Option<String>,
}
```

Rename the existing `FieldType` enum in `crates/trondb-core/src/types.rs` to `LegacyFieldType` to avoid collision:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LegacyFieldType {
    String,
    Int,
    Float,
    Bool,
}
```

Also rename `FieldDef.field_type: FieldType` to `FieldDef.field_type: LegacyFieldType`. These legacy types are removed in Task 8.

Add `Encoding::Sparse` to `crates/trondb-core/src/location.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Encoding {
    Float32,
    Int8,
    Binary,
    Sparse,
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --workspace`
Expected: all tests pass (new types are additive, no breakage)

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/types.rs crates/trondb-core/src/location.rs
git commit -m "feat: add VectorData, Metric, CollectionSchema, FieldType, Encoding::Sparse"
```

---

### Task 2: Error Variants

**Files:**
- Modify: `crates/trondb-core/src/error.rs`

- [ ] **Step 1: Add new error variants**

Add to `EngineError` enum in `crates/trondb-core/src/error.rs`:

```rust
#[error("duplicate index: {0}")]
DuplicateIndex(String),

#[error("duplicate representation: {0}")]
DuplicateRepresentation(String),

#[error("duplicate field: {0}")]
DuplicateField(String),

#[error("field not indexed (ScalarPreFilter requires index): {0}")]
FieldNotIndexed(String),

#[error("sparse vector required but collection has no sparse representation: {0}")]
SparseVectorRequired(String),

#[error("invalid field type for {field}: expected {expected}, got {got}")]
InvalidFieldType {
    field: String,
    expected: String,
    got: String,
},
```

- [ ] **Step 2: Add error display tests**

Add a test module to `crates/trondb-core/src/error.rs` (or add tests in an existing test that imports EngineError):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_messages() {
        assert!(EngineError::DuplicateIndex("idx1".into()).to_string().contains("idx1"));
        assert!(EngineError::DuplicateRepresentation("identity".into()).to_string().contains("identity"));
        assert!(EngineError::DuplicateField("status".into()).to_string().contains("status"));
        assert!(EngineError::FieldNotIndexed("city".into()).to_string().contains("city"));
        assert!(EngineError::SparseVectorRequired("venues".into()).to_string().contains("venues"));
        assert!(EngineError::InvalidFieldType {
            field: "age".into(),
            expected: "Int".into(),
            got: "String".into(),
        }.to_string().contains("age"));
    }
}
```

- [ ] **Step 3: Verify tests pass**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-core/src/error.rs
git commit -m "feat: add Phase 5a error variants — DuplicateIndex, SparseVectorRequired, etc."
```

---

### Task 3: FieldIndex + Sortable Byte Encoding

**Files:**
- Create: `crates/trondb-core/src/field_index.rs`
- Modify: `crates/trondb-core/src/lib.rs` (add `pub mod field_index;`)

This is a standalone module with unit tests. It uses Fjall partition handles for storage but can be tested with real Fjall instances.

- [ ] **Step 1: Add module declaration**

Add `pub mod field_index;` to `crates/trondb-core/src/lib.rs` (after `pub mod edge;`).

- [ ] **Step 2: Write sortable encoding tests**

Create `crates/trondb-core/src/field_index.rs` with test module:

```rust
use crate::error::EngineError;
use crate::types::{LogicalId, FieldType, StoredIndex, Value};

// ---------------------------------------------------------------------------
// Sortable byte encoding
// ---------------------------------------------------------------------------

/// Encode a Value as sortable bytes for field index keys.
pub fn encode_sortable(value: &Value, field_type: &FieldType) -> Result<Vec<u8>, EngineError> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_text_sorts_lexicographically() {
        let a = encode_sortable(&Value::String("alpha".into()), &FieldType::Text).unwrap();
        let b = encode_sortable(&Value::String("beta".into()), &FieldType::Text).unwrap();
        assert!(a < b);
    }

    #[test]
    fn encode_int_negative_before_positive() {
        let neg = encode_sortable(&Value::Int(-10), &FieldType::Int).unwrap();
        let zero = encode_sortable(&Value::Int(0), &FieldType::Int).unwrap();
        let pos = encode_sortable(&Value::Int(10), &FieldType::Int).unwrap();
        assert!(neg < zero);
        assert!(zero < pos);
    }

    #[test]
    fn encode_int_preserves_order() {
        let a = encode_sortable(&Value::Int(100), &FieldType::Int).unwrap();
        let b = encode_sortable(&Value::Int(200), &FieldType::Int).unwrap();
        assert!(a < b);
    }

    #[test]
    fn encode_bool_false_before_true() {
        let f = encode_sortable(&Value::Bool(false), &FieldType::Bool).unwrap();
        let t = encode_sortable(&Value::Bool(true), &FieldType::Bool).unwrap();
        assert!(f < t);
    }

    #[test]
    fn encode_float_preserves_order() {
        let a = encode_sortable(&Value::Float(-1.5), &FieldType::Float).unwrap();
        let b = encode_sortable(&Value::Float(0.0), &FieldType::Float).unwrap();
        let c = encode_sortable(&Value::Float(1.5), &FieldType::Float).unwrap();
        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn encode_datetime_sorts_chronologically() {
        let a = encode_sortable(
            &Value::String("2024-01-01T00:00:00Z".into()),
            &FieldType::DateTime,
        ).unwrap();
        let b = encode_sortable(
            &Value::String("2024-06-15T12:00:00Z".into()),
            &FieldType::DateTime,
        ).unwrap();
        assert!(a < b);
    }

    #[test]
    fn encode_compound_key() {
        let key = encode_compound_key(
            &[Value::String("venue1".into()), Value::String("2024-01-01T00:00:00Z".into())],
            &[FieldType::Text, FieldType::DateTime],
        ).unwrap();
        assert!(key.contains(&0x00)); // null separator
    }

    #[test]
    fn compound_key_prefix_ordering() {
        let key_a = encode_compound_key(
            &[Value::String("a".into()), Value::String("2024-01-01".into())],
            &[FieldType::Text, FieldType::DateTime],
        ).unwrap();
        let key_b = encode_compound_key(
            &[Value::String("b".into()), Value::String("2024-01-01".into())],
            &[FieldType::Text, FieldType::DateTime],
        ).unwrap();
        assert!(key_a < key_b);
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p trondb-core -- field_index::tests 2>&1 | head -5`
Expected: FAIL (todo!() panics)

- [ ] **Step 4: Implement sortable encoding**

Replace `todo!()` in `encode_sortable`:

```rust
pub fn encode_sortable(value: &Value, field_type: &FieldType) -> Result<Vec<u8>, EngineError> {
    match (value, field_type) {
        (Value::String(s), FieldType::Text | FieldType::EntityRef(_) | FieldType::DateTime) => {
            let mut bytes = s.as_bytes().to_vec();
            bytes.push(0x00); // null terminator for compound key separation
            Ok(bytes)
        }
        (Value::Bool(b), FieldType::Bool) => {
            Ok(vec![if *b { 0x01 } else { 0x00 }])
        }
        (Value::Int(n), FieldType::Int) => {
            // Big-endian i64 with sign bit flipped so negative sorts before positive
            let flipped = (*n as u64) ^ (1u64 << 63);
            Ok(flipped.to_be_bytes().to_vec())
        }
        (Value::Float(f), FieldType::Float) => {
            // IEEE 754 manipulation for sort order
            let bits = f.to_bits();
            let sortable = if *f >= 0.0 {
                bits ^ (1u64 << 63) // flip sign bit for positive
            } else {
                !bits // flip all bits for negative
            };
            Ok(sortable.to_be_bytes().to_vec())
        }
        _ => Err(EngineError::InvalidFieldType {
            field: format!("{field_type:?}"),
            expected: format!("{field_type:?}"),
            got: format!("{value:?}"),
        }),
    }
}

/// Encode multiple values as a compound key with null-byte separators.
pub fn encode_compound_key(
    values: &[Value],
    field_types: &[FieldType],
) -> Result<Vec<u8>, EngineError> {
    let mut key = Vec::new();
    for (value, ft) in values.iter().zip(field_types.iter()) {
        let encoded = encode_sortable(value, ft)?;
        key.extend_from_slice(&encoded);
    }
    Ok(key)
}
```

- [ ] **Step 5: Run encoding tests**

Run: `cargo test -p trondb-core -- field_index::tests`
Expected: all encoding tests pass

- [ ] **Step 6: Write FieldIndex tests**

Add to the test module:

```rust
use tempfile::TempDir;
use fjall::{Config, PartitionCreateOptions};

fn open_test_partition() -> (fjall::PartitionHandle, TempDir) {
    let dir = TempDir::new().unwrap();
    let ks = Config::new(dir.path()).open().unwrap();
    let part = ks.open_partition("test_idx", PartitionCreateOptions::default()).unwrap();
    (part, dir)
}

#[test]
fn field_index_insert_and_lookup_eq() {
    let (partition, _dir) = open_test_partition();
    let idx = FieldIndex::new(
        partition,
        vec![("status".into(), FieldType::Text)],
    );
    let id1 = LogicalId::from_string("e1");
    let id2 = LogicalId::from_string("e2");

    idx.insert(&id1, &[Value::String("active".into())]).unwrap();
    idx.insert(&id2, &[Value::String("active".into())]).unwrap();

    let results = idx.lookup_eq(&[Value::String("active".into())]).unwrap();
    assert_eq!(results.len(), 2);
}

#[test]
fn field_index_lookup_eq_no_match() {
    let (partition, _dir) = open_test_partition();
    let idx = FieldIndex::new(
        partition,
        vec![("status".into(), FieldType::Text)],
    );
    let id1 = LogicalId::from_string("e1");
    idx.insert(&id1, &[Value::String("active".into())]).unwrap();

    let results = idx.lookup_eq(&[Value::String("inactive".into())]).unwrap();
    assert!(results.is_empty());
}

#[test]
fn field_index_remove() {
    let (partition, _dir) = open_test_partition();
    let idx = FieldIndex::new(
        partition,
        vec![("status".into(), FieldType::Text)],
    );
    let id1 = LogicalId::from_string("e1");
    idx.insert(&id1, &[Value::String("active".into())]).unwrap();
    idx.remove(&id1, &[Value::String("active".into())]).unwrap();

    let results = idx.lookup_eq(&[Value::String("active".into())]).unwrap();
    assert!(results.is_empty());
}

#[test]
fn field_index_compound_key_prefix_lookup() {
    let (partition, _dir) = open_test_partition();
    let idx = FieldIndex::new(
        partition,
        vec![
            ("venue_id".into(), FieldType::Text),
            ("start_date".into(), FieldType::DateTime),
        ],
    );
    let id1 = LogicalId::from_string("e1");
    let id2 = LogicalId::from_string("e2");
    let id3 = LogicalId::from_string("e3");

    idx.insert(&id1, &[Value::String("v1".into()), Value::String("2024-01-01".into())]).unwrap();
    idx.insert(&id2, &[Value::String("v1".into()), Value::String("2024-06-01".into())]).unwrap();
    idx.insert(&id3, &[Value::String("v2".into()), Value::String("2024-01-01".into())]).unwrap();

    // Prefix lookup on venue_id only
    let results = idx.lookup_prefix(&[Value::String("v1".into())]).unwrap();
    assert_eq!(results.len(), 2);
}

#[test]
fn field_index_range_lookup() {
    let (partition, _dir) = open_test_partition();
    let idx = FieldIndex::new(
        partition,
        vec![("score".into(), FieldType::Int)],
    );
    for i in 0..10 {
        let id = LogicalId::from_string(&format!("e{i}"));
        idx.insert(&id, &[Value::Int(i * 10)]).unwrap();
    }
    // Range 20..=50 should match e2, e3, e4, e5
    let results = idx.lookup_range(&[Value::Int(20)], &[Value::Int(50)]).unwrap();
    assert_eq!(results.len(), 4);
}

#[test]
fn field_index_lookup_eq_wrong_count_fails() {
    let (partition, _dir) = open_test_partition();
    let idx = FieldIndex::new(
        partition,
        vec![
            ("a".into(), FieldType::Text),
            ("b".into(), FieldType::Text),
        ],
    );
    // lookup_eq with 1 value on a 2-field index should fail
    let result = idx.lookup_eq(&[Value::String("x".into())]);
    assert!(result.is_err());
}
```

- [ ] **Step 7: Implement FieldIndex struct**

Add to `crates/trondb-core/src/field_index.rs` above the tests:

```rust
// ---------------------------------------------------------------------------
// FieldIndex — Fjall-backed sortable field index
// ---------------------------------------------------------------------------

pub struct FieldIndex {
    partition: fjall::PartitionHandle,
    field_types: Vec<(String, FieldType)>,
}

impl FieldIndex {
    pub fn new(
        partition: fjall::PartitionHandle,
        field_types: Vec<(String, FieldType)>,
    ) -> Self {
        Self { partition, field_types }
    }

    pub fn insert(&self, entity_id: &LogicalId, values: &[Value]) -> Result<(), EngineError> {
        let types: Vec<FieldType> = self.field_types.iter().map(|(_, t)| t.clone()).collect();
        let key = encode_compound_key(values, &types)?;
        // Append entity ID to key to allow multiple entities with same field values
        let mut full_key = key;
        full_key.push(0x00);
        full_key.extend_from_slice(entity_id.as_str().as_bytes());

        self.partition.insert(&full_key, entity_id.as_str().as_bytes())
            .map_err(|e| EngineError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn remove(&self, entity_id: &LogicalId, values: &[Value]) -> Result<(), EngineError> {
        let types: Vec<FieldType> = self.field_types.iter().map(|(_, t)| t.clone()).collect();
        let key = encode_compound_key(values, &types)?;
        let mut full_key = key;
        full_key.push(0x00);
        full_key.extend_from_slice(entity_id.as_str().as_bytes());

        self.partition.remove(&full_key)
            .map_err(|e| EngineError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn lookup_eq(&self, values: &[Value]) -> Result<Vec<LogicalId>, EngineError> {
        if values.len() != self.field_types.len() {
            return Err(EngineError::InvalidQuery(format!(
                "lookup_eq requires {} values, got {}",
                self.field_types.len(),
                values.len()
            )));
        }
        let types: Vec<FieldType> = self.field_types.iter().map(|(_, t)| t.clone()).collect();
        let prefix = encode_compound_key(values, &types)?;
        self.scan_prefix(&prefix)
    }

    pub fn lookup_prefix(&self, values: &[Value]) -> Result<Vec<LogicalId>, EngineError> {
        let types: Vec<FieldType> = self.field_types.iter()
            .take(values.len())
            .map(|(_, t)| t.clone())
            .collect();
        let prefix = encode_compound_key(values, &types)?;
        self.scan_prefix(&prefix)
    }

    pub fn lookup_range(
        &self,
        from: &[Value],
        to: &[Value],
    ) -> Result<Vec<LogicalId>, EngineError> {
        let types: Vec<FieldType> = self.field_types.iter().map(|(_, t)| t.clone()).collect();
        let from_key = encode_compound_key(from, &types)?;
        let to_key = encode_compound_key(to, &types)?;

        let mut results = Vec::new();
        for kv in self.partition.range(from_key..=to_key) {
            let (_, v) = kv.map_err(|e| EngineError::Storage(e.to_string()))?;
            if let Ok(id_str) = std::str::from_utf8(&v) {
                results.push(LogicalId::from_string(id_str));
            }
        }
        Ok(results)
    }

    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<LogicalId>, EngineError> {
        let mut results = Vec::new();
        for kv in self.partition.prefix(prefix) {
            let (_, v) = kv.map_err(|e| EngineError::Storage(e.to_string()))?;
            if let Ok(id_str) = std::str::from_utf8(&v) {
                results.push(LogicalId::from_string(id_str));
            }
        }
        Ok(results)
    }

    pub fn field_types(&self) -> &[(String, FieldType)] {
        &self.field_types
    }
}
```

- [ ] **Step 8: Run all tests**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 9: Commit**

```bash
git add crates/trondb-core/src/field_index.rs crates/trondb-core/src/lib.rs
git commit -m "feat: add FieldIndex with sortable byte encoding — equality, prefix, range lookups"
```

---

### Task 4: SparseIndex

**Files:**
- Create: `crates/trondb-core/src/sparse_index.rs`
- Modify: `crates/trondb-core/src/lib.rs` (add `pub mod sparse_index;`)

- [ ] **Step 1: Add module declaration**

Add `pub mod sparse_index;` to `crates/trondb-core/src/lib.rs`.

- [ ] **Step 2: Write tests first**

Create `crates/trondb-core/src/sparse_index.rs`:

```rust
use std::collections::HashMap;
use dashmap::DashMap;
use crate::types::LogicalId;

const MIN_WEIGHT: f32 = 0.001;

// ---------------------------------------------------------------------------
// SparseIndex — RAM inverted index for SPLADE-style sparse vectors
// ---------------------------------------------------------------------------

pub struct SparseIndex {
    postings: DashMap<u32, Vec<(LogicalId, f32)>>,
}

impl SparseIndex {
    pub fn new() -> Self {
        Self {
            postings: DashMap::new(),
        }
    }

    pub fn insert(&self, entity_id: &LogicalId, vector: &[(u32, f32)]) {
        todo!()
    }

    pub fn remove(&self, entity_id: &LogicalId, vector: &[(u32, f32)]) {
        todo!()
    }

    /// Search by accumulating dot-product scores, return top-k.
    pub fn search(&self, query: &[(u32, f32)], k: usize) -> Vec<(LogicalId, f32)> {
        todo!()
    }

    pub fn len(&self) -> usize {
        self.postings.iter().map(|e| e.value().len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.postings.is_empty()
    }
}

impl Default for SparseIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_id(s: &str) -> LogicalId {
        LogicalId::from_string(s)
    }

    #[test]
    fn insert_and_search_single() {
        let idx = SparseIndex::new();
        idx.insert(&make_id("e1"), &[(1, 0.8), (42, 0.5)]);

        let results = idx.search(&[(1, 1.0), (42, 1.0)], 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, make_id("e1"));
        // dot product = 0.8*1.0 + 0.5*1.0 = 1.3
        assert!((results[0].1 - 1.3).abs() < 0.01);
    }

    #[test]
    fn search_ranking_order() {
        let idx = SparseIndex::new();
        idx.insert(&make_id("e1"), &[(1, 0.8), (2, 0.2)]);
        idx.insert(&make_id("e2"), &[(1, 0.3), (2, 0.9)]);

        let results = idx.search(&[(1, 1.0)], 10);
        // e1 has higher weight on dim 1
        assert_eq!(results[0].0, make_id("e1"));
        assert_eq!(results[1].0, make_id("e2"));
    }

    #[test]
    fn search_top_k_limits() {
        let idx = SparseIndex::new();
        for i in 0..10 {
            idx.insert(&LogicalId::from_string(&format!("e{i}")), &[(1, i as f32 * 0.1)]);
        }
        let results = idx.search(&[(1, 1.0)], 3);
        assert_eq!(results.len(), 3);
        // Highest scores first
        assert_eq!(results[0].0, LogicalId::from_string("e9"));
    }

    #[test]
    fn remove_entity() {
        let idx = SparseIndex::new();
        idx.insert(&make_id("e1"), &[(1, 0.8), (42, 0.5)]);
        idx.remove(&make_id("e1"), &[(1, 0.8), (42, 0.5)]);
        assert!(idx.search(&[(1, 1.0)], 10).is_empty());
    }

    #[test]
    fn low_weight_filtered() {
        let idx = SparseIndex::new();
        idx.insert(&make_id("e1"), &[(1, 0.0001), (2, 0.5)]);
        // Dim 1 below MIN_WEIGHT, should not be indexed
        let results = idx.search(&[(1, 1.0)], 10);
        assert!(results.is_empty());
        // Dim 2 above threshold
        let results = idx.search(&[(2, 1.0)], 10);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_empty_index() {
        let idx = SparseIndex::new();
        let results = idx.search(&[(1, 1.0)], 10);
        assert!(results.is_empty());
    }

    #[test]
    fn len_counts_postings() {
        let idx = SparseIndex::new();
        idx.insert(&make_id("e1"), &[(1, 0.8), (2, 0.5)]);
        idx.insert(&make_id("e2"), &[(1, 0.3)]);
        assert_eq!(idx.len(), 3); // 2 postings for dim1, 1 for dim2
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p trondb-core -- sparse_index::tests 2>&1 | head -5`
Expected: FAIL (todo!() panics)

- [ ] **Step 4: Implement SparseIndex methods**

Replace the `todo!()` implementations:

```rust
pub fn insert(&self, entity_id: &LogicalId, vector: &[(u32, f32)]) {
    for &(dim, weight) in vector {
        if weight.abs() < MIN_WEIGHT {
            continue;
        }
        self.postings
            .entry(dim)
            .or_default()
            .push((entity_id.clone(), weight));
    }
}

pub fn remove(&self, entity_id: &LogicalId, vector: &[(u32, f32)]) {
    for &(dim, _) in vector {
        self.postings.entry(dim).and_modify(|entries| {
            entries.retain(|(id, _)| id != entity_id);
        });
        // Atomically remove empty posting lists
        self.postings.remove_if(&dim, |_, v| v.is_empty());
    }
}

pub fn search(&self, query: &[(u32, f32)], k: usize) -> Vec<(LogicalId, f32)> {
    let mut scores: HashMap<LogicalId, f32> = HashMap::new();

    for &(dim, query_weight) in query {
        if let Some(postings) = self.postings.get(&dim) {
            for (entity_id, posting_weight) in postings.iter() {
                *scores.entry(entity_id.clone()).or_default() += query_weight * posting_weight;
            }
        }
    }

    let mut results: Vec<(LogicalId, f32)> = scores.into_iter().collect();
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(k);
    results
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/src/sparse_index.rs crates/trondb-core/src/lib.rs
git commit -m "feat: add SparseIndex — RAM inverted index with dot-product search"
```

---

### Task 5: Hybrid RRF Merge

**Files:**
- Create: `crates/trondb-core/src/hybrid.rs`
- Modify: `crates/trondb-core/src/lib.rs` (add `pub mod hybrid;`)

- [ ] **Step 1: Add module declaration**

Add `pub mod hybrid;` to `crates/trondb-core/src/lib.rs`.

- [ ] **Step 2: Write tests and implementation**

Create `crates/trondb-core/src/hybrid.rs`:

```rust
use std::collections::HashMap;
use crate::types::LogicalId;

const DEFAULT_RRF_K: usize = 60;

/// Merge dense and sparse search results using Reciprocal Rank Fusion.
/// Ranks are 1-based (first result has rank 1).
/// Returns merged results sorted by descending RRF score.
pub fn merge_rrf(
    dense_results: &[(LogicalId, f32)],
    sparse_results: &[(LogicalId, f32)],
    rrf_k: usize,
) -> Vec<(LogicalId, f32)> {
    let mut scores: HashMap<LogicalId, f32> = HashMap::new();

    for (rank, (id, _)) in dense_results.iter().enumerate() {
        let rrf_score = 1.0 / (rrf_k as f32 + (rank + 1) as f32); // 1-based rank
        *scores.entry(id.clone()).or_default() += rrf_score;
    }

    for (rank, (id, _)) in sparse_results.iter().enumerate() {
        let rrf_score = 1.0 / (rrf_k as f32 + (rank + 1) as f32);
        *scores.entry(id.clone()).or_default() += rrf_score;
    }

    let mut results: Vec<(LogicalId, f32)> = scores.into_iter().collect();
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results
}

/// Default RRF k parameter.
pub fn default_rrf_k() -> usize {
    DEFAULT_RRF_K
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_id(s: &str) -> LogicalId {
        LogicalId::from_string(s)
    }

    #[test]
    fn rrf_merge_overlapping_results() {
        let dense = vec![
            (make_id("e1"), 0.95),
            (make_id("e2"), 0.80),
            (make_id("e3"), 0.60),
        ];
        let sparse = vec![
            (make_id("e2"), 1.5),
            (make_id("e1"), 1.0),
            (make_id("e4"), 0.5),
        ];

        let merged = merge_rrf(&dense, &sparse, 60);

        // e1: 1/(60+1) + 1/(60+2) = 0.01639 + 0.01613 = 0.03252
        // e2: 1/(60+2) + 1/(60+1) = 0.01613 + 0.01639 = 0.03252
        // Both e1 and e2 appear in both lists — they should have the highest scores
        let top_ids: Vec<&LogicalId> = merged.iter().map(|(id, _)| id).collect();
        assert!(top_ids.contains(&&make_id("e1")));
        assert!(top_ids.contains(&&make_id("e2")));
        assert_eq!(merged.len(), 4); // e1, e2, e3, e4
    }

    #[test]
    fn rrf_merge_disjoint_results() {
        let dense = vec![(make_id("e1"), 0.9)];
        let sparse = vec![(make_id("e2"), 1.0)];

        let merged = merge_rrf(&dense, &sparse, 60);
        assert_eq!(merged.len(), 2);
        // Both should have same score (both rank 1 in their respective lists)
        assert!((merged[0].1 - merged[1].1).abs() < 0.001);
    }

    #[test]
    fn rrf_merge_empty_inputs() {
        let merged = merge_rrf(&[], &[], 60);
        assert!(merged.is_empty());

        let dense = vec![(make_id("e1"), 0.9)];
        let merged = merge_rrf(&dense, &[], 60);
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn rrf_scores_decrease_with_rank() {
        let dense = vec![
            (make_id("e1"), 0.9),
            (make_id("e2"), 0.8),
            (make_id("e3"), 0.7),
        ];
        let merged = merge_rrf(&dense, &[], 60);
        assert!(merged[0].1 > merged[1].1);
        assert!(merged[1].1 > merged[2].1);
    }

    #[test]
    fn entity_in_both_lists_ranks_higher_than_single() {
        let dense = vec![
            (make_id("e1"), 0.9),
            (make_id("e2"), 0.8),
        ];
        let sparse = vec![
            (make_id("e1"), 1.0),
            (make_id("e3"), 0.5),
        ];

        let merged = merge_rrf(&dense, &sparse, 60);
        // e1 is in both lists (rank 1 in both), should be first
        assert_eq!(merged[0].0, make_id("e1"));
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-core/src/hybrid.rs crates/trondb-core/src/lib.rs
git commit -m "feat: add RRF merge for hybrid SEARCH — 1-based ranking, k=60 default"
```

---

## Chunk 2: TQL + Core Migration

### Task 6: TQL Lexer Tokens

**Files:**
- Modify: `crates/trondb-tql/src/token.rs`

- [ ] **Step 1: Add new tokens**

Add the following tokens to the `Token` enum in `crates/trondb-tql/src/token.rs`, in the keywords section (after `Delete`):

```rust
#[token("REPRESENTATION", priority = 10, ignore(ascii_case))]
Representation,

#[token("MODEL", priority = 10, ignore(ascii_case))]
Model,

#[token("METRIC", priority = 10, ignore(ascii_case))]
TokenMetric,

#[token("SPARSE", priority = 10, ignore(ascii_case))]
Sparse,

#[token("FIELD", priority = 10, ignore(ascii_case))]
Field,

#[token("DATETIME", priority = 10, ignore(ascii_case))]
DateTime,

#[token("TEXT", priority = 10, ignore(ascii_case))]
Text,

#[token("BOOL", priority = 10, ignore(ascii_case))]
TokenBool,

#[token("INT", priority = 10, ignore(ascii_case))]
TokenInt,

#[token("FLOAT", priority = 10, ignore(ascii_case))]
TokenFloat,

#[token("ENTITY_REF", priority = 10, ignore(ascii_case))]
EntityRef,

#[token("INDEX", priority = 10, ignore(ascii_case))]
Index,

#[token("ON", priority = 10, ignore(ascii_case))]
On,

#[token("INNER_PRODUCT", priority = 10, ignore(ascii_case))]
InnerProduct,

#[token("COSINE", priority = 10, ignore(ascii_case))]
Cosine,
```

Add in the symbols section:

```rust
#[token(":")]
Colon,
```

Note: Token variants named `TokenMetric`, `TokenBool`, `TokenInt`, `TokenFloat` to avoid collision with Rust keywords and the existing `FloatLit`/`IntLit` literal tokens.

- [ ] **Step 2: Add lexer tests**

Add to the tests module:

```rust
#[test]
fn lex_representation_tokens() {
    let tokens = lex("REPRESENTATION identity MODEL 'jina-v4' DIMENSIONS 1024 METRIC COSINE");
    assert_eq!(tokens[0], Token::Representation);
    assert_eq!(tokens[1], Token::Ident("identity".to_string()));
    assert_eq!(tokens[2], Token::Model);
    assert_eq!(tokens[3], Token::StringLit("jina-v4".to_string()));
    assert_eq!(tokens[4], Token::Dimensions);
    assert_eq!(tokens[5], Token::IntLit(1024));
    assert_eq!(tokens[6], Token::TokenMetric);
    assert_eq!(tokens[7], Token::Cosine);
}

#[test]
fn lex_sparse_vector_literal() {
    let tokens = lex("[1:0.8, 42:0.5]");
    assert_eq!(tokens, vec![
        Token::LBracket,
        Token::IntLit(1),
        Token::Colon,
        Token::FloatLit(0.8),
        Token::Comma,
        Token::IntLit(42),
        Token::Colon,
        Token::FloatLit(0.5),
        Token::RBracket,
    ]);
}

#[test]
fn lex_field_and_index_tokens() {
    let tokens = lex("FIELD status TEXT INDEX idx_status ON");
    assert_eq!(tokens[0], Token::Field);
    assert_eq!(tokens[1], Token::Ident("status".to_string()));
    assert_eq!(tokens[2], Token::Text);
    assert_eq!(tokens[3], Token::Index);
    assert_eq!(tokens[4], Token::Ident("idx_status".to_string()));
    assert_eq!(tokens[5], Token::On);
}

#[test]
fn lex_sparse_keyword() {
    let tokens = lex("SPARSE INNER_PRODUCT ENTITY_REF BOOL INT FLOAT DATETIME");
    assert_eq!(tokens[0], Token::Sparse);
    assert_eq!(tokens[1], Token::InnerProduct);
    assert_eq!(tokens[2], Token::EntityRef);
    assert_eq!(tokens[3], Token::TokenBool);
    assert_eq!(tokens[4], Token::TokenInt);
    assert_eq!(tokens[5], Token::TokenFloat);
    assert_eq!(tokens[6], Token::DateTime);
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p trondb-tql`
Expected: all tests pass (including old ones)

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-tql/src/token.rs
git commit -m "feat(tql): add 16 new tokens — REPRESENTATION, SPARSE, FIELD, INDEX, Colon, etc."
```

---

### Task 7: TQL AST + Parser

**Files:**
- Modify: `crates/trondb-tql/src/ast.rs`
- Modify: `crates/trondb-tql/src/parser.rs`

This task changes the AST types (breaking change for trondb-core). After this task completes, verify with `cargo test -p trondb-tql`. The trondb-core crate will NOT compile until Task 8.

- [ ] **Step 1: Replace AST types**

Rewrite `crates/trondb-tql/src/ast.rs`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateCollection(CreateCollectionStmt),
    Insert(InsertStmt),
    Fetch(FetchStmt),
    Search(SearchStmt),
    Explain(Box<Statement>),
    CreateEdgeType(CreateEdgeTypeStmt),
    InsertEdge(InsertEdgeStmt),
    DeleteEdge(DeleteEdgeStmt),
    Traverse(TraverseStmt),
}

// --- CREATE COLLECTION (expanded) ---

#[derive(Debug, Clone, PartialEq)]
pub struct CreateCollectionStmt {
    pub name: String,
    pub representations: Vec<RepresentationDecl>,
    pub fields: Vec<FieldDecl>,
    pub indexes: Vec<IndexDecl>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepresentationDecl {
    pub name: String,
    pub model: Option<String>,
    pub dimensions: Option<usize>,
    pub metric: Metric,
    pub sparse: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Metric {
    Cosine,
    InnerProduct,
}

impl Default for Metric {
    fn default() -> Self {
        Metric::Cosine
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldDecl {
    pub name: String,
    pub field_type: FieldType,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Text,
    DateTime,
    Bool,
    Int,
    Float,
    EntityRef(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexDecl {
    pub name: String,
    pub fields: Vec<String>,
    pub partial_condition: Option<WhereClause>,
}

// --- INSERT (expanded with named representations) ---

#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    pub collection: String,
    pub fields: Vec<String>,
    pub values: Vec<Literal>,
    pub vectors: Vec<(String, VectorLiteral)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VectorLiteral {
    Dense(Vec<f64>),
    Sparse(Vec<(u32, f32)>),
}

// --- FETCH (unchanged) ---

#[derive(Debug, Clone, PartialEq)]
pub struct FetchStmt {
    pub collection: String,
    pub fields: FieldList,
    pub filter: Option<WhereClause>,
    pub limit: Option<usize>,
}

// --- SEARCH (expanded) ---

#[derive(Debug, Clone, PartialEq)]
pub struct SearchStmt {
    pub collection: String,
    pub fields: FieldList,
    pub dense_vector: Option<Vec<f64>>,
    pub sparse_vector: Option<Vec<(u32, f32)>>,
    pub filter: Option<WhereClause>,
    pub confidence: Option<f64>,
    pub limit: Option<usize>,
}

// --- Common types ---

#[derive(Debug, Clone, PartialEq)]
pub enum FieldList {
    All,
    Named(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum WhereClause {
    Eq(String, Literal),
    Gt(String, Literal),
    Lt(String, Literal),
    And(Box<WhereClause>, Box<WhereClause>),
    Or(Box<WhereClause>, Box<WhereClause>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

// --- Edge types (unchanged from Phase 5) ---

#[derive(Debug, Clone, PartialEq)]
pub struct CreateEdgeTypeStmt {
    pub name: String,
    pub from_collection: String,
    pub to_collection: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertEdgeStmt {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
    pub metadata: Vec<(String, Literal)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteEdgeStmt {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TraverseStmt {
    pub edge_type: String,
    pub from_id: String,
    pub depth: usize,
    pub limit: Option<usize>,
}
```

- [ ] **Step 2: Rewrite parser for new CREATE COLLECTION**

Replace `parse_create_collection` in `crates/trondb-tql/src/parser.rs`:

```rust
fn parse_create_collection(&mut self) -> Result<Statement, ParseError> {
    self.advance(); // COLLECTION
    let name = self.expect_ident()?;
    self.expect(&Token::LParen)?;

    let mut representations = Vec::new();
    let mut fields = Vec::new();
    let mut indexes = Vec::new();

    loop {
        match self.peek() {
            Some(Token::RParen) => {
                self.advance();
                break;
            }
            Some(Token::Representation) => {
                representations.push(self.parse_representation_decl()?);
            }
            Some(Token::Field) => {
                fields.push(self.parse_field_decl()?);
            }
            Some(Token::Index) => {
                indexes.push(self.parse_index_decl()?);
            }
            Some(tok) => {
                let tok_str = format!("{tok:?}");
                let pos = self.tokens[self.pos].1.start;
                return Err(ParseError::UnexpectedToken {
                    pos,
                    expected: "REPRESENTATION, FIELD, INDEX, or )".into(),
                    got: tok_str,
                });
            }
            None => return Err(ParseError::UnexpectedEof("expected ) or declaration".into())),
        }

        // Consume optional comma between declarations
        if self.peek() == Some(&Token::Comma) {
            self.advance();
        }
    }

    self.expect(&Token::Semicolon)?;
    Ok(Statement::CreateCollection(CreateCollectionStmt {
        name,
        representations,
        fields,
        indexes,
    }))
}

fn parse_representation_decl(&mut self) -> Result<RepresentationDecl, ParseError> {
    self.advance(); // REPRESENTATION
    let name = self.expect_ident()?;
    let mut model = None;
    let mut dimensions = None;
    let mut metric = Metric::Cosine;
    let mut sparse = false;

    // Parse optional attributes in any order
    loop {
        match self.peek() {
            Some(Token::Model) => {
                self.advance();
                model = Some(self.expect_string_lit()?);
            }
            Some(Token::Dimensions) => {
                self.advance();
                dimensions = Some(self.expect_int()? as usize);
            }
            Some(Token::TokenMetric) => {
                self.advance();
                match self.peek() {
                    Some(Token::Cosine) => { self.advance(); metric = Metric::Cosine; }
                    Some(Token::InnerProduct) => { self.advance(); metric = Metric::InnerProduct; }
                    _ => return Err(ParseError::InvalidSyntax("expected COSINE or INNER_PRODUCT".into())),
                }
            }
            Some(Token::Sparse) => {
                self.advance();
                match self.peek() {
                    Some(Token::True) => { self.advance(); sparse = true; }
                    Some(Token::False) => { self.advance(); sparse = false; }
                    _ => return Err(ParseError::InvalidSyntax("expected true or false after SPARSE".into())),
                }
            }
            _ => break,
        }
    }

    Ok(RepresentationDecl { name, model, dimensions, metric, sparse })
}

fn parse_field_decl(&mut self) -> Result<FieldDecl, ParseError> {
    self.advance(); // FIELD
    let name = self.expect_ident()?;
    let field_type = self.parse_field_type()?;
    Ok(FieldDecl { name, field_type })
}

fn parse_field_type(&mut self) -> Result<FieldType, ParseError> {
    match self.advance() {
        Some((Token::Text, _)) => Ok(FieldType::Text),
        Some((Token::DateTime, _)) => Ok(FieldType::DateTime),
        Some((Token::TokenBool, _)) => Ok(FieldType::Bool),
        Some((Token::TokenInt, _)) => Ok(FieldType::Int),
        Some((Token::TokenFloat, _)) => Ok(FieldType::Float),
        Some((Token::EntityRef, _)) => {
            self.expect(&Token::LParen)?;
            let collection = self.expect_ident()?;
            self.expect(&Token::RParen)?;
            Ok(FieldType::EntityRef(collection))
        }
        Some((tok, pos)) => Err(ParseError::UnexpectedToken {
            pos,
            expected: "field type (TEXT, DATETIME, BOOL, INT, FLOAT, ENTITY_REF)".into(),
            got: format!("{tok:?}"),
        }),
        None => Err(ParseError::UnexpectedEof("expected field type".into())),
    }
}

fn parse_index_decl(&mut self) -> Result<IndexDecl, ParseError> {
    self.advance(); // INDEX
    let name = self.expect_ident()?;
    self.expect(&Token::On)?;
    self.expect(&Token::LParen)?;

    let mut fields = vec![self.expect_ident()?];
    while self.peek() == Some(&Token::Comma) {
        self.advance();
        fields.push(self.expect_ident()?);
    }
    self.expect(&Token::RParen)?;

    let partial_condition = if self.peek() == Some(&Token::Where) {
        self.advance();
        Some(self.parse_where_clause()?)
    } else {
        None
    };

    Ok(IndexDecl { name, fields, partial_condition })
}
```

- [ ] **Step 3: Rewrite parse_search for sparse/hybrid/WHERE**

Replace `parse_search` in parser.rs:

```rust
fn parse_search(&mut self) -> Result<Statement, ParseError> {
    self.advance(); // SEARCH
    let collection = self.expect_ident()?;

    // Optional WHERE clause (ScalarPreFilter)
    let filter = if self.peek() == Some(&Token::Where) {
        self.advance();
        Some(self.parse_where_clause()?)
    } else {
        None
    };

    // Parse NEAR VECTOR and/or NEAR SPARSE
    let mut dense_vector = None;
    let mut sparse_vector = None;

    while self.peek() == Some(&Token::Near) {
        self.advance(); // NEAR
        match self.peek() {
            Some(Token::Vector) => {
                self.advance(); // VECTOR
                dense_vector = Some(self.parse_float_list()?);
            }
            Some(Token::Sparse) => {
                self.advance(); // SPARSE
                sparse_vector = Some(self.parse_sparse_vector_list()?);
            }
            Some(tok) => {
                let tok_str = format!("{tok:?}");
                let pos = self.tokens[self.pos].1.start;
                return Err(ParseError::UnexpectedToken {
                    pos,
                    expected: "VECTOR or SPARSE".into(),
                    got: tok_str,
                });
            }
            None => return Err(ParseError::UnexpectedEof("expected VECTOR or SPARSE".into())),
        }
    }

    if dense_vector.is_none() && sparse_vector.is_none() {
        return Err(ParseError::InvalidSyntax(
            "SEARCH requires at least one NEAR VECTOR or NEAR SPARSE clause".into(),
        ));
    }

    let confidence = if self.peek() == Some(&Token::Confidence) {
        self.advance();
        self.expect(&Token::Gt)?;
        Some(self.parse_number_as_f64()?)
    } else {
        None
    };

    let limit = if self.peek() == Some(&Token::Limit) {
        self.advance();
        Some(self.expect_int()? as usize)
    } else {
        None
    };

    self.expect(&Token::Semicolon)?;
    Ok(Statement::Search(SearchStmt {
        collection,
        fields: FieldList::All,
        dense_vector,
        sparse_vector,
        filter,
        confidence,
        limit,
    }))
}

fn parse_sparse_vector_list(&mut self) -> Result<Vec<(u32, f32)>, ParseError> {
    self.expect(&Token::LBracket)?;
    let mut entries = Vec::new();

    loop {
        if self.peek() == Some(&Token::RBracket) {
            break;
        }
        let dim = self.expect_int()? as u32;
        self.expect(&Token::Colon)?;
        let weight = self.parse_number_as_f64()? as f32;
        entries.push((dim, weight));

        if self.peek() == Some(&Token::Comma) {
            self.advance();
        } else {
            break;
        }
    }

    self.expect(&Token::RBracket)?;
    Ok(entries)
}
```

- [ ] **Step 4: Rewrite parse_insert_entity for named representations**

Replace `parse_insert_entity` in parser.rs:

```rust
fn parse_insert_entity(&mut self) -> Result<Statement, ParseError> {
    self.advance(); // INTO
    let collection = self.expect_ident()?;
    self.expect(&Token::LParen)?;

    let mut fields = vec![self.expect_ident()?];
    while self.peek() == Some(&Token::Comma) {
        self.advance();
        fields.push(self.expect_ident()?);
    }
    self.expect(&Token::RParen)?;

    self.expect(&Token::Values)?;
    self.expect(&Token::LParen)?;

    let mut values = vec![self.parse_literal()?];
    while self.peek() == Some(&Token::Comma) {
        self.advance();
        values.push(self.parse_literal()?);
    }
    self.expect(&Token::RParen)?;

    // Parse named representation vectors (no backward-compat shorthand per spec)
    let mut vectors = Vec::new();

    // Named representations
    while self.peek() == Some(&Token::Representation) {
        self.advance(); // REPRESENTATION
        let repr_name = self.expect_ident()?;
        match self.peek() {
            Some(Token::Vector) => {
                self.advance();
                let vec = self.parse_float_list()?;
                vectors.push((repr_name, VectorLiteral::Dense(vec)));
            }
            Some(Token::Sparse) => {
                self.advance();
                let vec = self.parse_sparse_vector_list()?;
                vectors.push((repr_name, VectorLiteral::Sparse(vec)));
            }
            Some(tok) => {
                let tok_str = format!("{tok:?}");
                let pos = self.tokens[self.pos].1.start;
                return Err(ParseError::UnexpectedToken {
                    pos,
                    expected: "VECTOR or SPARSE".into(),
                    got: tok_str,
                });
            }
            None => return Err(ParseError::UnexpectedEof("expected VECTOR or SPARSE".into())),
        }
    }

    self.expect(&Token::Semicolon)?;
    Ok(Statement::Insert(InsertStmt {
        collection,
        fields,
        values,
        vectors,
    }))
}
```

- [ ] **Step 5: Update parser tests**

Replace all existing parser tests and add new ones. Key tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_create_collection_block() {
        let stmt = parse("CREATE COLLECTION venues (
            REPRESENTATION identity MODEL 'jina-v4' DIMENSIONS 1024 METRIC COSINE,
            FIELD status TEXT,
            FIELD venue_id ENTITY_REF(venues),
            INDEX idx_status ON (status)
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                assert_eq!(c.name, "venues");
                assert_eq!(c.representations.len(), 1);
                assert_eq!(c.representations[0].name, "identity");
                assert_eq!(c.representations[0].dimensions, Some(1024));
                assert_eq!(c.representations[0].metric, Metric::Cosine);
                assert!(!c.representations[0].sparse);
                assert_eq!(c.fields.len(), 2);
                assert_eq!(c.fields[0].name, "status");
                assert_eq!(c.fields[0].field_type, FieldType::Text);
                assert_eq!(c.indexes.len(), 1);
                assert_eq!(c.indexes[0].name, "idx_status");
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_create_collection_sparse_repr() {
        let stmt = parse("CREATE COLLECTION docs (
            REPRESENTATION sparse_title MODEL 'splade-v3' METRIC INNER_PRODUCT SPARSE true
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                assert_eq!(c.representations[0].sparse, true);
                assert_eq!(c.representations[0].metric, Metric::InnerProduct);
                assert_eq!(c.representations[0].dimensions, None);
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_create_collection_partial_index() {
        let stmt = parse("CREATE COLLECTION events (
            FIELD publish_ready BOOL,
            INDEX idx_ready ON (publish_ready) WHERE publish_ready = true
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                assert!(c.indexes[0].partial_condition.is_some());
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_create_collection_compound_index() {
        let stmt = parse("CREATE COLLECTION events (
            FIELD venue_id TEXT,
            FIELD start_date DATETIME,
            INDEX idx_venue_start ON (venue_id, start_date)
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                assert_eq!(c.indexes[0].fields, vec!["venue_id", "start_date"]);
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_search_dense_only() {
        let stmt = parse("SEARCH venues NEAR VECTOR [0.1, 0.2, 0.3] LIMIT 20;").unwrap();
        match stmt {
            Statement::Search(s) => {
                assert!(s.dense_vector.is_some());
                assert!(s.sparse_vector.is_none());
                assert!(s.filter.is_none());
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn parse_search_sparse_only() {
        let stmt = parse("SEARCH venues NEAR SPARSE [1:0.8, 42:0.5, 1337:0.3] LIMIT 20;").unwrap();
        match stmt {
            Statement::Search(s) => {
                assert!(s.dense_vector.is_none());
                let sparse = s.sparse_vector.unwrap();
                assert_eq!(sparse.len(), 3);
                assert_eq!(sparse[0], (1, 0.8));
                assert_eq!(sparse[1], (42, 0.5));
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn parse_search_hybrid() {
        let stmt = parse("SEARCH venues NEAR VECTOR [0.1, 0.2] NEAR SPARSE [1:0.8] LIMIT 10;").unwrap();
        match stmt {
            Statement::Search(s) => {
                assert!(s.dense_vector.is_some());
                assert!(s.sparse_vector.is_some());
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn parse_search_with_where() {
        let stmt = parse("SEARCH venues WHERE h3_res4 = '89283' NEAR VECTOR [0.1, 0.2] LIMIT 10;").unwrap();
        match stmt {
            Statement::Search(s) => {
                assert!(s.filter.is_some());
                assert!(s.dense_vector.is_some());
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn parse_search_no_near_fails() {
        let result = parse("SEARCH venues LIMIT 10;");
        assert!(result.is_err());
    }

    #[test]
    fn parse_insert_with_named_repr() {
        let stmt = parse("INSERT INTO venues (id, name) VALUES ('v1', 'Pub')
            REPRESENTATION identity VECTOR [0.1, 0.2, 0.3];").unwrap();
        match stmt {
            Statement::Insert(i) => {
                assert_eq!(i.vectors.len(), 1);
                assert_eq!(i.vectors[0].0, "identity");
                match &i.vectors[0].1 {
                    VectorLiteral::Dense(v) => assert_eq!(v.len(), 3),
                    _ => panic!("expected Dense"),
                }
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_with_sparse_repr() {
        let stmt = parse("INSERT INTO venues (id, name) VALUES ('v1', 'Pub')
            REPRESENTATION sparse_title SPARSE [1:0.8, 42:0.5];").unwrap();
        match stmt {
            Statement::Insert(i) => {
                assert_eq!(i.vectors.len(), 1);
                match &i.vectors[0].1 {
                    VectorLiteral::Sparse(s) => assert_eq!(s.len(), 2),
                    _ => panic!("expected Sparse"),
                }
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_multiple_reprs() {
        let stmt = parse("INSERT INTO venues (id) VALUES ('v1')
            REPRESENTATION identity VECTOR [0.1, 0.2]
            REPRESENTATION sparse SPARSE [1:0.5];").unwrap();
        match stmt {
            Statement::Insert(i) => assert_eq!(i.vectors.len(), 2),
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_no_vector() {
        let stmt = parse("INSERT INTO venues (id, name) VALUES ('v1', 'Pub');").unwrap();
        match stmt {
            Statement::Insert(i) => assert!(i.vectors.is_empty()),
            _ => panic!("expected Insert"),
        }
    }

    // Keep existing tests for FETCH, edge operations, TRAVERSE
    #[test]
    fn parse_fetch_all() {
        let stmt = parse("FETCH * FROM venues;").unwrap();
        assert_eq!(
            stmt,
            Statement::Fetch(FetchStmt {
                collection: "venues".to_string(),
                fields: FieldList::All,
                filter: None,
                limit: None,
            })
        );
    }

    #[test]
    fn parse_fetch_with_where() {
        let stmt = parse("FETCH name, city FROM venues WHERE city = 'London';").unwrap();
        match &stmt {
            Statement::Fetch(f) => {
                assert_eq!(f.collection, "venues");
                assert!(f.filter.is_some());
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_fetch_where_and() {
        let stmt = parse("FETCH * FROM venues WHERE city = 'London' AND status = 'active';").unwrap();
        match &stmt {
            Statement::Fetch(f) => {
                match &f.filter {
                    Some(WhereClause::And(_, _)) => {}
                    other => panic!("expected And clause, got {other:?}"),
                }
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_fetch_with_limit() {
        let stmt = parse("FETCH * FROM venues LIMIT 10;").unwrap();
        match &stmt {
            Statement::Fetch(f) => assert_eq!(f.limit, Some(10)),
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn parse_error_missing_semicolon() {
        assert!(parse("FETCH * FROM venues").is_err());
    }

    #[test]
    fn parse_create_edge() {
        let stmt = parse("CREATE EDGE knows FROM people TO people;").unwrap();
        assert_eq!(
            stmt,
            Statement::CreateEdgeType(CreateEdgeTypeStmt {
                name: "knows".to_string(),
                from_collection: "people".to_string(),
                to_collection: "people".to_string(),
            })
        );
    }

    #[test]
    fn parse_insert_edge() {
        let stmt = parse("INSERT EDGE knows FROM 'v1' TO 'v2';").unwrap();
        match &stmt {
            Statement::InsertEdge(e) => {
                assert_eq!(e.edge_type, "knows");
                assert!(e.metadata.is_empty());
            }
            _ => panic!("expected InsertEdge"),
        }
    }

    #[test]
    fn parse_insert_edge_with_metadata() {
        let stmt = parse("INSERT EDGE knows FROM 'v1' TO 'v2' WITH (since = '2024');").unwrap();
        match &stmt {
            Statement::InsertEdge(e) => assert_eq!(e.metadata.len(), 1),
            _ => panic!("expected InsertEdge"),
        }
    }

    #[test]
    fn parse_delete_edge() {
        let stmt = parse("DELETE EDGE knows FROM 'v1' TO 'v2';").unwrap();
        assert_eq!(
            stmt,
            Statement::DeleteEdge(DeleteEdgeStmt {
                edge_type: "knows".to_string(),
                from_id: "v1".to_string(),
                to_id: "v2".to_string(),
            })
        );
    }

    #[test]
    fn parse_traverse() {
        let stmt = parse("TRAVERSE knows FROM 'v1';").unwrap();
        assert_eq!(
            stmt,
            Statement::Traverse(TraverseStmt {
                edge_type: "knows".to_string(),
                from_id: "v1".to_string(),
                depth: 1,
                limit: None,
            })
        );
    }

    #[test]
    fn parse_traverse_with_depth_and_limit() {
        let stmt = parse("TRAVERSE knows FROM 'v1' DEPTH 1 LIMIT 10;").unwrap();
        match &stmt {
            Statement::Traverse(t) => {
                assert_eq!(t.depth, 1);
                assert_eq!(t.limit, Some(10));
            }
            _ => panic!("expected Traverse"),
        }
    }
}
```

- [ ] **Step 6: Run TQL tests**

Run: `cargo test -p trondb-tql`
Expected: all tests pass

- [ ] **Step 7: Commit TQL changes**

```bash
git add crates/trondb-tql/src/ast.rs crates/trondb-tql/src/parser.rs
git commit -m "feat(tql): expand AST + parser — block CREATE COLLECTION, hybrid SEARCH, named INSERT representations"
```

Note: trondb-core will NOT compile after this commit. Task 8 fixes it.

---

### Task 8: Core Type Migration + Store + Planner

**Files:**
- Modify: `crates/trondb-core/src/types.rs` — Remove old Collection/FieldDef/FieldType, update Representation
- Modify: `crates/trondb-core/src/store.rs` — CollectionSchema storage, remove old create_collection(name, dims)
- Modify: `crates/trondb-core/src/planner.rs` — Strategy enums, expanded plan types, plan() takes schemas
- Modify: `crates/trondb-core/src/executor.rs` — Stub-compile with new types (functional changes in Tasks 9-10)
- Modify: `crates/trondb-core/src/lib.rs` — Fix compilation

This task makes the workspace compile again after the AST changes in Task 7.

**Breaking data change:** This task changes the serialized format of collections and entities in Fjall.
Existing `_meta` partition entries (`{name, dimensions}`) will NOT deserialize as `CollectionSchema`.
Requires a clean data directory — delete `trondb_data/` before running after this migration.
The WAL replay handler in Task 11 will also require a clean WAL directory.

- [ ] **Step 1: Update types.rs**

In `crates/trondb-core/src/types.rs`:

1. Remove the old `FieldType` enum (String/Int/Float/Bool variants)
2. Remove `FieldDef` struct
3. Remove `Collection` struct and its `impl` block
4. Rename `FieldType` to `FieldType` (since the old one is gone now)
5. Update `Representation` struct:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Representation {
    pub name: String,
    pub repr_type: ReprType,
    pub fields: Vec<String>,
    pub vector: VectorData,
    pub recipe_hash: [u8; 32],
    pub state: ReprState,
}
```

6. Update tests: remove `collection_validates_dimensions` test, update `FieldType` references to `FieldType` in the `collection_schema_round_trip` and `schema_field_type_variants` tests.

- [ ] **Step 2: Update store.rs**

Replace `create_collection` and `get_dimensions` in `crates/trondb-core/src/store.rs`:

```rust
use crate::types::{CollectionSchema, Entity, LogicalId};

// Remove: use crate::types::{Collection, Entity, LogicalId};

pub fn create_collection_schema(&self, schema: &CollectionSchema) -> Result<(), EngineError> {
    let key = format!("{COLLECTION_PREFIX}{}", schema.name);
    if self.meta.get(&key)
        .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?
        .is_some()
    {
        return Err(EngineError::CollectionAlreadyExists(schema.name.clone()));
    }

    let bytes = rmp_serde::to_vec_named(schema)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    self.meta.insert(&key, bytes)
        .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?;

    // Create entity partition
    self.keyspace
        .open_partition(&schema.name, PartitionCreateOptions::default())
        .map_err(|e| EngineError::Storage(e.to_string()))?;

    // Create field index partitions
    for idx in &schema.indexes {
        let partition_name = format!("fidx.{}.{}", schema.name, idx.name);
        self.keyspace
            .open_partition(&partition_name, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;
    }

    self.keyspace.persist(PersistMode::SyncAll)
        .map_err(|e| EngineError::Storage(e.to_string()))?;

    Ok(())
}

pub fn get_collection_schema(&self, name: &str) -> Result<CollectionSchema, EngineError> {
    let key = format!("{COLLECTION_PREFIX}{name}");
    let bytes = self.meta.get(&key)
        .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?
        .ok_or_else(|| EngineError::CollectionNotFound(name.to_owned()))?;
    rmp_serde::from_slice(&bytes).map_err(|e| EngineError::Storage(e.to_string()))
}

pub fn list_collection_schemas(&self) -> Vec<CollectionSchema> {
    self.meta
        .prefix(COLLECTION_PREFIX)
        .filter_map(|kv| {
            let (_k, v) = kv.ok()?;
            rmp_serde::from_slice(&v).ok()
        })
        .collect()
}
```

Keep `has_collection`, `list_collections` working. Remove `get_dimensions` (callers will use schema). Remove `create_collection(name, dims)`. Keep the old `get_collection_meta` but change it to return `CollectionSchema`.

Update the `insert` method to not check dimensions (validation moves to executor which knows about representations):

```rust
pub fn insert(&self, collection: &str, entity: Entity) -> Result<(), EngineError> {
    if !self.has_collection(collection) {
        return Err(EngineError::CollectionNotFound(collection.to_owned()));
    }

    let partition = self.keyspace
        .open_partition(collection, PartitionCreateOptions::default())
        .map_err(|e| EngineError::Storage(e.to_string()))?;

    let key = format!("{ENTITY_PREFIX}{}", entity.id);
    let bytes = rmp_serde::to_vec_named(&entity)
        .map_err(|e| EngineError::Storage(e.to_string()))?;

    partition.insert(&key, bytes)
        .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?;

    Ok(())
}
```

Add method to open a field index partition:

```rust
pub fn open_field_index_partition(&self, collection: &str, index_name: &str) -> Result<fjall::PartitionHandle, EngineError> {
    let partition_name = format!("fidx.{collection}.{index_name}");
    self.keyspace
        .open_partition(&partition_name, PartitionCreateOptions::default())
        .map_err(|e| EngineError::Storage(e.to_string()))
}
```

Update store tests to use new API.

- [ ] **Step 3: Update planner.rs**

Rewrite `crates/trondb-core/src/planner.rs`:

```rust
use dashmap::DashMap;
use crate::error::EngineError;
use crate::types::CollectionSchema;
use trondb_tql::{FieldList, Literal, Statement, WhereClause, VectorLiteral};

// ---------------------------------------------------------------------------
// Strategy types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum SearchStrategy {
    Hnsw,
    Sparse,
    Hybrid { rrf_k: usize },
}

#[derive(Debug, Clone)]
pub enum FetchStrategy {
    FullScan,
    FieldIndexLookup { index_name: String },
}

#[derive(Debug, Clone)]
pub struct PreFilter {
    pub index_name: String,
    pub condition: WhereClause,
}

// ---------------------------------------------------------------------------
// Plan types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Plan {
    CreateCollection(CreateCollectionPlan),
    Insert(InsertPlan),
    Fetch(FetchPlan),
    Search(SearchPlan),
    Explain(Box<Plan>),
    CreateEdgeType(CreateEdgeTypePlan),
    InsertEdge(InsertEdgePlan),
    DeleteEdge(DeleteEdgePlan),
    Traverse(TraversePlan),
}

#[derive(Debug, Clone)]
pub struct CreateCollectionPlan {
    pub name: String,
    pub representations: Vec<trondb_tql::RepresentationDecl>,
    pub fields: Vec<trondb_tql::FieldDecl>,
    pub indexes: Vec<trondb_tql::IndexDecl>,
}

#[derive(Debug, Clone)]
pub struct InsertPlan {
    pub collection: String,
    pub fields: Vec<String>,
    pub values: Vec<Literal>,
    pub vectors: Vec<(String, VectorLiteral)>,
}

#[derive(Debug, Clone)]
pub struct FetchPlan {
    pub collection: String,
    pub fields: FieldList,
    pub filter: Option<WhereClause>,
    pub limit: Option<usize>,
    pub strategy: FetchStrategy,
}

#[derive(Debug, Clone)]
pub struct SearchPlan {
    pub collection: String,
    pub dense_vector: Option<Vec<f64>>,
    pub sparse_vector: Option<Vec<(u32, f32)>>,
    pub strategy: SearchStrategy,
    pub pre_filter: Option<PreFilter>,
    pub k: usize,
    pub confidence_threshold: f64,
}

// Edge plan types unchanged
#[derive(Debug, Clone)]
pub struct CreateEdgeTypePlan {
    pub name: String,
    pub from_collection: String,
    pub to_collection: String,
}

#[derive(Debug, Clone)]
pub struct InsertEdgePlan {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
    pub metadata: Vec<(String, Literal)>,
}

#[derive(Debug, Clone)]
pub struct DeleteEdgePlan {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
}

#[derive(Debug, Clone)]
pub struct TraversePlan {
    pub edge_type: String,
    pub from_id: String,
    pub depth: usize,
    pub limit: Option<usize>,
}

// ---------------------------------------------------------------------------
// Planner
// ---------------------------------------------------------------------------

pub fn plan(stmt: &Statement, schemas: &DashMap<String, CollectionSchema>) -> Result<Plan, EngineError> {
    match stmt {
        Statement::CreateCollection(s) => Ok(Plan::CreateCollection(CreateCollectionPlan {
            name: s.name.clone(),
            representations: s.representations.clone(),
            fields: s.fields.clone(),
            indexes: s.indexes.clone(),
        })),

        Statement::Insert(s) => Ok(Plan::Insert(InsertPlan {
            collection: s.collection.clone(),
            fields: s.fields.clone(),
            values: s.values.clone(),
            vectors: s.vectors.clone(),
        })),

        Statement::Fetch(s) => {
            let strategy = select_fetch_strategy(&s.collection, &s.filter, schemas);
            Ok(Plan::Fetch(FetchPlan {
                collection: s.collection.clone(),
                fields: s.fields.clone(),
                filter: s.filter.clone(),
                limit: s.limit,
                strategy,
            }))
        }

        Statement::Search(s) => {
            let strategy = select_search_strategy(s.dense_vector.is_some(), s.sparse_vector.is_some());
            let pre_filter = select_pre_filter(&s.collection, &s.filter, schemas)?;

            // Validate sparse representation exists when needed
            if matches!(strategy, SearchStrategy::Sparse | SearchStrategy::Hybrid { .. }) {
                if let Some(schema) = schemas.get(&s.collection) {
                    let has_sparse_repr = schema.representations.iter().any(|r| r.sparse);
                    if !has_sparse_repr {
                        return Err(EngineError::SparseVectorRequired(s.collection.clone()));
                    }
                }
            }

            Ok(Plan::Search(SearchPlan {
                collection: s.collection.clone(),
                dense_vector: s.dense_vector.clone(),
                sparse_vector: s.sparse_vector.clone(),
                strategy,
                pre_filter,
                k: s.limit.unwrap_or(10),
                confidence_threshold: s.confidence.unwrap_or(0.0),
            }))
        }

        Statement::CreateEdgeType(s) => Ok(Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: s.name.clone(),
            from_collection: s.from_collection.clone(),
            to_collection: s.to_collection.clone(),
        })),

        Statement::InsertEdge(s) => Ok(Plan::InsertEdge(InsertEdgePlan {
            edge_type: s.edge_type.clone(),
            from_id: s.from_id.clone(),
            to_id: s.to_id.clone(),
            metadata: s.metadata.clone(),
        })),

        Statement::DeleteEdge(s) => Ok(Plan::DeleteEdge(DeleteEdgePlan {
            edge_type: s.edge_type.clone(),
            from_id: s.from_id.clone(),
            to_id: s.to_id.clone(),
        })),

        Statement::Traverse(s) => Ok(Plan::Traverse(TraversePlan {
            edge_type: s.edge_type.clone(),
            from_id: s.from_id.clone(),
            depth: s.depth,
            limit: s.limit,
        })),

        Statement::Explain(inner) => {
            let inner_plan = plan(inner, schemas)?;
            Ok(Plan::Explain(Box::new(inner_plan)))
        }
    }
}

fn select_fetch_strategy(
    collection: &str,
    filter: &Option<WhereClause>,
    schemas: &DashMap<String, CollectionSchema>,
) -> FetchStrategy {
    if let Some(WhereClause::Eq(field, _)) = filter {
        if let Some(schema) = schemas.get(collection) {
            for idx in &schema.indexes {
                // Leading-column match: compound indexes are usable when WHERE matches
                // the first field. Full compound WHERE support deferred to future phase.
                if idx.fields.first().map(|f| f.as_str()) == Some(field.as_str()) {
                    return FetchStrategy::FieldIndexLookup { index_name: idx.name.clone() };
                }
            }
        }
    }
    FetchStrategy::FullScan
}

fn select_search_strategy(has_dense: bool, has_sparse: bool) -> SearchStrategy {
    match (has_dense, has_sparse) {
        (true, true) => SearchStrategy::Hybrid { rrf_k: crate::hybrid::default_rrf_k() },
        (true, false) => SearchStrategy::Hnsw,
        (false, true) => SearchStrategy::Sparse,
        (false, false) => unreachable!("parser enforces at least one NEAR clause"),
    }
}

fn select_pre_filter(
    collection: &str,
    filter: &Option<WhereClause>,
    schemas: &DashMap<String, CollectionSchema>,
) -> Result<Option<PreFilter>, EngineError> {
    if let Some(clause) = filter {
        if let WhereClause::Eq(field, _) = clause {
            if let Some(schema) = schemas.get(collection) {
                for idx in &schema.indexes {
                    if idx.fields.first().map(|f| f.as_str()) == Some(field.as_str()) {
                        return Ok(Some(PreFilter {
                            index_name: idx.name.clone(),
                            condition: clause.clone(),
                        }));
                    }
                }
                // SEARCH WHERE references a field with no matching index — error per spec
                return Err(EngineError::FieldNotIndexed(field.clone()));
            }
        }
    }
    Ok(None)
}
```

Update planner tests to pass `schemas` parameter (use empty DashMap for existing tests).

- [ ] **Step 4: Stub-update executor.rs to compile**

Update `crates/trondb-core/src/executor.rs` to compile with new types:

1. Add new fields to Executor struct:
```rust
pub struct Executor {
    store: FjallStore,
    wal: WalWriter,
    location: Arc<LocationTable>,
    indexes: DashMap<String, HnswIndex>,
    sparse_indexes: DashMap<String, crate::sparse_index::SparseIndex>,
    field_indexes: DashMap<String, crate::field_index::FieldIndex>,
    adjacency: AdjacencyIndex,
    edge_types: DashMap<String, EdgeType>,
    schemas: DashMap<String, crate::types::CollectionSchema>,
}
```

2. Update `new()` to initialize new fields with empty DashMaps
3. Add accessor: `pub fn schemas(&self) -> &DashMap<String, CollectionSchema>`

4. Stub the `execute()` match arms. Use these patterns for each:

```rust
// CreateCollection — stub (Task 9 implements fully)
Plan::CreateCollection(p) => {
    // Build a minimal CollectionSchema from plan
    let schema = CollectionSchema {
        name: p.name.clone(),
        representations: p.representations.iter().map(|r| StoredRepresentation {
            name: r.name.clone(),
            model: r.model.clone(),
            dimensions: r.dimensions,
            metric: if r.metric == trondb_tql::Metric::InnerProduct {
                Metric::InnerProduct
            } else {
                Metric::Cosine
            },
            sparse: r.sparse,
        }).collect(),
        fields: p.fields.iter().map(|f| StoredField {
            name: f.name.clone(),
            field_type: convert_field_type(&f.field_type),
        }).collect(),
        indexes: p.indexes.iter().map(|i| StoredIndex {
            name: i.name.clone(),
            fields: i.fields.clone(),
            partial_condition: None,
        }).collect(),
    };
    self.store.create_collection_schema(&schema)?;
    self.schemas.insert(schema.name.clone(), schema);
    Ok(QueryResult { rows: vec![], stats: QueryStats { mode: QueryMode::Deterministic } })
}

// Insert — stub (Task 9 implements fully)
Plan::Insert(p) => {
    let id = /* extract from fields/values as before */;
    let mut representations = Vec::new();
    // TODO(Task 9): match vectors to schema representations, update indexes
    for (repr_name, vec_lit) in &p.vectors {
        let vector = match vec_lit {
            VectorLiteral::Dense(v) => VectorData::Dense(v.iter().map(|x| *x as f32).collect()),
            VectorLiteral::Sparse(s) => VectorData::Sparse(s.clone()),
        };
        representations.push(Representation {
            name: repr_name.clone(),
            repr_type: ReprType::Original,
            fields: p.fields.clone(),
            vector,
            recipe_hash: [0u8; 32],
            state: ReprState::Clean,
        });
    }
    let entity = Entity { id: LogicalId::from_string(&id), representations, fields };
    self.store.insert(&p.collection, entity)?;
    Ok(QueryResult { rows: vec![], stats: QueryStats { mode: QueryMode::Deterministic } })
}

// Search — stub (Task 10 implements fully)
Plan::Search(p) => {
    match &p.strategy {
        SearchStrategy::Hnsw => {
            // Use existing HNSW search pattern with p.dense_vector
            let query_vector = p.dense_vector.as_ref()
                .ok_or_else(|| EngineError::InvalidQuery("HNSW strategy requires dense vector".into()))?;
            // ... existing HNSW search code, unchanged ...
        }
        SearchStrategy::Sparse | SearchStrategy::Hybrid { .. } => {
            // TODO(Task 10): implement sparse and hybrid search
            return Err(EngineError::UnsupportedOperation("sparse/hybrid SEARCH not yet wired".into()));
        }
    }
}

// Fetch — keep existing logic, map new strategy field
Plan::Fetch(p) => {
    // Existing FullScan logic works for both strategies at this point
    // TODO(Task 10): implement FieldIndexLookup strategy
    // ... existing FETCH code, unchanged ...
}
```

5. Update `explain_plan()` — stub new plan types:
```rust
Plan::CreateCollection(p) => {
    props.push(("operation", "CreateCollection".into()));
    props.push(("collection", p.name.clone()));
    props.push(("representations", p.representations.len().to_string()));
    props.push(("fields", p.fields.len().to_string()));
    props.push(("indexes", p.indexes.len().to_string()));
}
Plan::Search(p) => {
    props.push(("operation", "Search".into()));
    props.push(("collection", p.collection.clone()));
    props.push(("strategy", format!("{:?}", p.strategy)));
    props.push(("k", p.k.to_string()));
    if p.pre_filter.is_some() {
        props.push(("pre_filter", "ScalarPreFilter".into()));
    }
}
```

6. Add conversion helper:
```rust
fn convert_field_type(ft: &trondb_tql::FieldType) -> crate::types::FieldType {
    match ft {
        trondb_tql::FieldType::Text => crate::types::FieldType::Text,
        trondb_tql::FieldType::DateTime => crate::types::FieldType::DateTime,
        trondb_tql::FieldType::Bool => crate::types::FieldType::Bool,
        trondb_tql::FieldType::Int => crate::types::FieldType::Int,
        trondb_tql::FieldType::Float => crate::types::FieldType::Float,
        trondb_tql::FieldType::EntityRef(s) => crate::types::FieldType::EntityRef(s.clone()),
    }
}
```

7. Migrate store tests. Example pattern:
```rust
#[test]
fn create_and_list_collections() {
    let store = open_test_store();
    let schema = CollectionSchema {
        name: "test_coll".into(),
        representations: vec![StoredRepresentation {
            name: "identity".into(), model: None, dimensions: Some(3),
            metric: Metric::Cosine, sparse: false,
        }],
        fields: vec![],
        indexes: vec![],
    };
    store.create_collection_schema(&schema).unwrap();
    let schemas = store.list_collection_schemas();
    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, "test_coll");
}
```

8. Remove `get_collection_dimensions()` public method (delegates to removed `store.get_dimensions()`). Callers use `schemas.get(name)` instead.

9. Fix `replay_wal_records` for `SchemaCreateColl`:
```rust
RecordType::SchemaCreateColl => {
    let schema: CollectionSchema = rmp_serde::from_slice(&record.payload)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    self.store.create_collection_schema(&schema)?;
    self.schemas.insert(schema.name.clone(), schema);
}
```

- [ ] **Step 5: Fix lib.rs compilation**

Update `crates/trondb-core/src/lib.rs`:
1. Change `planner::plan(&stmt)` to `planner::plan(&stmt, &self.executor.schemas())` in `execute_tql()`
2. Fix HNSW rebuild to use `schema.representations` instead of `get_dimensions`
3. Fix store tests to use `create_collection_schema` instead of `create_collection`
4. Fix integration tests to use new CREATE COLLECTION syntax

- [ ] **Step 6: Run workspace tests**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 7: Commit**

```bash
git add crates/trondb-core/src/types.rs crates/trondb-core/src/store.rs crates/trondb-core/src/planner.rs crates/trondb-core/src/executor.rs crates/trondb-core/src/lib.rs
git commit -m "feat: migrate core types to Phase 5a schema — CollectionSchema, strategies, expanded planner"
```

---

## Chunk 3: Executor + Startup + Integration

### Task 9: Executor — CREATE COLLECTION + INSERT

**Files:**
- Modify: `crates/trondb-core/src/executor.rs`

Wire the full CREATE COLLECTION path (schema storage, HNSW/SparseIndex/FieldIndex instantiation) and the full INSERT path (named representations, field index updates, sparse index updates).

**Note:** Entity deletion (`ENTITY_DELETE`) is not yet implemented. When it is added, it must remove entries from all SparseIndexes and FieldIndexes. This is tracked for a future phase.

- [ ] **Step 1: Implement CREATE COLLECTION**

Replace the Task 8 stub for `Plan::CreateCollection`:

```rust
Plan::CreateCollection(p) => {
    // 1. Validate no duplicates
    let mut repr_names = std::collections::HashSet::new();
    for r in &p.representations {
        if !repr_names.insert(&r.name) {
            return Err(EngineError::DuplicateRepresentation(r.name.clone()));
        }
    }
    let mut field_names = std::collections::HashSet::new();
    for f in &p.fields {
        if !field_names.insert(&f.name) {
            return Err(EngineError::DuplicateField(f.name.clone()));
        }
    }
    let mut index_names = std::collections::HashSet::new();
    for i in &p.indexes {
        if !index_names.insert(&i.name) {
            return Err(EngineError::DuplicateIndex(i.name.clone()));
        }
    }

    // 2. Build CollectionSchema
    let schema = CollectionSchema {
        name: p.name.clone(),
        representations: p.representations.iter().map(|r| StoredRepresentation {
            name: r.name.clone(),
            model: r.model.clone(),
            dimensions: r.dimensions,
            metric: if r.metric == trondb_tql::Metric::InnerProduct {
                Metric::InnerProduct
            } else {
                Metric::Cosine
            },
            sparse: r.sparse,
        }).collect(),
        fields: p.fields.iter().map(|f| StoredField {
            name: f.name.clone(),
            field_type: convert_field_type(&f.field_type),
        }).collect(),
        indexes: p.indexes.iter().map(|i| StoredIndex {
            name: i.name.clone(),
            fields: i.fields.clone(),
            partial_condition: None,
        }).collect(),
    };

    // 3. WAL
    let schema_bytes = rmp_serde::to_vec_named(&schema)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    self.wal.begin_tx().await?;
    self.wal.append(RecordType::SchemaCreateColl, &schema_bytes).await?;
    self.wal.commit_tx().await?;

    // 4. Store in Fjall
    self.store.create_collection_schema(&schema)?;

    // 5. Instantiate dense HNSW indexes
    for repr in &schema.representations {
        if !repr.sparse {
            if let Some(dims) = repr.dimensions {
                let hnsw_key = format!("{}:{}", schema.name, repr.name);
                let hnsw = HnswIndex::new(dims);
                self.indexes.insert(hnsw_key, hnsw);
            }
        }
    }

    // 6. Instantiate sparse indexes
    for repr in &schema.representations {
        if repr.sparse {
            let sparse_key = format!("{}:{}", schema.name, repr.name);
            self.sparse_indexes.insert(sparse_key, SparseIndex::new());
        }
    }

    // 7. Instantiate field indexes
    for idx in &schema.indexes {
        let partition = self.store.open_field_index_partition(&schema.name, &idx.name)?;
        let field_types: Vec<(String, crate::types::FieldType)> = idx.fields.iter()
            .filter_map(|f| {
                schema.fields.iter()
                    .find(|sf| sf.name == *f)
                    .map(|sf| (sf.name.clone(), sf.field_type.clone()))
            })
            .collect();
        let fidx_key = format!("{}:{}", schema.name, idx.name);
        self.field_indexes.insert(fidx_key, FieldIndex::new(partition, field_types));
    }

    // 8. Register schema
    self.schemas.insert(schema.name.clone(), schema);

    Ok(QueryResult { rows: vec![], stats: QueryStats { mode: QueryMode::Deterministic } })
}
```

- [ ] **Step 2: Implement INSERT with representations + indexes**

Replace the Task 8 stub for `Plan::Insert`:

```rust
Plan::Insert(p) => {
    // 1. Look up schema
    let schema = self.schemas.get(&p.collection)
        .ok_or_else(|| EngineError::CollectionNotFound(p.collection.clone()))?
        .clone();

    // 2. Extract entity ID from fields/values
    let id_idx = p.fields.iter().position(|f| f == "id")
        .ok_or_else(|| EngineError::InvalidQuery("INSERT requires 'id' field".into()))?;
    let id_str = match &p.values[id_idx] {
        Literal::String(s) => s.clone(),
        _ => return Err(EngineError::InvalidQuery("id must be a string".into())),
    };
    let entity_id = LogicalId::from_string(&id_str);

    // 3. Check for existing entity and remove old index entries if updating
    if let Ok(old_entity) = self.store.get_entity(&p.collection, &entity_id) {
        // Remove old sparse index entries
        for repr in &old_entity.representations {
            if let VectorData::Sparse(ref sv) = repr.vector {
                let sparse_key = format!("{}:{}", p.collection, repr.name);
                if let Some(sidx) = self.sparse_indexes.get(&sparse_key) {
                    sidx.remove(&entity_id, sv);
                }
            }
        }
        // Remove old field index entries
        for (fidx_key, fidx) in self.field_indexes.iter() {
            if fidx_key.starts_with(&format!("{}:", p.collection)) {
                let old_values: Vec<Value> = fidx.field_types().iter()
                    .filter_map(|(fname, _)| old_entity.fields.get(fname).cloned())
                    .collect();
                if old_values.len() == fidx.field_types().len() {
                    let _ = fidx.remove(&entity_id, &old_values);
                }
            }
        }
    }

    // 4. Build representations from vectors
    let mut representations = Vec::new();
    for (repr_name, vec_lit) in &p.vectors {
        // Match to schema representation
        let schema_repr = schema.representations.iter()
            .find(|r| r.name == *repr_name)
            .ok_or_else(|| EngineError::InvalidQuery(
                format!("no representation '{}' in collection '{}'", repr_name, p.collection)
            ))?;

        let vector = match vec_lit {
            VectorLiteral::Dense(v) => {
                // Validate dimensions
                if let Some(expected_dims) = schema_repr.dimensions {
                    if v.len() != expected_dims {
                        return Err(EngineError::InvalidQuery(format!(
                            "representation '{}' expects {} dimensions, got {}",
                            repr_name, expected_dims, v.len()
                        )));
                    }
                }
                VectorData::Dense(v.iter().map(|x| *x as f32).collect())
            }
            VectorLiteral::Sparse(s) => VectorData::Sparse(s.clone()),
        };
        representations.push(Representation {
            name: repr_name.clone(),
            repr_type: ReprType::Original,
            fields: p.fields.clone(),
            vector,
            recipe_hash: [0u8; 32],
            state: ReprState::Clean,
        });
    }

    // 5. Build entity field map
    let mut fields = HashMap::new();
    for (i, fname) in p.fields.iter().enumerate() {
        if let Some(lit) = p.values.get(i) {
            let value = literal_to_value(lit);
            fields.insert(fname.clone(), value);
        }
    }
    let entity = Entity { id: entity_id.clone(), representations: representations.clone(), fields: fields.clone() };

    // 6. WAL + Fjall insert
    let entity_bytes = rmp_serde::to_vec_named(&entity)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    self.wal.begin_tx().await?;
    self.wal.append(RecordType::EntityInsert, &entity_bytes).await?;
    self.wal.commit_tx().await?;
    self.store.insert(&p.collection, entity)?;

    // 7. Update HNSW indexes
    for repr in &representations {
        if let VectorData::Dense(ref dv) = repr.vector {
            let hnsw_key = format!("{}:{}", p.collection, repr.name);
            if let Some(hnsw) = self.indexes.get(&hnsw_key) {
                hnsw.insert(&entity_id, dv)?;
            }
        }
    }

    // 8. Update SparseIndexes
    for repr in &representations {
        if let VectorData::Sparse(ref sv) = repr.vector {
            let sparse_key = format!("{}:{}", p.collection, repr.name);
            if let Some(sidx) = self.sparse_indexes.get(&sparse_key) {
                sidx.insert(&entity_id, sv);
            }
        }
    }

    // 9. Update FieldIndexes
    for (fidx_key, fidx) in self.field_indexes.iter() {
        if fidx_key.starts_with(&format!("{}:", p.collection)) {
            let values: Vec<Value> = fidx.field_types().iter()
                .filter_map(|(fname, _)| fields.get(fname).cloned())
                .collect();
            if values.len() == fidx.field_types().len() {
                fidx.insert(&entity_id, &values)?;
            }
        }
    }

    // 10. Location table update (existing pattern)
    self.location.set_descriptor(&entity_id, /* ... existing pattern ... */);

    Ok(QueryResult { rows: vec![], stats: QueryStats { mode: QueryMode::Deterministic } })
}
```

- [ ] **Step 3: Write executor tests for CREATE + INSERT**

```rust
#[tokio::test]
async fn create_collection_registers_indexes() {
    let (mut executor, _dir) = test_executor().await;
    let result = executor.execute(Plan::CreateCollection(CreateCollectionPlan {
        name: "test".into(),
        representations: vec![
            RepresentationDecl { name: "dense".into(), model: None, dimensions: Some(3), metric: Metric::Cosine, sparse: false },
            RepresentationDecl { name: "sparse".into(), model: None, dimensions: None, metric: Metric::InnerProduct, sparse: true },
        ],
        fields: vec![FieldDecl { name: "status".into(), field_type: trondb_tql::FieldType::Text }],
        indexes: vec![IndexDecl { name: "idx_status".into(), fields: vec!["status".into()], partial_condition: None }],
    })).await.unwrap();
    assert!(executor.indexes.contains_key("test:dense"));
    assert!(executor.sparse_indexes.contains_key("test:sparse"));
    assert!(executor.field_indexes.contains_key("test:idx_status"));
    assert!(executor.schemas.contains_key("test"));
}

#[tokio::test]
async fn create_collection_duplicate_repr_fails() {
    let (mut executor, _dir) = test_executor().await;
    let result = executor.execute(Plan::CreateCollection(CreateCollectionPlan {
        name: "test".into(),
        representations: vec![
            RepresentationDecl { name: "dup".into(), model: None, dimensions: Some(3), metric: Metric::Cosine, sparse: false },
            RepresentationDecl { name: "dup".into(), model: None, dimensions: Some(3), metric: Metric::Cosine, sparse: false },
        ],
        fields: vec![], indexes: vec![],
    })).await;
    assert!(matches!(result, Err(EngineError::DuplicateRepresentation(_))));
}

#[tokio::test]
async fn create_collection_duplicate_field_fails() {
    let (mut executor, _dir) = test_executor().await;
    let result = executor.execute(Plan::CreateCollection(CreateCollectionPlan {
        name: "test".into(),
        representations: vec![],
        fields: vec![
            FieldDecl { name: "status".into(), field_type: trondb_tql::FieldType::Text },
            FieldDecl { name: "status".into(), field_type: trondb_tql::FieldType::Text },
        ],
        indexes: vec![],
    })).await;
    assert!(matches!(result, Err(EngineError::DuplicateField(_))));
}

#[tokio::test]
async fn insert_updates_hnsw_and_sparse_indexes() {
    let (mut executor, _dir) = test_executor().await;
    // Create collection with both dense and sparse
    // ... (setup) ...
    // Insert entity
    // ... (insert) ...
    // Verify HNSW has the entity
    let hnsw = executor.indexes.get("test:dense").unwrap();
    assert_eq!(hnsw.len(), 1);
    // Verify SparseIndex has the entity
    let sparse = executor.sparse_indexes.get("test:sparse").unwrap();
    assert!(!sparse.is_empty());
}

#[tokio::test]
async fn insert_updates_field_index() {
    let (mut executor, _dir) = test_executor().await;
    // Create collection with field index
    // Insert entity with matching field values
    // Verify FieldIndex.lookup_eq returns the entity ID
    let fidx = executor.field_indexes.get("test:idx_status").unwrap();
    let results = fidx.lookup_eq(&[Value::String("active".into())]).unwrap();
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn insert_update_removes_old_index_entries() {
    let (mut executor, _dir) = test_executor().await;
    // Create collection with field index
    // Insert entity with status="active"
    // Re-insert same entity with status="inactive"
    // Verify old index entry removed, new one present
    let fidx = executor.field_indexes.get("test:idx_status").unwrap();
    let active = fidx.lookup_eq(&[Value::String("active".into())]).unwrap();
    assert!(active.is_empty()); // old entry removed
    let inactive = fidx.lookup_eq(&[Value::String("inactive".into())]).unwrap();
    assert_eq!(inactive.len(), 1);
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace`

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "feat: wire CREATE COLLECTION + INSERT — schema storage, HNSW/sparse/field index updates"
```

---

### Task 10: Executor — FETCH + SEARCH + EXPLAIN

**Files:**
- Modify: `crates/trondb-core/src/executor.rs`

**Design note — ScalarPreFilter:** The pre-filter is a post-filter strategy: run the full HNSW/sparse search, then discard results not in the candidate set. To compensate for potential result loss, over-fetch by multiplying `k * 4`, then trim to `k` after filtering. True HNSW candidate restriction would require changes to `hnsw_rs` internals.

**Design note — Confidence threshold:** Confidence threshold applies only to the `Hnsw` strategy (cosine similarity). For `Sparse` and `Hybrid` strategies, confidence threshold is skipped — RRF scores are relative ranking scores, not comparable to cosine similarity.

**Design note — Range/prefix FETCH:** The planner's `select_fetch_strategy` currently only handles `WhereClause::Eq`. Range and prefix FETCH via FieldIndex is deferred to a future phase. These queries fall back to FullScan.

- [ ] **Step 1: Implement FETCH with FieldIndex strategy**

Replace the FETCH path in `Plan::Fetch`:

```rust
Plan::Fetch(p) => {
    match &p.strategy {
        FetchStrategy::FieldIndexLookup { index_name } => {
            let fidx_key = format!("{}:{}", p.collection, index_name);
            let fidx = self.field_indexes.get(&fidx_key)
                .ok_or_else(|| EngineError::InvalidQuery(format!("field index '{}' not found", index_name)))?;

            // Extract lookup value from WHERE clause
            let entity_ids = if let Some(WhereClause::Eq(_, lit)) = &p.filter {
                let value = literal_to_value(lit);
                fidx.lookup_eq(&[value])?
            } else {
                return Err(EngineError::InvalidQuery("FieldIndexLookup requires Eq filter".into()));
            };

            // Resolve entities from Fjall
            let mut rows = Vec::new();
            for eid in &entity_ids {
                if let Ok(entity) = self.store.get_entity(&p.collection, eid) {
                    let row = entity_to_row(&entity, &p.fields);
                    rows.push(row);
                }
            }

            if let Some(limit) = p.limit {
                rows.truncate(limit);
            }

            Ok(QueryResult {
                rows,
                stats: QueryStats { mode: QueryMode::Deterministic },
            })
        }
        FetchStrategy::FullScan => {
            // Existing FETCH logic — scan partition, apply WHERE filter
            // ... (keep existing code, no changes needed) ...
        }
    }
}
```

- [ ] **Step 2: Implement SEARCH with all strategies**

Replace the SEARCH path in `Plan::Search`:

```rust
Plan::Search(p) => {
    // Get candidate IDs from pre-filter if present
    let pre_filter_ids: Option<std::collections::HashSet<LogicalId>> = if let Some(pf) = &p.pre_filter {
        let fidx_key = format!("{}:{}", p.collection, pf.index_name);
        let fidx = self.field_indexes.get(&fidx_key)
            .ok_or_else(|| EngineError::InvalidQuery(format!("pre-filter index '{}' not found", pf.index_name)))?;
        let value = match &pf.condition {
            WhereClause::Eq(_, lit) => literal_to_value(lit),
            _ => return Err(EngineError::InvalidQuery("pre-filter only supports Eq".into())),
        };
        let ids = fidx.lookup_eq(&[value])?;
        Some(ids.into_iter().collect())
    } else {
        None
    };

    // Over-fetch when pre-filtering to compensate for post-filter loss
    let fetch_k = if pre_filter_ids.is_some() { p.k * 4 } else { p.k };

    let results: Vec<(LogicalId, f32)> = match &p.strategy {
        SearchStrategy::Hnsw => {
            let query = p.dense_vector.as_ref()
                .ok_or_else(|| EngineError::InvalidQuery("HNSW strategy requires dense vector".into()))?;
            // Find the first dense HNSW index for this collection
            let hnsw_key = self.indexes.iter()
                .find(|entry| entry.key().starts_with(&format!("{}:", p.collection)))
                .map(|entry| entry.key().clone())
                .ok_or_else(|| EngineError::InvalidQuery("no HNSW index for collection".into()))?;
            let hnsw = self.indexes.get(&hnsw_key).unwrap();
            let query_f32: Vec<f32> = query.iter().map(|x| *x as f32).collect();
            let mut raw = hnsw.search(&query_f32, fetch_k)?;

            // Apply confidence threshold (only for HNSW — cosine similarity is meaningful)
            if p.confidence_threshold > 0.0 {
                raw.retain(|(_, score)| *score as f64 >= p.confidence_threshold);
            }
            raw
        }

        SearchStrategy::Sparse => {
            let query = p.sparse_vector.as_ref()
                .ok_or_else(|| EngineError::InvalidQuery("Sparse strategy requires sparse vector".into()))?;
            let sparse_key = self.sparse_indexes.iter()
                .find(|entry| entry.key().starts_with(&format!("{}:", p.collection)))
                .map(|entry| entry.key().clone())
                .ok_or_else(|| EngineError::InvalidQuery("no SparseIndex for collection".into()))?;
            let sidx = self.sparse_indexes.get(&sparse_key).unwrap();
            // No confidence threshold for sparse — scores are dot products, not cosine similarity
            sidx.search(query, fetch_k)
        }

        SearchStrategy::Hybrid { rrf_k } => {
            let dense_query = p.dense_vector.as_ref()
                .ok_or_else(|| EngineError::InvalidQuery("Hybrid strategy requires dense vector".into()))?;
            let sparse_query = p.sparse_vector.as_ref()
                .ok_or_else(|| EngineError::InvalidQuery("Hybrid strategy requires sparse vector".into()))?;

            // Dense search
            let hnsw_key = self.indexes.iter()
                .find(|entry| entry.key().starts_with(&format!("{}:", p.collection)))
                .map(|entry| entry.key().clone())
                .ok_or_else(|| EngineError::InvalidQuery("no HNSW index for collection".into()))?;
            let hnsw = self.indexes.get(&hnsw_key).unwrap();
            let query_f32: Vec<f32> = dense_query.iter().map(|x| *x as f32).collect();
            let dense_results = hnsw.search(&query_f32, fetch_k)?;

            // Sparse search
            let sparse_key = self.sparse_indexes.iter()
                .find(|entry| entry.key().starts_with(&format!("{}:", p.collection)))
                .map(|entry| entry.key().clone())
                .ok_or_else(|| EngineError::InvalidQuery("no SparseIndex for collection".into()))?;
            let sidx = self.sparse_indexes.get(&sparse_key).unwrap();
            let sparse_results = sidx.search(sparse_query, fetch_k);

            // RRF merge — no confidence threshold (RRF scores are relative ranking)
            crate::hybrid::merge_rrf(&dense_results, &sparse_results, *rrf_k)
        }
    };

    // Apply pre-filter (post-filter on results)
    let filtered = if let Some(ref allowed_ids) = pre_filter_ids {
        results.into_iter()
            .filter(|(id, _)| allowed_ids.contains(id))
            .collect::<Vec<_>>()
    } else {
        results
    };

    // Trim to k after filtering
    let final_results: Vec<(LogicalId, f32)> = filtered.into_iter().take(p.k).collect();

    // Resolve entities
    let mut rows = Vec::new();
    for (id, score) in &final_results {
        if let Ok(entity) = self.store.get_entity(&p.collection, id) {
            let mut row = entity_to_row(&entity, &FieldList::All);
            row.score = Some(*score);
            rows.push(row);
        }
    }

    Ok(QueryResult {
        rows,
        stats: QueryStats { mode: QueryMode::Probabilistic },
    })
}
```

- [ ] **Step 3: Update EXPLAIN output**

Update `explain_plan()` for new plan types:

```rust
Plan::CreateCollection(p) => {
    props.push(("operation", "CreateCollection".into()));
    props.push(("collection", p.name.clone()));
    props.push(("representations", p.representations.len().to_string()));
    props.push(("fields", p.fields.len().to_string()));
    props.push(("indexes", p.indexes.len().to_string()));
}
Plan::Fetch(p) => {
    props.push(("operation", "Fetch".into()));
    props.push(("collection", p.collection.clone()));
    match &p.strategy {
        FetchStrategy::FullScan => props.push(("strategy", "FullScan".into())),
        FetchStrategy::FieldIndexLookup { index_name } =>
            props.push(("strategy", format!("FieldIndexLookup ({})", index_name))),
    }
}
Plan::Search(p) => {
    props.push(("operation", "Search".into()));
    props.push(("collection", p.collection.clone()));
    match &p.strategy {
        SearchStrategy::Hnsw => {
            props.push(("strategy", "HnswSearch".into()));
            props.push(("dense", "HNSW".into()));
        }
        SearchStrategy::Sparse => {
            props.push(("strategy", "SparseSearch".into()));
            props.push(("sparse", "SparseIndex".into()));
        }
        SearchStrategy::Hybrid { rrf_k } => {
            props.push(("strategy", "HybridSearch".into()));
            props.push(("dense", "HNSW".into()));
            props.push(("sparse", "SparseIndex".into()));
            props.push(("merge", format!("RRF (k={})", rrf_k)));
        }
    }
    props.push(("k", p.k.to_string()));
    if let Some(pf) = &p.pre_filter {
        props.push(("pre_filter", format!("ScalarPreFilter ({})", pf.index_name)));
    }
}
```

- [ ] **Step 4: Write tests**

```rust
#[tokio::test]
async fn fetch_via_field_index() {
    let (mut executor, _dir) = test_executor().await;
    // Create collection with field index on "city"
    // Insert two entities: city=London, city=Paris
    // FETCH WHERE city = 'London' (planner selects FieldIndexLookup)
    // Verify only London entity returned
    let result = executor.execute(Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
        limit: None,
        strategy: FetchStrategy::FieldIndexLookup { index_name: "idx_city".into() },
    })).await.unwrap();
    assert_eq!(result.rows.len(), 1);
}

#[tokio::test]
async fn search_sparse_returns_ranked() {
    let (mut executor, _dir) = test_executor().await;
    // Create collection with sparse representation
    // Insert 3 entities with different sparse vectors
    // SEARCH NEAR SPARSE [1:1.0] LIMIT 2
    let result = executor.execute(Plan::Search(SearchPlan {
        collection: "docs".into(),
        dense_vector: None,
        sparse_vector: Some(vec![(1, 1.0)]),
        strategy: SearchStrategy::Sparse,
        pre_filter: None,
        k: 2,
        confidence_threshold: 0.0,
    })).await.unwrap();
    assert_eq!(result.rows.len(), 2);
    // Verify descending score order
    assert!(result.rows[0].score.unwrap() >= result.rows[1].score.unwrap());
}

#[tokio::test]
async fn search_hybrid_merges_dense_sparse() {
    let (mut executor, _dir) = test_executor().await;
    // Create collection with both dense and sparse representations
    // Insert entities with both vector types
    // SEARCH NEAR VECTOR [...] NEAR SPARSE [...]
    let result = executor.execute(Plan::Search(SearchPlan {
        collection: "docs".into(),
        dense_vector: Some(vec![0.1, 0.2, 0.3]),
        sparse_vector: Some(vec![(1, 0.8), (2, 0.5)]),
        strategy: SearchStrategy::Hybrid { rrf_k: 60 },
        pre_filter: None,
        k: 5,
        confidence_threshold: 0.0, // not applied for hybrid
    })).await.unwrap();
    assert!(!result.rows.is_empty());
}

#[tokio::test]
async fn search_with_prefilter_narrows_results() {
    let (mut executor, _dir) = test_executor().await;
    // Create collection with dense repr + field index on "city"
    // Insert: entity1 (city=London), entity2 (city=Paris), entity3 (city=London)
    // SEARCH WHERE city = 'London' NEAR VECTOR [...]
    // Should only return London entities
    let result = executor.execute(Plan::Search(SearchPlan {
        collection: "venues".into(),
        dense_vector: Some(vec![0.1, 0.2, 0.3]),
        sparse_vector: None,
        strategy: SearchStrategy::Hnsw,
        pre_filter: Some(PreFilter {
            index_name: "idx_city".into(),
            condition: WhereClause::Eq("city".into(), Literal::String("London".into())),
        }),
        k: 10,
        confidence_threshold: 0.0,
    })).await.unwrap();
    // All results should be London
    for row in &result.rows {
        let city = row.fields.get("city").unwrap();
        assert_eq!(city, &Value::String("London".into()));
    }
}

#[tokio::test]
async fn explain_shows_strategy() {
    let (mut executor, _dir) = test_executor().await;
    // EXPLAIN SEARCH with Hybrid strategy
    let plan = Plan::Explain(Box::new(Plan::Search(SearchPlan {
        collection: "docs".into(),
        dense_vector: Some(vec![0.1]),
        sparse_vector: Some(vec![(1, 0.5)]),
        strategy: SearchStrategy::Hybrid { rrf_k: 60 },
        pre_filter: None,
        k: 10,
        confidence_threshold: 0.0,
    })));
    let result = executor.execute(plan).await.unwrap();
    // Check EXPLAIN output contains strategy info
    let strategy_row = result.rows.iter().find(|r| {
        r.fields.get("property").map(|v| v == &Value::String("strategy".into())).unwrap_or(false)
    });
    assert!(strategy_row.is_some());
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace`

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "feat: wire FETCH FieldIndex, SEARCH sparse/hybrid/prefilter, expanded EXPLAIN"
```

---

### Task 11: Startup Rebuild + WAL Replay

**Files:**
- Modify: `crates/trondb-core/src/executor.rs` (WAL replay)
- Modify: `crates/trondb-core/src/lib.rs` (Engine::open rebuild)

**Breaking WAL change:** The `SchemaCreateColl` record payload changes from `{name, dimensions}` to `CollectionSchema`. Existing WAL segments from before Phase 5a will fail to deserialize. A clean WAL directory is required when upgrading to Phase 5a (`rm -rf trondb_data/wal/`). This is acceptable for a pre-production system.

- [ ] **Step 1: Update WAL replay for new schema format**

In `replay_wal_records`, replace the `SchemaCreateColl` handler:

```rust
RecordType::SchemaCreateColl => {
    // Phase 5a: payload is a full CollectionSchema (not old {name, dimensions})
    let schema: CollectionSchema = rmp_serde::from_slice(&record.payload)
        .map_err(|e| EngineError::Storage(
            format!("failed to deserialize CollectionSchema from WAL: {}. \
                     If upgrading from pre-Phase 5a, delete trondb_data/wal/ and restart.", e)
        ))?;
    self.store.create_collection_schema(&schema)?;
    self.schemas.insert(schema.name.clone(), schema);
}
```

- [ ] **Step 2: Update Engine::open startup rebuild**

Add the following to `Engine::open` in `lib.rs`, after WAL replay and before the existing HNSW rebuild:

```rust
// --- Phase 5a: Load schemas and rebuild indexes ---

// 1. Load all collection schemas from Fjall
let schemas_list = store.list_collection_schemas();
for schema in &schemas_list {
    executor.schemas.insert(schema.name.clone(), schema.clone());
}

// 2. For each schema, rebuild HNSW indexes for dense representations
for schema in &schemas_list {
    for repr in &schema.representations {
        if !repr.sparse {
            if let Some(dims) = repr.dimensions {
                let hnsw_key = format!("{}:{}", schema.name, repr.name);
                let hnsw = HnswIndex::new(dims);

                // Scan all entities in the collection and re-index dense vectors
                if let Ok(entities) = store.scan_all_entities(&schema.name) {
                    for entity in &entities {
                        for entity_repr in &entity.representations {
                            if entity_repr.name == repr.name {
                                if let VectorData::Dense(ref dv) = entity_repr.vector {
                                    let _ = hnsw.insert(&entity.id, dv);
                                }
                            }
                        }
                    }
                }

                executor.indexes.insert(hnsw_key, hnsw);
            }
        }
    }
}

// 3. For each schema, rebuild SparseIndex for sparse representations
for schema in &schemas_list {
    for repr in &schema.representations {
        if repr.sparse {
            let sparse_key = format!("{}:{}", schema.name, repr.name);
            let sparse_idx = SparseIndex::new();

            // Scan all entities and re-index sparse vectors
            if let Ok(entities) = store.scan_all_entities(&schema.name) {
                for entity in &entities {
                    for entity_repr in &entity.representations {
                        if entity_repr.name == repr.name {
                            if let VectorData::Sparse(ref sv) = entity_repr.vector {
                                sparse_idx.insert(&entity.id, sv);
                            }
                        }
                    }
                }
            }

            executor.sparse_indexes.insert(sparse_key, sparse_idx);
        }
    }
}

// 4. For each schema, re-instantiate FieldIndex structs
//    (Fjall partitions survive restart — just need the FieldIndex wrapper)
for schema in &schemas_list {
    for idx_def in &schema.indexes {
        let partition = store.open_field_index_partition(&schema.name, &idx_def.name)?;
        let field_types: Vec<(String, crate::types::FieldType)> = idx_def.fields.iter()
            .filter_map(|f| {
                schema.fields.iter()
                    .find(|sf| sf.name == *f)
                    .map(|sf| (sf.name.clone(), sf.field_type.clone()))
            })
            .collect();
        let fidx_key = format!("{}:{}", schema.name, idx_def.name);
        executor.field_indexes.insert(fidx_key, FieldIndex::new(partition, field_types));
    }
}

// 5. AdjacencyIndex rebuild (existing, unchanged)
```

- [ ] **Step 3: Write restart tests**

```rust
#[tokio::test]
async fn sparse_index_survives_restart() {
    let dir = TempDir::new().unwrap();

    // Phase 1: Create, insert, verify
    {
        let engine = Engine::open(dir.path()).await.unwrap();
        engine.execute_tql("CREATE COLLECTION docs (
            REPRESENTATION sparse_title MODEL 'splade' METRIC INNER_PRODUCT SPARSE true
        );").await.unwrap();
        engine.execute_tql("INSERT INTO docs (id, title) VALUES ('d1', 'Hello')
            REPRESENTATION sparse_title SPARSE [1:0.8, 42:0.5];").await.unwrap();
        // Verify sparse index has entry
        let result = engine.execute_tql("SEARCH docs NEAR SPARSE [1:1.0] LIMIT 5;").await.unwrap();
        assert_eq!(result.rows.len(), 1);
    }

    // Phase 2: Reopen and verify sparse index rebuilt
    {
        let engine = Engine::open(dir.path()).await.unwrap();
        let result = engine.execute_tql("SEARCH docs NEAR SPARSE [1:1.0] LIMIT 5;").await.unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].fields.get("id"), Some(&Value::String("d1".into())));
    }
}

#[tokio::test]
async fn field_index_survives_restart() {
    let dir = TempDir::new().unwrap();

    {
        let engine = Engine::open(dir.path()).await.unwrap();
        engine.execute_tql("CREATE COLLECTION venues (
            REPRESENTATION identity DIMENSIONS 3 METRIC COSINE,
            FIELD city TEXT,
            INDEX idx_city ON (city)
        );").await.unwrap();
        engine.execute_tql("INSERT INTO venues (id, city) VALUES ('v1', 'London')
            REPRESENTATION identity VECTOR [0.1, 0.2, 0.3];").await.unwrap();
    }

    {
        let engine = Engine::open(dir.path()).await.unwrap();
        let result = engine.execute_tql("FETCH * FROM venues WHERE city = 'London';").await.unwrap();
        assert_eq!(result.rows.len(), 1);
    }
}

#[tokio::test]
async fn hybrid_search_works_after_restart() {
    let dir = TempDir::new().unwrap();

    {
        let engine = Engine::open(dir.path()).await.unwrap();
        engine.execute_tql("CREATE COLLECTION docs (
            REPRESENTATION dense DIMENSIONS 3 METRIC COSINE,
            REPRESENTATION sparse MODEL 'splade' METRIC INNER_PRODUCT SPARSE true
        );").await.unwrap();
        engine.execute_tql("INSERT INTO docs (id) VALUES ('d1')
            REPRESENTATION dense VECTOR [0.1, 0.2, 0.3]
            REPRESENTATION sparse SPARSE [1:0.8];").await.unwrap();
    }

    {
        let engine = Engine::open(dir.path()).await.unwrap();
        let result = engine.execute_tql(
            "SEARCH docs NEAR VECTOR [0.1, 0.2, 0.3] NEAR SPARSE [1:1.0] LIMIT 5;"
        ).await.unwrap();
        assert!(!result.rows.is_empty());
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace`

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/executor.rs crates/trondb-core/src/lib.rs
git commit -m "feat: startup rebuild for SparseIndex/FieldIndex/schemas + WAL replay for CollectionSchema"
```

---

### Task 12: Integration Tests + Test Migration + CLAUDE.md

**Files:**
- Modify: `crates/trondb-core/src/lib.rs` (integration tests)
- Modify: `CLAUDE.md`

- [ ] **Step 1: Migrate all existing integration tests to new syntax**

Update every `CREATE COLLECTION venues WITH DIMENSIONS 3;` to:
```sql
CREATE COLLECTION venues (
    REPRESENTATION identity DIMENSIONS 3 METRIC COSINE
);
```

Update every `VECTOR [...]` in INSERT to:
```sql
REPRESENTATION identity VECTOR [...]
```

Ensure all Phase 5 edge tests still pass with new syntax.

- [ ] **Step 2: Add Phase 5a integration tests**

New integration tests in `lib.rs::tests`:
- `create_collection_with_fields_and_indexes` — CREATE COLLECTION with fields + indexes → INSERT → FETCH via field index
- `compound_index_lookup` — two-field compound index prefix lookup
- `partial_index_only_matches` — partial index WHERE condition filters entries
- `sparse_search_returns_ranked` — sparse SEARCH returns correct ranking
- `hybrid_search_merges_dense_sparse` — hybrid SEARCH merges correctly
- `scalar_prefilter_narrows_search` — ScalarPreFilter reduces SEARCH results
- `explain_shows_strategy_for_each_type` — EXPLAIN output for FieldIndexLookup, Hnsw, Sparse, HybridSearch, ScalarPreFilter
- `all_indexes_survive_restart` — field + sparse + HNSW all work after restart
- `h3_field_index_equality` — H3 cell ID lookup via TEXT field index
- `existing_edge_tests_with_new_syntax` — Phase 5 edge tests pass with new schema
- `error_duplicate_representation` — duplicate representation name fails
- `error_duplicate_index` — duplicate index name in CREATE COLLECTION fails
- `error_duplicate_field` — duplicate field name in CREATE COLLECTION fails
- `error_sparse_vector_on_dense_only_collection` — SEARCH NEAR SPARSE on collection with no sparse repr fails (SparseVectorRequired)
- `error_field_not_indexed` — SEARCH WHERE on unindexed field returns FieldNotIndexed error
- `error_invalid_field_type` — WHERE clause value type mismatch returns InvalidFieldType error

- [ ] **Step 3: Update CLAUDE.md**

Update CLAUDE.md header to "Phase 5a: Indexes" and add:
```
- Field Index: Fjall-backed sortable byte encoding, compound/partial indexes, H3 geospatial
  - One Fjall partition per declared index (fidx.{collection}.{index_name})
  - Sortable encoding: Text=UTF-8, Int=sign-bit-flipped BE, Float=IEEE754 manipulated, Bool=0x00/0x01
  - Compound keys: null-byte separated, prefix scan on leading fields
  - Partial indexes: WHERE condition evaluated at write time
- Sparse Vector Index: RAM-resident inverted index (DashMap), SPLADE-style sparse vectors
  - SparseIndex rebuilt from Fjall on startup
  - Inner product scoring, min_weight=0.001 filter
- Hybrid SEARCH: RRF merge (k=60, 1-based ranking) combines dense + sparse results
- ScalarPreFilter: WHERE + SEARCH optimisation via field index narrowing
- Planner: schema-aware strategy selection (Hnsw, Sparse, Hybrid, FieldIndexLookup, ScalarPreFilter)
- CREATE COLLECTION: block syntax with REPRESENTATION, FIELD, INDEX declarations
- INSERT: named representation vectors (REPRESENTATION name VECTOR/SPARSE)
- EXPLAIN: shows strategy, index names, representation names (no ACU costs — deferred)
```

- [ ] **Step 4: Run full test suite**

Run: `cargo test --workspace`
Expected: all tests pass, 0 warnings

- [ ] **Step 5: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: clean

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/src/lib.rs CLAUDE.md
git commit -m "test: add Phase 5a integration tests, migrate existing tests to new syntax, update CLAUDE.md"
```
