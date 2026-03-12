# Phase 10: Pluggable Vectoriser + Mutation Cascade — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give TronDB the ability to generate vectors from entity fields — not just store pre-computed ones — enabling composite vectors, mutation cascade, and natural language SEARCH.

**Architecture:** New `trondb-vectoriser` crate provides implementations of a `Vectoriser` trait defined in `trondb-core`. The trait follows dependency inversion: core defines the contract, the vectoriser crate implements it, binaries wire them together. A `VectoriserRegistry` in the engine maps collection names to their vectoriser instance. INSERT detects managed representations (those with FIELDS configured) and auto-generates vectors. UPDATE triggers dirty detection and background recomputation.

**Tech Stack:** Rust 2021, async-trait, ort (ONNX Runtime, feature-gated), tokenizers (HuggingFace, feature-gated), reqwest (external providers, feature-gated), sha2 (recipe hashing)

**Spec:** `docs/superpowers/specs/2026-03-12-trondb-roadmap-phases-10-15.md` §3

---

## Dependency Graph

```
trondb-core (defines Vectoriser trait + VectoriserRegistry)
    ↑
trondb-vectoriser (implements: Passthrough, OnnxDense, OnnxSparse, Network, External)
    ↑
trondb-cli, trondb-server (wire vectorisers into engine at startup)
```

No circular dependencies. `trondb-core` never imports `trondb-vectoriser`.

## File Structure

### New crate: `crates/trondb-vectoriser/`

| File | Responsibility |
|------|---------------|
| `Cargo.toml` | Crate manifest with `onnx` and `external` feature flags |
| `src/lib.rs` | Module declarations, re-exports |
| `src/passthrough.rs` | PassthroughVectoriser — accepts pre-computed vectors, rejects encode_query |
| `src/mock.rs` | MockVectoriser — deterministic fake for tests (always returns same vector) |
| `src/onnx_dense.rs` | OnnxDenseVectoriser — local ONNX dense model (behind `onnx` feature) |
| `src/onnx_sparse.rs` | OnnxSparseVectoriser — local ONNX sparse model (behind `onnx` feature) |
| `src/network.rs` | NetworkVectoriser — HTTP client to local model cluster |
| `src/external.rs` | ExternalVectoriser — HTTP client to third-party APIs (behind `external` feature) |
| `src/recipe.rs` | recipe_hash computation from field config + model_id |

### New file in `trondb-core`:

| File | Responsibility |
|------|---------------|
| `src/vectoriser.rs` | Vectoriser trait, VectorOutput, VectoriserError, VectoriserRegistry |

### Modified files:

| File | Changes |
|------|---------|
| `crates/trondb-core/src/lib.rs` | Add `pub mod vectoriser;`, Engine holds VectoriserRegistry, mutation cascade background task |
| `crates/trondb-core/src/types.rs` | StoredRepresentation gains `fields: Vec<String>`, CollectionSchema gains `vectoriser_config: Option<VectoriserConfig>` |
| `crates/trondb-core/src/executor.rs` | INSERT auto-vectorise path, UPDATE dirty detection + cascade trigger, SEARCH dirty exclusion, NL SEARCH |
| `crates/trondb-core/src/planner.rs` | SearchPlan gains `query_text` + `using_repr`, NaturalLanguageSearch strategy |
| `crates/trondb-core/Cargo.toml` | Add `async-trait` dependency |
| `crates/trondb-tql/src/token.rs` | New tokens: Using, Fields, ModelPath, Device, Vectoriser (keyword), Endpoint, Auth |
| `crates/trondb-tql/src/ast.rs` | RepresentationDecl gains `fields`, SearchStmt gains `query_text` + `using_repr`, new VectoriserConfigDecl |
| `crates/trondb-tql/src/parser.rs` | Parse FIELDS clause, NEAR 'text', USING, extended CREATE COLLECTION |
| `crates/trondb-cli/Cargo.toml` | Add `trondb-vectoriser` dependency |
| `crates/trondb-cli/src/main.rs` | Wire vectorisers into engine at startup |
| `crates/trondb-server/Cargo.toml` | Add `trondb-vectoriser` dependency |
| `Cargo.toml` (workspace) | Add `trondb-vectoriser` to members, add workspace deps |

---

## Chunk 1: Foundation

### Task 1: Vectoriser Trait + Types in trondb-core

**Files:**
- Create: `crates/trondb-core/src/vectoriser.rs`
- Modify: `crates/trondb-core/src/lib.rs` (add `pub mod vectoriser;`)
- Modify: `crates/trondb-core/Cargo.toml` (add `async-trait`)

**Context:** This task defines the trait that ALL vectoriser implementations must satisfy. It lives in `trondb-core` so the executor can use `dyn Vectoriser` without depending on the implementation crate. The trait must be object-safe (no generics on methods, all returns are concrete or boxed).

- [ ] **Step 1: Add async-trait to trondb-core Cargo.toml**

In `crates/trondb-core/Cargo.toml`, add under `[dependencies]`:
```toml
async-trait.workspace = true
tracing.workspace = true
```

(`tracing` is needed later for cascade logging but add it now to avoid a second Cargo.toml edit.)

- [ ] **Step 2: Write the failing test for Vectoriser trait object safety**

Create `crates/trondb-core/src/vectoriser.rs`:

```rust
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use thiserror::Error;

use crate::types::{Value, VectorData};

// ---------------------------------------------------------------------------
// Vectoriser errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum VectoriserError {
    #[error("encoding failed: {0}")]
    EncodeFailed(String),

    #[error("operation not supported: {0}")]
    NotSupported(String),

    #[error("model not loaded: {0}")]
    ModelNotLoaded(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("auth error: {0}")]
    Auth(String),
}

// ---------------------------------------------------------------------------
// FieldSet — input to encode()
// ---------------------------------------------------------------------------

/// Named field values passed to the vectoriser for encoding.
/// Keys are field names, values are their current string representations.
pub type FieldSet = HashMap<String, String>;

// ---------------------------------------------------------------------------
// Vectoriser trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Vectoriser: Send + Sync {
    /// Unique identifier for this vectoriser instance (e.g. "onnx-dense:bge-small-en-v1.5")
    fn id(&self) -> &str;

    /// Model identifier (e.g. "bge-small-en-v1.5", "splade-v3", "passthrough")
    fn model_id(&self) -> &str;

    /// Output dimensions (dense) or 0 (sparse/passthrough)
    fn output_size(&self) -> usize;

    /// Whether this produces dense or sparse vectors
    fn output_kind(&self) -> VectorKind;

    /// Generate a vector from entity fields
    async fn encode(&self, fields: &FieldSet) -> Result<VectorData, VectoriserError>;

    /// Batch encode for throughput (default: sequential)
    async fn encode_batch(&self, batch: &[FieldSet]) -> Result<Vec<VectorData>, VectoriserError> {
        let mut results = Vec::with_capacity(batch.len());
        for fields in batch {
            results.push(self.encode(fields).await?);
        }
        Ok(results)
    }

    /// Whether MRL (Matryoshka) truncation is supported
    fn supports_mrl(&self) -> bool { false }

    /// Encode a query string (for SEARCH NEAR 'text')
    async fn encode_query(&self, query: &str) -> Result<VectorData, VectoriserError>;

    /// Optional: incremental update (delta encoding). Returns None to signal full recompute needed.
    async fn encode_delta(&self, _prev: &VectorData, _changed_fields: &FieldSet)
        -> Result<Option<VectorData>, VectoriserError> {
        Ok(None) // default: full recompute via encode()
    }
}

// NOTE: The spec (§3.1) defines VectorOutput::Dense(Vec<f64>). We intentionally use the
// existing VectorData::Dense(Vec<f32>) to avoid f64→f32 conversions throughout the engine.
// The entire pipeline (HNSW, quantisation, storage) operates on f32. This is a deliberate
// divergence — do not "fix" it to f64.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorKind {
    Dense,
    Sparse,
}

// ---------------------------------------------------------------------------
// VectoriserRegistry — maps collection:repr to vectoriser instance
// ---------------------------------------------------------------------------

/// Registry mapping "{collection}:{repr_name}" keys to vectoriser instances.
pub struct VectoriserRegistry {
    vectorisers: DashMap<String, Arc<dyn Vectoriser>>,
}

impl VectoriserRegistry {
    pub fn new() -> Self {
        Self {
            vectorisers: DashMap::new(),
        }
    }

    /// Register a vectoriser for a collection's representation.
    pub fn register(&self, collection: &str, repr_name: &str, vectoriser: Arc<dyn Vectoriser>) {
        let key = format!("{collection}:{repr_name}");
        self.vectorisers.insert(key, vectoriser);
    }

    /// Look up the vectoriser for a collection's representation.
    pub fn get(&self, collection: &str, repr_name: &str) -> Option<Arc<dyn Vectoriser>> {
        let key = format!("{collection}:{repr_name}");
        self.vectorisers.get(&key).map(|v| Arc::clone(&v))
    }

    /// Check if a representation has a registered vectoriser.
    pub fn has(&self, collection: &str, repr_name: &str) -> bool {
        let key = format!("{collection}:{repr_name}");
        self.vectorisers.contains_key(&key)
    }

    /// Remove all vectorisers for a collection.
    pub fn remove_collection(&self, collection: &str) {
        let prefix = format!("{collection}:");
        self.vectorisers.retain(|k, _| !k.starts_with(&prefix));
    }
}

impl Default for VectoriserRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal mock to verify the trait is object-safe.
    struct FakeVectoriser;

    #[async_trait]
    impl Vectoriser for FakeVectoriser {
        fn id(&self) -> &str { "fake" }
        fn model_id(&self) -> &str { "fake-model" }
        fn output_size(&self) -> usize { 3 }
        fn output_kind(&self) -> VectorKind { VectorKind::Dense }

        async fn encode(&self, _fields: &FieldSet) -> Result<VectorData, VectoriserError> {
            Ok(VectorData::Dense(vec![0.1, 0.2, 0.3]))
        }

        async fn encode_query(&self, _query: &str) -> Result<VectorData, VectoriserError> {
            Ok(VectorData::Dense(vec![0.1, 0.2, 0.3]))
        }
    }

    #[test]
    fn trait_is_object_safe() {
        // If this compiles, the trait is object-safe
        let _: Box<dyn Vectoriser> = Box::new(FakeVectoriser);
    }

    #[test]
    fn registry_add_and_get() {
        let registry = VectoriserRegistry::new();
        let v: Arc<dyn Vectoriser> = Arc::new(FakeVectoriser);
        registry.register("venues", "identity", v);

        assert!(registry.has("venues", "identity"));
        assert!(!registry.has("venues", "nonexistent"));

        let retrieved = registry.get("venues", "identity").unwrap();
        assert_eq!(retrieved.id(), "fake");
    }

    #[test]
    fn registry_remove_collection() {
        let registry = VectoriserRegistry::new();
        let v: Arc<dyn Vectoriser> = Arc::new(FakeVectoriser);
        registry.register("venues", "identity", Arc::clone(&v));
        registry.register("venues", "sparse", Arc::clone(&v));
        registry.register("events", "identity", v);

        registry.remove_collection("venues");

        assert!(!registry.has("venues", "identity"));
        assert!(!registry.has("venues", "sparse"));
        assert!(registry.has("events", "identity"));
    }

    #[tokio::test]
    async fn encode_returns_vector() {
        let v = FakeVectoriser;
        let fields = FieldSet::from([("name".into(), "Test Venue".into())]);
        let result = v.encode(&fields).await.unwrap();
        match result {
            VectorData::Dense(d) => assert_eq!(d.len(), 3),
            _ => panic!("expected Dense"),
        }
    }

    #[tokio::test]
    async fn encode_batch_default_impl() {
        let v = FakeVectoriser;
        let batch = vec![
            FieldSet::from([("name".into(), "A".into())]),
            FieldSet::from([("name".into(), "B".into())]),
        ];
        let results = v.encode_batch(&batch).await.unwrap();
        assert_eq!(results.len(), 2);
    }
}
```

- [ ] **Step 3: Register the module in lib.rs**

In `crates/trondb-core/src/lib.rs`, add after the existing module declarations (line 14):
```rust
pub mod vectoriser;
```

- [ ] **Step 4: Run tests to verify**

Run: `cargo test -p trondb-core vectoriser -- --nocapture`
Expected: All 5 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/vectoriser.rs crates/trondb-core/src/lib.rs crates/trondb-core/Cargo.toml
git commit -m "feat(core): add Vectoriser trait, VectoriserError, VectoriserRegistry"
```

---

### Task 2: New Crate trondb-vectoriser + PassthroughVectoriser + MockVectoriser

**Files:**
- Create: `crates/trondb-vectoriser/Cargo.toml`
- Create: `crates/trondb-vectoriser/src/lib.rs`
- Create: `crates/trondb-vectoriser/src/passthrough.rs`
- Create: `crates/trondb-vectoriser/src/mock.rs`
- Modify: `Cargo.toml` (workspace — add member + deps)

**Context:** PassthroughVectoriser formalises the current behaviour — vectors are provided by the caller at INSERT time. It does NOT support encode() or encode_query() (returns NotSupported). MockVectoriser is a deterministic fake for integration tests — always returns a fixed vector derived from a hash of the input, so tests are reproducible.

- [ ] **Step 1: Add crate to workspace**

In `/mnt/truenas/projects/TronDB/Cargo.toml`, add `"crates/trondb-vectoriser"` to the `members` array. Add workspace deps:
```toml
ort = { version = "2", optional = true }
tokenizers = { version = "0.21", optional = true }
reqwest = { version = "0.12", features = ["json"], optional = true }
```

Wait — workspace deps can't be optional. Instead, just add the non-optional ones to workspace and let the crate Cargo.toml handle optionality. Actually, for workspace deps that are only used by one crate, it's cleaner to declare them directly in the crate's Cargo.toml rather than workspace. Only add to workspace if shared.

So only modify workspace `Cargo.toml` members:
```toml
members = [
    "crates/trondb-core",
    "crates/trondb-tql",
    "crates/trondb-cli",
    "crates/trondb-wal",
    "crates/trondb-routing",
    "crates/trondb-proto",
    "crates/trondb-server",
    "crates/trondb-vectoriser",
]
```

- [ ] **Step 2: Create crate Cargo.toml**

Create `crates/trondb-vectoriser/Cargo.toml`:
```toml
[package]
name = "trondb-vectoriser"
version = "0.1.0"
edition = "2021"

[dependencies]
trondb-core = { path = "../trondb-core" }
async-trait.workspace = true
thiserror.workspace = true
serde.workspace = true
sha2.workspace = true
tokio.workspace = true
tracing.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }

[features]
default = []
onnx = ["dep:ort", "dep:tokenizers"]
external = ["dep:reqwest"]

[dependencies.ort]
version = "2"
optional = true

[dependencies.tokenizers]
version = "0.21"
optional = true
default-features = false
features = ["onig"]

[dependencies.reqwest]
version = "0.12"
features = ["json"]
optional = true
```

- [ ] **Step 3: Create lib.rs**

Create `crates/trondb-vectoriser/src/lib.rs`:
```rust
pub mod passthrough;
pub mod mock;

// Feature-gated modules
#[cfg(feature = "onnx")]
pub mod onnx_dense;
#[cfg(feature = "onnx")]
pub mod onnx_sparse;

pub mod network;

#[cfg(feature = "external")]
pub mod external;

pub mod recipe;

// Re-exports for convenience
pub use passthrough::PassthroughVectoriser;
pub use mock::MockVectoriser;
```

- [ ] **Step 4: Write PassthroughVectoriser with tests**

Create `crates/trondb-vectoriser/src/passthrough.rs`:
```rust
use async_trait::async_trait;
use trondb_core::types::VectorData;
use trondb_core::vectoriser::{FieldSet, VectorKind, Vectoriser, VectoriserError};

/// Accepts pre-computed vectors at INSERT time. Does not generate vectors.
/// encode() and encode_query() return NotSupported.
/// This formalises the existing TronDB behaviour before Phase 10.
pub struct PassthroughVectoriser {
    dimensions: usize,
    kind: VectorKind,
}

impl PassthroughVectoriser {
    pub fn new_dense(dimensions: usize) -> Self {
        Self { dimensions, kind: VectorKind::Dense }
    }

    pub fn new_sparse() -> Self {
        Self { dimensions: 0, kind: VectorKind::Sparse }
    }
}

#[async_trait]
impl Vectoriser for PassthroughVectoriser {
    fn id(&self) -> &str { "passthrough" }
    fn model_id(&self) -> &str { "passthrough" }
    fn output_size(&self) -> usize { self.dimensions }
    fn output_kind(&self) -> VectorKind { self.kind }

    async fn encode(&self, _fields: &FieldSet) -> Result<VectorData, VectoriserError> {
        Err(VectoriserError::NotSupported(
            "PassthroughVectoriser does not generate vectors — provide vectors at INSERT time".into()
        ))
    }

    async fn encode_query(&self, _query: &str) -> Result<VectorData, VectoriserError> {
        Err(VectoriserError::NotSupported(
            "PassthroughVectoriser does not support natural language queries".into()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_dense_metadata() {
        let v = PassthroughVectoriser::new_dense(384);
        assert_eq!(v.id(), "passthrough");
        assert_eq!(v.model_id(), "passthrough");
        assert_eq!(v.output_size(), 384);
        assert_eq!(v.output_kind(), VectorKind::Dense);
    }

    #[test]
    fn passthrough_sparse_metadata() {
        let v = PassthroughVectoriser::new_sparse();
        assert_eq!(v.output_size(), 0);
        assert_eq!(v.output_kind(), VectorKind::Sparse);
    }

    #[tokio::test]
    async fn encode_returns_not_supported() {
        let v = PassthroughVectoriser::new_dense(384);
        let fields = FieldSet::from([("name".into(), "test".into())]);
        let err = v.encode(&fields).await.unwrap_err();
        assert!(matches!(err, VectoriserError::NotSupported(_)));
    }

    #[tokio::test]
    async fn encode_query_returns_not_supported() {
        let v = PassthroughVectoriser::new_dense(384);
        let err = v.encode_query("test query").await.unwrap_err();
        assert!(matches!(err, VectoriserError::NotSupported(_)));
    }
}
```

- [ ] **Step 5: Write MockVectoriser with tests**

Create `crates/trondb-vectoriser/src/mock.rs`:
```rust
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use trondb_core::types::VectorData;
use trondb_core::vectoriser::{FieldSet, VectorKind, Vectoriser, VectoriserError};

/// Deterministic mock vectoriser for tests.
/// Produces a reproducible vector by hashing input fields.
/// Not a real embedding model — just consistent fake data.
pub struct MockVectoriser {
    dimensions: usize,
}

impl MockVectoriser {
    pub fn new(dimensions: usize) -> Self {
        Self { dimensions }
    }

    /// Generate a deterministic vector from a string by hashing it.
    fn hash_to_vector(&self, input: &str) -> Vec<f32> {
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        let hash = hasher.finalize();

        // Expand hash bytes into f32 values in [-1, 1]
        let mut vector = Vec::with_capacity(self.dimensions);
        for i in 0..self.dimensions {
            let byte_idx = i % 32;
            let raw = hash[byte_idx] as f32 / 128.0 - 1.0; // maps 0..255 to -1..~0.99
            vector.push(raw);
        }
        vector
    }
}

#[async_trait]
impl Vectoriser for MockVectoriser {
    fn id(&self) -> &str { "mock" }
    fn model_id(&self) -> &str { "mock-model" }
    fn output_size(&self) -> usize { self.dimensions }
    fn output_kind(&self) -> VectorKind { VectorKind::Dense }

    async fn encode(&self, fields: &FieldSet) -> Result<VectorData, VectoriserError> {
        // Sort fields for determinism, concatenate
        let mut pairs: Vec<_> = fields.iter().collect();
        pairs.sort_by_key(|(k, _)| k.clone());
        let combined: String = pairs.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("|");
        Ok(VectorData::Dense(self.hash_to_vector(&combined)))
    }

    async fn encode_query(&self, query: &str) -> Result<VectorData, VectoriserError> {
        Ok(VectorData::Dense(self.hash_to_vector(query)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn encode_is_deterministic() {
        let v = MockVectoriser::new(8);
        let fields = FieldSet::from([("name".into(), "Jazz Club".into())]);
        let a = v.encode(&fields).await.unwrap();
        let b = v.encode(&fields).await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn encode_different_input_different_output() {
        let v = MockVectoriser::new(8);
        let a = v.encode(&FieldSet::from([("name".into(), "Jazz".into())])).await.unwrap();
        let b = v.encode(&FieldSet::from([("name".into(), "Rock".into())])).await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn encode_query_works() {
        let v = MockVectoriser::new(4);
        let result = v.encode_query("live jazz in Bristol").await.unwrap();
        match result {
            VectorData::Dense(d) => assert_eq!(d.len(), 4),
            _ => panic!("expected Dense"),
        }
    }

    #[tokio::test]
    async fn output_dimensions_match() {
        let v = MockVectoriser::new(384);
        let fields = FieldSet::from([("x".into(), "y".into())]);
        match v.encode(&fields).await.unwrap() {
            VectorData::Dense(d) => assert_eq!(d.len(), 384),
            _ => panic!("expected Dense"),
        }
    }
}
```

- [ ] **Step 6: Create stub files for ALL feature-gated modules**

Create `crates/trondb-vectoriser/src/network.rs`:
```rust
// NetworkVectoriser — Task 14
```

Create `crates/trondb-vectoriser/src/recipe.rs`:
```rust
// Recipe hash re-export — Task 4
```

Create `crates/trondb-vectoriser/src/onnx_dense.rs`:
```rust
// OnnxDenseVectoriser — Task 12 (behind `onnx` feature)
```

Create `crates/trondb-vectoriser/src/onnx_sparse.rs`:
```rust
// OnnxSparseVectoriser — Task 13 (behind `onnx` feature)
```

Create `crates/trondb-vectoriser/src/external.rs`:
```rust
// ExternalVectoriser — Task 14 (behind `external` feature)
```

- [ ] **Step 7: Run tests**

Run: `cargo test -p trondb-vectoriser -- --nocapture`
Expected: All passthrough + mock tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/trondb-vectoriser/ Cargo.toml
git commit -m "feat(vectoriser): new crate with PassthroughVectoriser + MockVectoriser"
```

---

### Task 3: Schema Evolution — FIELDS in REPRESENTATION + VectoriserConfig

**Files:**
- Modify: `crates/trondb-tql/src/token.rs` (new tokens)
- Modify: `crates/trondb-tql/src/ast.rs` (RepresentationDecl gains fields, new VectoriserConfigDecl)
- Modify: `crates/trondb-tql/src/parser.rs` (parse FIELDS clause, vectoriser config)
- Modify: `crates/trondb-core/src/types.rs` (StoredRepresentation gains fields, VectoriserConfig)

**Context:** Currently `RepresentationDecl` has no FIELDS clause — all representations are passthrough. This task adds the FIELDS keyword so that managed representations declare which entity fields contribute to the vector. The vectoriser config (MODEL_PATH, DEVICE, VECTORISER type, ENDPOINT, AUTH) goes on the collection level. Existing syntax (no FIELDS) continues to work as passthrough.

New CREATE COLLECTION syntax after this task:
```sql
CREATE COLLECTION events (
    -- Collection-level vectoriser config (optional — omit for passthrough-only)
    MODEL 'bge-small-en-v1.5'
    MODEL_PATH '/models/bge-small-en-v1.5.onnx'
    DEVICE 'cpu'

    FIELD name TEXT,
    FIELD description TEXT,
    FIELD category TEXT,

    -- Passthrough representation (no FIELDS clause — backwards compatible)
    REPRESENTATION identity DIMENSIONS 384,

    -- Managed representation (FIELDS clause — vectoriser generates this)
    REPRESENTATION semantic DIMENSIONS 384 FIELDS (name, description, category),

    INDEX idx_category ON (category),
);
```

- [ ] **Step 1: Add new tokens to lexer**

In `crates/trondb-tql/src/token.rs`, add these tokens inside the `Token` enum (after the existing keyword tokens, before `Ident`):

```rust
    #[token("FIELDS", priority = 10, ignore(ascii_case))]
    Fields,

    #[token("USING", priority = 10, ignore(ascii_case))]
    Using,

    #[token("MODEL_PATH", priority = 10, ignore(ascii_case))]
    ModelPath,

    #[token("DEVICE", priority = 10, ignore(ascii_case))]
    TokenDevice,
```

Note: `MODEL` already exists as Token::Model. `VECTORISER`/`ENDPOINT`/`AUTH` will be parsed as identifiers when they appear as key-value config — we don't need dedicated tokens for them since they appear in a structured context. Actually, let's add them for cleaner parsing:

```rust
    #[token("VECTORISER", priority = 10, ignore(ascii_case))]
    TokenVectoriser,

    #[token("ENDPOINT", priority = 10, ignore(ascii_case))]
    TokenEndpoint,

    #[token("AUTH", priority = 10, ignore(ascii_case))]
    TokenAuth,
```

- [ ] **Step 2: Write lexer tests for new tokens**

In `crates/trondb-tql/src/token.rs` tests section, add:

```rust
    #[test]
    fn lex_fields_keyword() {
        let tokens = lex("FIELDS (name, description)");
        assert_eq!(tokens[0], Token::Fields);
        assert_eq!(tokens[1], Token::LParen);
        assert_eq!(tokens[2], Token::Ident("name".to_string()));
        assert_eq!(tokens[3], Token::Comma);
        assert_eq!(tokens[4], Token::Ident("description".to_string()));
        assert_eq!(tokens[5], Token::RParen);
    }

    #[test]
    fn lex_using_keyword() {
        assert_eq!(lex("USING"), vec![Token::Using]);
        assert_eq!(lex("using"), vec![Token::Using]);
    }

    #[test]
    fn lex_vectoriser_config_tokens() {
        let tokens = lex("MODEL_PATH '/models/bge.onnx' DEVICE 'cpu'");
        assert_eq!(tokens[0], Token::ModelPath);
        assert_eq!(tokens[1], Token::StringLit("/models/bge.onnx".to_string()));
        assert_eq!(tokens[2], Token::TokenDevice);
        assert_eq!(tokens[3], Token::StringLit("cpu".to_string()));
    }
```

Run: `cargo test -p trondb-tql token::tests -- --nocapture`
Expected: New tests pass (plus all existing lexer tests).

- [ ] **Step 3: Update AST types**

In `crates/trondb-tql/src/ast.rs`:

Add `fields` to `RepresentationDecl`:
```rust
#[derive(Debug, Clone, PartialEq)]
pub struct RepresentationDecl {
    pub name: String,
    pub model: Option<String>,
    pub dimensions: Option<usize>,
    pub metric: Metric,
    pub sparse: bool,
    pub fields: Vec<String>,  // NEW — empty means passthrough
}
```

Add `VectoriserConfigDecl` and update `CreateCollectionStmt`:
```rust
/// Collection-level vectoriser configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct VectoriserConfigDecl {
    pub model: Option<String>,
    pub model_path: Option<String>,
    pub device: Option<String>,
    pub vectoriser_type: Option<String>,  // "external", "network", or absent for local
    pub endpoint: Option<String>,
    pub auth: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateCollectionStmt {
    pub name: String,
    pub representations: Vec<RepresentationDecl>,
    pub fields: Vec<FieldDecl>,
    pub indexes: Vec<IndexDecl>,
    pub vectoriser_config: Option<VectoriserConfigDecl>,  // NEW
}
```

Add `query_text` and `using_repr` to `SearchStmt`:
```rust
#[derive(Debug, Clone, PartialEq)]
pub struct SearchStmt {
    pub collection: String,
    pub fields: FieldList,
    pub dense_vector: Option<Vec<f64>>,
    pub sparse_vector: Option<Vec<(u32, f32)>>,
    pub query_text: Option<String>,    // NEW — NEAR 'text query'
    pub using_repr: Option<String>,    // NEW — USING repr_name
    pub filter: Option<WhereClause>,
    pub confidence: Option<f64>,
    pub limit: Option<usize>,
}
```

- [ ] **Step 4: Update parser — FIELDS clause in REPRESENTATION**

In `crates/trondb-tql/src/parser.rs`, update `parse_representation_decl()`:

After the existing loop body (which handles Model, Dimensions, TokenMetric, Sparse), add a new arm for Fields:

```rust
                Some(Token::Fields) => {
                    self.advance();
                    self.expect(&Token::LParen)?;
                    let mut field_names = vec![self.expect_ident()?];
                    while self.peek() == Some(&Token::Comma) {
                        self.advance();
                        field_names.push(self.expect_ident()?);
                    }
                    self.expect(&Token::RParen)?;
                    fields = field_names;
                }
```

Add `let mut fields = Vec::new();` at the top of the function alongside the other `let mut` declarations.

Update the return:
```rust
        Ok(RepresentationDecl { name, model, dimensions, metric, sparse, fields })
```

- [ ] **Step 5: Update parser — collection-level vectoriser config**

In the `parse_create_collection()` function, before the representation/field/index loop, add parsing for collection-level config tokens (MODEL, MODEL_PATH, DEVICE, VECTORISER, ENDPOINT, AUTH). These appear before the first REPRESENTATION/FIELD/INDEX declaration:

```rust
        // Parse optional collection-level vectoriser config (before declarations)
        let mut vec_model = None;
        let mut vec_model_path = None;
        let mut vec_device = None;
        let mut vec_type = None;
        let mut vec_endpoint = None;
        let mut vec_auth = None;

        loop {
            match self.peek() {
                Some(Token::Model) if vec_model.is_none() => {
                    self.advance();
                    vec_model = Some(self.expect_string_lit()?);
                }
                Some(Token::ModelPath) => {
                    self.advance();
                    vec_model_path = Some(self.expect_string_lit()?);
                }
                Some(Token::TokenDevice) => {
                    self.advance();
                    vec_device = Some(self.expect_string_lit()?);
                }
                Some(Token::TokenVectoriser) => {
                    self.advance();
                    vec_type = Some(self.expect_string_lit()?);
                }
                Some(Token::TokenEndpoint) => {
                    self.advance();
                    vec_endpoint = Some(self.expect_string_lit()?);
                }
                Some(Token::TokenAuth) => {
                    self.advance();
                    vec_auth = Some(self.expect_string_lit()?);
                }
                _ => break,
            }
        }

        let vectoriser_config = if vec_model.is_some() || vec_model_path.is_some()
            || vec_device.is_some() || vec_type.is_some()
            || vec_endpoint.is_some() || vec_auth.is_some()
        {
            Some(VectoriserConfigDecl {
                model: vec_model,
                model_path: vec_model_path,
                device: vec_device,
                vectoriser_type: vec_type,
                endpoint: vec_endpoint,
                auth: vec_auth,
            })
        } else {
            None
        };
```

Update the return of `parse_create_collection()` to include `vectoriser_config`.

**MODEL token ambiguity resolution:** The collection-level config loop runs FIRST (before the REPRESENTATION/FIELD/INDEX loop). Since the config parser only matches `Token::Model` when `vec_model.is_none()`, it consumes at most ONE MODEL token. This is safe because:
- If a collection declares `MODEL 'bge'` at the top, the config parser takes it
- Individual REPRESENTATIONs that also specify `MODEL 'something'` are parsed inside `parse_representation_decl()`, which runs later in the declaration loop
- If there's NO collection-level MODEL, the config parser's `Some(Token::Model) if vec_model.is_none()` guard never fires, so the MODEL token falls through to the `_ => break` and remains available for the REPRESENTATION parser

**Edge case:** If someone writes `MODEL 'bge' MODEL 'jina'` at the collection level, only the first is consumed (guard `vec_model.is_none()`), and the second triggers `_ => break` ending the config loop. The second MODEL then enters the declaration loop where it would fail because a bare MODEL isn't a valid start for REPRESENTATION/FIELD/INDEX. This is acceptable — duplicate collection-level MODEL is user error.

- [ ] **Step 6: Fix all existing code that constructs RepresentationDecl, CreateCollectionStmt, SearchStmt, or StoredRepresentation**

These types gained new fields. Every manual construction site must be updated. Known sites (verify with `cargo build --workspace 2>&1 | grep "missing field"`):

**`RepresentationDecl`** — add `fields: vec![]`:
- `crates/trondb-tql/src/parser.rs` — `parse_representation_decl()` return (updated in Step 4)
- `crates/trondb-core/src/planner.rs` — test functions (search for `RepresentationDecl {`)

**`CreateCollectionStmt`** — add `vectoriser_config: None`:
- `crates/trondb-tql/src/parser.rs` — `parse_create_collection()` return
- Any test that constructs CreateCollectionStmt directly

**`SearchStmt`** — add `query_text: None, using_repr: None`:
- `crates/trondb-tql/src/parser.rs` — `parse_search()` return (updated in Task 10, but all existing test assertions that pattern-match SearchStmt fields need updating now)
- `crates/trondb-core/src/planner.rs` — test functions that construct SearchStmt directly (search for `SearchStmt {`)

**`StoredRepresentation`** — add `fields: vec![]`:
- `crates/trondb-core/src/types.rs` — `collection_schema_round_trip` test (~line 289)
- `crates/trondb-core/src/executor.rs` — CREATE COLLECTION executor arm where StoredRepresentation is built
- `crates/trondb-core/src/planner.rs` — any test that builds StoredRepresentation

**`CollectionSchema`** — add `vectoriser_config: None`:
- `crates/trondb-core/src/types.rs` — `collection_schema_round_trip` test
- Any executor test helpers that build schemas

Run: `cargo test --workspace -- --nocapture`
Expected: All existing tests pass (backwards compatible — empty fields = passthrough, None config = no vectoriser).

- [ ] **Step 7: Write parser tests for new syntax**

In `crates/trondb-tql/src/parser.rs` test section:

```rust
    #[test]
    fn parse_representation_with_fields() {
        let stmt = parse("CREATE COLLECTION events (
            FIELD name TEXT,
            FIELD description TEXT,
            REPRESENTATION semantic DIMENSIONS 384 FIELDS (name, description),
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                assert_eq!(c.representations.len(), 1);
                assert_eq!(c.representations[0].fields, vec!["name", "description"]);
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_representation_without_fields_is_passthrough() {
        let stmt = parse("CREATE COLLECTION venues (
            REPRESENTATION identity DIMENSIONS 384,
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                assert!(c.representations[0].fields.is_empty());
                assert!(c.vectoriser_config.is_none());
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_existing_create_collection_syntax_unchanged() {
        // Exact syntax from existing tests — must still parse identically
        let stmt = parse("CREATE COLLECTION venues (
            REPRESENTATION default MODEL 'jina-v4' DIMENSIONS 1024 METRIC COSINE,
            FIELD status TEXT,
            INDEX idx_status ON (status),
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                assert_eq!(c.name, "venues");
                assert_eq!(c.representations.len(), 1);
                assert_eq!(c.representations[0].model, Some("jina-v4".into()));
                assert!(c.representations[0].fields.is_empty()); // no FIELDS = passthrough
                assert!(c.vectoriser_config.is_none()); // no collection-level config
                assert_eq!(c.fields.len(), 1);
                assert_eq!(c.indexes.len(), 1);
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_collection_with_vectoriser_config() {
        let stmt = parse("CREATE COLLECTION events (
            MODEL 'bge-small-en-v1.5'
            MODEL_PATH '/models/bge.onnx'
            DEVICE 'cpu'
            FIELD name TEXT,
            REPRESENTATION semantic DIMENSIONS 384 FIELDS (name),
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                let vc = c.vectoriser_config.unwrap();
                assert_eq!(vc.model.unwrap(), "bge-small-en-v1.5");
                assert_eq!(vc.model_path.unwrap(), "/models/bge.onnx");
                assert_eq!(vc.device.unwrap(), "cpu");
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn parse_collection_external_vectoriser() {
        let stmt = parse("CREATE COLLECTION events (
            MODEL 'text-embedding-3-small'
            VECTORISER 'external'
            ENDPOINT 'https://api.openai.com/v1/embeddings'
            AUTH 'env:OPENAI_API_KEY'
            FIELD name TEXT,
            REPRESENTATION embed DIMENSIONS 1536 FIELDS (name),
        );").unwrap();
        match stmt {
            Statement::CreateCollection(c) => {
                let vc = c.vectoriser_config.unwrap();
                assert_eq!(vc.vectoriser_type.unwrap(), "external");
                assert_eq!(vc.endpoint.unwrap(), "https://api.openai.com/v1/embeddings");
                assert_eq!(vc.auth.unwrap(), "env:OPENAI_API_KEY");
            }
            _ => panic!("expected CreateCollection"),
        }
    }
```

Run: `cargo test -p trondb-tql -- --nocapture`
Expected: All tests pass.

- [ ] **Step 8: Update StoredRepresentation in trondb-core types**

In `crates/trondb-core/src/types.rs`, update `StoredRepresentation`.

**IMPORTANT:** Use `#[serde(default)]` on ALL new fields. Without this, deserialisation of existing Fjall-persisted schemas (which lack these fields) will fail on startup, breaking existing databases.

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredRepresentation {
    pub name: String,
    pub model: Option<String>,
    pub dimensions: Option<usize>,
    pub metric: Metric,
    pub sparse: bool,
    #[serde(default)]  // REQUIRED: existing persisted schemas lack this field
    pub fields: Vec<String>,  // NEW — empty = passthrough
}
```

Add `VectoriserConfig` to types:
```rust
/// Collection-level vectoriser configuration, persisted in schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectoriserConfig {
    pub model: Option<String>,
    pub model_path: Option<String>,
    pub device: Option<String>,
    pub vectoriser_type: Option<String>,
    pub endpoint: Option<String>,
    pub auth: Option<String>,
}
```

Update `CollectionSchema`:
```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionSchema {
    pub name: String,
    pub representations: Vec<StoredRepresentation>,
    pub fields: Vec<StoredField>,
    pub indexes: Vec<StoredIndex>,
    #[serde(default)]  // REQUIRED: existing persisted schemas lack this field
    pub vectoriser_config: Option<VectoriserConfig>,  // NEW
}
```

- [ ] **Step 9: Update executor's CREATE COLLECTION to store new fields**

In `crates/trondb-core/src/executor.rs`, find the `Plan::CreateCollection(p)` arm. Update the `CollectionSchema` construction to include the new fields from the plan. The `StoredRepresentation` construction needs the `fields` field from `RepresentationDecl`.

Find where `StoredRepresentation` is constructed (in the CreateCollection executor arm) and add:
```rust
fields: repr_decl.fields.clone(),
```

For `vectoriser_config`, convert from the AST's `VectoriserConfigDecl` to `VectoriserConfig`:
```rust
vectoriser_config: p.vectoriser_config.as_ref().map(|vc| VectoriserConfig {
    model: vc.model.clone(),
    model_path: vc.model_path.clone(),
    device: vc.device.clone(),
    vectoriser_type: vc.vectoriser_type.clone(),
    endpoint: vc.endpoint.clone(),
    auth: vc.auth.clone(),
}),
```

Also update the planner's `CreateCollectionPlan` to carry the `vectoriser_config`:

In `crates/trondb-core/src/planner.rs`, update `CreateCollectionPlan`:
```rust
#[derive(Debug, Clone)]
pub struct CreateCollectionPlan {
    pub name: String,
    pub representations: Vec<trondb_tql::RepresentationDecl>,
    pub fields: Vec<trondb_tql::FieldDecl>,
    pub indexes: Vec<trondb_tql::IndexDecl>,
    pub vectoriser_config: Option<trondb_tql::VectoriserConfigDecl>,  // NEW
}
```

And in the `plan()` function's `CreateCollection` arm, pass through `vectoriser_config`:
```rust
vectoriser_config: stmt.vectoriser_config.clone(),
```

- [ ] **Step 10: Fix all compilation errors and run full test suite**

Run: `cargo test --workspace -- --nocapture`
Expected: All tests pass. Existing CREATE COLLECTION tests work because `fields` defaults to `Vec::new()` and `vectoriser_config` defaults to `None`.

Note: The existing `collection_schema_round_trip` test in `types.rs` constructs `CollectionSchema` — update it to include `vectoriser_config: None` and update `StoredRepresentation` to include `fields: vec![]`.

- [ ] **Step 11: Commit**

```bash
git add crates/trondb-tql/ crates/trondb-core/src/types.rs crates/trondb-core/src/planner.rs crates/trondb-core/src/executor.rs
git commit -m "feat(schema): add FIELDS clause to REPRESENTATION, collection-level vectoriser config"
```

---

### Task 4: Recipe Hash Computation

**Files:**
- Modify: `crates/trondb-core/src/vectoriser.rs` (add compute_recipe_hash here — executor needs it, so it lives in core)
- Create content in: `crates/trondb-vectoriser/src/recipe.rs` (re-export only)
- Modify: `crates/trondb-core/src/executor.rs` (compute hash on INSERT)

**Context:** The `recipe_hash` on each `Representation` is a SHA-256 digest of the representation's configuration: model_id + sorted field names. If any contributing field changes or the model changes, the hash changes, and the representation is stale. Currently `recipe_hash` is always `[0u8; 32]`.

The function lives in `trondb-core::vectoriser` (not `trondb-vectoriser`) because the executor uses it directly and `trondb-core` cannot depend on `trondb-vectoriser` (that would create a circular dependency).

- [ ] **Step 1: Add compute_recipe_hash to trondb-core/src/vectoriser.rs with tests**

Add to the bottom of `crates/trondb-core/src/vectoriser.rs` (before `#[cfg(test)]`):

```rust
// ---------------------------------------------------------------------------
// Recipe hash — staleness detection for mutation cascade
// ---------------------------------------------------------------------------

/// Compute a recipe hash from the vectoriser model ID and contributing field names.
/// The hash changes when the model or the field list changes.
/// Used for staleness detection in the mutation cascade.
pub fn compute_recipe_hash(model_id: &str, fields: &[String]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"recipe:v1:");
    hasher.update(model_id.as_bytes());
    hasher.update(b":");
    // Sort fields for determinism
    let mut sorted_fields = fields.to_vec();
    sorted_fields.sort();
    for (i, field) in sorted_fields.iter().enumerate() {
        if i > 0 { hasher.update(b","); }
        hasher.update(field.as_bytes());
    }
    hasher.finalize().into()
}
```

Add tests in the same file's `#[cfg(test)]` block:

```rust
    #[test]
    fn recipe_hash_is_deterministic() {
        let fields = vec!["name".into(), "description".into()];
        let a = compute_recipe_hash("bge-small-en-v1.5", &fields);
        let b = compute_recipe_hash("bge-small-en-v1.5", &fields);
        assert_eq!(a, b);
    }

    #[test]
    fn recipe_hash_changes_with_model() {
        let fields = vec!["name".into()];
        let a = compute_recipe_hash("bge-small-en-v1.5", &fields);
        let b = compute_recipe_hash("jina-v4", &fields);
        assert_ne!(a, b);
    }

    #[test]
    fn recipe_hash_changes_with_fields() {
        let a = compute_recipe_hash("bge", &vec!["name".into()]);
        let b = compute_recipe_hash("bge", &vec!["name".into(), "description".into()]);
        assert_ne!(a, b);
    }

    #[test]
    fn recipe_hash_is_field_order_independent() {
        let a = compute_recipe_hash("bge", &vec!["name".into(), "description".into()]);
        let b = compute_recipe_hash("bge", &vec!["description".into(), "name".into()]);
        assert_eq!(a, b);
    }

    #[test]
    fn recipe_hash_is_not_all_zeros() {
        let h = compute_recipe_hash("bge", &vec!["name".into()]);
        assert_ne!(h, [0u8; 32]);
    }
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p trondb-core vectoriser -- --nocapture`
Expected: All vectoriser tests pass (trait tests from Task 1 + new recipe hash tests).

- [ ] **Step 3: Create re-export in trondb-vectoriser/src/recipe.rs**

In `crates/trondb-vectoriser/src/recipe.rs`:
```rust
pub use trondb_core::vectoriser::compute_recipe_hash;
```

- [ ] **Step 4: Update INSERT executor to compute recipe_hash**

In `crates/trondb-core/src/executor.rs`, at the point where `Representation` structs are built during INSERT (around lines 334-342), compute the recipe hash instead of using `[0u8; 32]`.

Change the Representation construction:
```rust
let recipe_hash = if repr_decl_fields.is_empty() {
    [0u8; 32] // passthrough — no staleness tracking
} else {
    let model_id = schema.vectoriser_config.as_ref()
        .and_then(|vc| vc.model.as_deref())
        .unwrap_or("unknown");
    crate::vectoriser::compute_recipe_hash(model_id, &repr_decl_fields)
};
```

Where `repr_decl_fields` comes from looking up the StoredRepresentation's fields for the matching repr_name.

- [ ] **Step 4: Run full test suite**

Run: `cargo test --workspace -- --nocapture`
Expected: All tests pass. Existing INSERT tests still work (passthrough reprs get `[0u8; 32]`).

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/vectoriser.rs crates/trondb-core/src/executor.rs crates/trondb-vectoriser/src/recipe.rs
git commit -m "feat(core): recipe_hash computation for staleness detection"
```

---

## Chunk 2: Core Features

### Task 5: Auto-vectorise INSERT (Fields-Only Path)

**Files:**
- Modify: `crates/trondb-core/src/executor.rs` (INSERT arm)
- Modify: `crates/trondb-core/src/lib.rs` (Engine holds VectoriserRegistry)
- Modify: `crates/trondb-core/Cargo.toml` (if needed)

**Context:** Currently INSERT requires explicit vectors (`REPRESENTATION identity VECTOR [...]`). After this task, if a representation has FIELDS configured and the collection has a vectoriser, the executor calls `vectoriser.encode()` with the entity's field values to generate the vector automatically. Explicit vectors are still accepted for passthrough representations. An INSERT that provides fields but no vector for a managed representation triggers auto-vectorisation.

The VectoriserRegistry must be accessible from the Executor. Currently Executor is created in `Engine::open()`. We need to add the registry as a field.

**Test dependency:** Integration tests in `trondb-core` need a mock vectoriser. Since `trondb-core` cannot depend on `trondb-vectoriser` (circular), add `trondb-vectoriser` as a **dev-dependency** of `trondb-core`. Dev-dependencies don't create circular production dependencies — they're only used for tests.

In `crates/trondb-core/Cargo.toml`:
```toml
[dev-dependencies]
tempfile = "3"
trondb-vectoriser = { path = "../trondb-vectoriser" }
```

- [ ] **Step 1: Add VectoriserRegistry to Executor**

In `crates/trondb-core/src/executor.rs`:

Add to Executor struct:
```rust
    vectoriser_registry: Arc<VectoriserRegistry>,
```

Update `Executor::new()` to accept it:
```rust
    pub fn new(
        store: FjallStore,
        wal: WalWriter,
        location: Arc<LocationTable>,
        vectoriser_registry: Arc<VectoriserRegistry>,
    ) -> Self {
```

And store it: `vectoriser_registry,`

In `crates/trondb-core/src/lib.rs`, update `Engine::open()` to create and pass a VectoriserRegistry:
```rust
use crate::vectoriser::VectoriserRegistry;

// In Engine struct:
    vectoriser_registry: Arc<VectoriserRegistry>,

// In Engine::open():
    let vectoriser_registry = Arc::new(VectoriserRegistry::new());
    let executor = Executor::new(store, wal, location.clone(), vectoriser_registry.clone());
```

Add a public method to Engine for vectoriser registration:
```rust
    pub fn vectoriser_registry(&self) -> &Arc<VectoriserRegistry> {
        &self.vectoriser_registry
    }
```

Fix all compilation errors (other callers of `Executor::new`, including tests).

- [ ] **Step 2: Write failing integration test**

In `crates/trondb-core/src/executor.rs` test section (or a new test file), add:

```rust
    #[tokio::test]
    async fn insert_auto_vectorise_from_fields() {
        // Setup: create engine with mock vectoriser
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = Engine::open(test_config(dir.path())).await.unwrap();

        // Create collection with FIELDS
        engine.execute("CREATE COLLECTION events (
            MODEL 'mock-model'
            FIELD name TEXT,
            FIELD description TEXT,
            REPRESENTATION semantic DIMENSIONS 8 FIELDS (name, description),
        );").await.unwrap();

        // Register mock vectoriser for this collection's representation
        let mock = Arc::new(MockVectoriser::new(8));
        engine.vectoriser_registry().register("events", "semantic", mock);

        // INSERT without explicit vector — should auto-vectorise
        engine.execute("INSERT INTO events (id, name, description) VALUES ('e1', 'Jazz Night', 'Live jazz at the Blue Note');").await.unwrap();

        // Verify: SEARCH should find it (vector was generated)
        // We need a vector to search with — use the mock's encode_query
        let mock2 = MockVectoriser::new(8);
        let query_vec = mock2.encode_query("jazz").await.unwrap();
        // ... search using that vector
    }
```

This test will fail because the INSERT executor doesn't auto-vectorise yet.

- [ ] **Step 3: Implement auto-vectorisation in INSERT**

In the INSERT arm of `executor.rs`, after building entity metadata and before the WAL transaction:

1. Look up the schema's representations
2. For each stored representation that has non-empty `fields`:
   a. Check if the INSERT already provided an explicit vector for this repr name
   b. If not, check if a vectoriser is registered for this collection:repr
   c. If yes, build a FieldSet from the entity's metadata for the contributing fields
   d. Call `vectoriser.encode(&field_set).await`
   e. Create a Representation with the generated vector

```rust
// Auto-vectorise managed representations that weren't explicitly provided
let explicit_repr_names: HashSet<&str> = p.vectors.iter().map(|(n, _)| n.as_str()).collect();

for stored_repr in &schema.representations {
    if stored_repr.fields.is_empty() {
        continue; // passthrough — skip
    }
    if explicit_repr_names.contains(stored_repr.name.as_str()) {
        continue; // caller provided explicit vector — skip
    }

    // Build FieldSet from entity metadata for the contributing fields
    let field_set: FieldSet = stored_repr.fields.iter()
        .filter_map(|f| {
            entity.metadata.get(f).map(|v| (f.clone(), v.to_string()))
        })
        .collect();

    if field_set.is_empty() {
        continue; // none of the contributing fields have values
    }

    // Look up vectoriser
    let vectoriser = self.vectoriser_registry
        .get(&p.collection, &stored_repr.name)
        .ok_or_else(|| EngineError::InvalidQuery(format!(
            "representation '{}' has FIELDS but no vectoriser registered for collection '{}'",
            stored_repr.name, p.collection
        )))?;

    let vector_data = vectoriser.encode(&field_set).await
        .map_err(|e| EngineError::Storage(format!("vectoriser encode failed: {e}")))?;

    let model_id = vectoriser.model_id();
    let recipe_hash = crate::vectoriser::compute_recipe_hash(model_id, &stored_repr.fields);

    let repr = Representation {
        name: stored_repr.name.clone(),
        repr_type: if stored_repr.fields.len() > 1 { ReprType::Composite } else { ReprType::Atomic },
        fields: stored_repr.fields.clone(),
        vector: vector_data,
        recipe_hash,
        state: ReprState::Clean,
    };
    entity.representations.push(repr);
}
```

Import `FieldSet` at the top of executor.rs:
```rust
use crate::vectoriser::FieldSet;
```

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace -- --nocapture`
Expected: New test passes. Existing tests pass (no vectoriser registered = no auto-vectorisation = passthrough path unchanged).

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/
git commit -m "feat(executor): auto-vectorise INSERT when FIELDS configured + vectoriser registered"
```

---

### Task 6: Composite Vectors

**Files:**
- Modify: `crates/trondb-core/src/executor.rs` (composite detection already in Task 5)

**Context:** Composite vectors are already handled by Task 5's auto-vectorisation: when a representation has multiple FIELDS, the vectoriser receives all contributing fields in the FieldSet, concatenates them (field-name prefixed), and produces one vector. The MockVectoriser already does this. This task adds explicit testing and ensures ReprType::Composite is set correctly.

- [ ] **Step 1: Write integration test for composite vectors**

```rust
    #[tokio::test]
    async fn insert_composite_vector_multiple_fields() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = Engine::open(test_config(dir.path())).await.unwrap();

        engine.execute("CREATE COLLECTION events (
            MODEL 'mock-model'
            FIELD name TEXT,
            FIELD description TEXT,
            FIELD category TEXT,
            REPRESENTATION semantic DIMENSIONS 8 FIELDS (name, description, category),
            REPRESENTATION name_only DIMENSIONS 8 FIELDS (name),
        );").await.unwrap();

        let mock = Arc::new(MockVectoriser::new(8));
        engine.vectoriser_registry().register("events", "semantic", mock.clone());
        engine.vectoriser_registry().register("events", "name_only", mock);

        engine.execute("INSERT INTO events (id, name, description, category) VALUES ('e1', 'Jazz Night', 'Live jazz', 'music');").await.unwrap();

        // Verify entity has both representations
        let result = engine.execute("FETCH * FROM events WHERE id = 'e1';").await.unwrap();
        assert_eq!(result.rows.len(), 1);

        // Verify the stored entity has 2 representations
        // (This requires reading the raw entity — add a helper or check via EXPLAIN)
    }
```

- [ ] **Step 2: Verify ReprType is set correctly**

In the auto-vectorise code (Task 5 Step 3), we already set:
```rust
repr_type: if stored_repr.fields.len() > 1 { ReprType::Composite } else { ReprType::Atomic },
```

Add a unit test that verifies this:
```rust
    #[tokio::test]
    async fn composite_repr_type_set_correctly() {
        // ... setup with multi-field repr ...
        // Read entity back and check repr_type
    }
```

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace -- --nocapture`
Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-core/
git commit -m "test(executor): verify composite vector generation with multiple FIELDS"
```

---

## Chunk 3: Mutation Cascade

### Task 7: Dirty Detection on UPDATE

**Files:**
- Modify: `crates/trondb-core/src/executor.rs` (UPDATE arm)

**Context:** When an entity's fields change via UPDATE, representations whose contributing fields were modified must be marked Dirty. The executor checks which fields changed, finds representations that depend on those fields, and transitions their Location Table state to Dirty. A WAL record (ReprDirty, 0x21) is written for crash recovery.

- [ ] **Step 1: Write failing test — UPDATE marks representation dirty**

```rust
    #[tokio::test]
    async fn update_marks_managed_repr_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = Engine::open(test_config(dir.path())).await.unwrap();

        engine.execute("CREATE COLLECTION events (
            MODEL 'mock-model'
            FIELD name TEXT,
            FIELD description TEXT,
            REPRESENTATION semantic DIMENSIONS 8 FIELDS (name, description),
        );").await.unwrap();

        let mock = Arc::new(MockVectoriser::new(8));
        engine.vectoriser_registry().register("events", "semantic", mock);

        // INSERT with auto-vectorise
        engine.execute("INSERT INTO events (id, name, description) VALUES ('e1', 'Jazz Night', 'Live jazz');").await.unwrap();

        // UPDATE a contributing field
        engine.execute("UPDATE 'e1' IN events SET name = 'Blues Night';").await.unwrap();

        // The representation should now be Dirty in the Location Table
        let loc = engine.location_table().get(&ReprKey {
            entity_id: LogicalId::from_string("e1"),
            repr_index: 0,
        });
        assert_eq!(loc.unwrap().state, LocState::Dirty);
    }
```

This will fail because UPDATE doesn't check representations.

- [ ] **Step 2: Add public accessor for location_table on Engine**

In `crates/trondb-core/src/lib.rs`, add:
```rust
    pub fn location_table(&self) -> &Arc<LocationTable> {
        &self.executor.location_table()
    }
```

And in Executor:
```rust
    pub fn location_table(&self) -> &Arc<LocationTable> {
        &self.location
    }
```

- [ ] **Step 3: Implement dirty detection in UPDATE executor**

In the UPDATE arm of `executor.rs` (after Step 3 "Apply assignments" and before Step 4 "WAL append"), add:

```rust
// Step 3.5: Dirty detection — check which representations are affected
let changed_fields: HashSet<&str> = p.assignments.iter()
    .map(|(f, _)| f.as_str())
    .collect();

let mut dirty_repr_indices: Vec<u32> = Vec::new();
if let Some(ref s) = schema {
    for (idx, stored_repr) in s.representations.iter().enumerate() {
        if stored_repr.fields.is_empty() {
            continue; // passthrough — never dirty from field changes
        }
        // If any contributing field was changed, mark dirty
        if stored_repr.fields.iter().any(|f| changed_fields.contains(f.as_str())) {
            dirty_repr_indices.push(idx as u32);
        }
    }
}
```

After the WAL EntityWrite, append ReprDirty records:
```rust
for repr_idx in &dirty_repr_indices {
    let loc_key = ReprKey {
        entity_id: entity_id.clone(),
        repr_index: *repr_idx,
    };
    let dirty_payload = rmp_serde::to_vec_named(&loc_key)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    self.wal.append(RecordType::ReprDirty, &p.collection, tx_id, 1, dirty_payload);
}
```

After WAL commit + Fjall persist, transition Location Table:
```rust
for repr_idx in &dirty_repr_indices {
    let loc_key = ReprKey {
        entity_id: entity_id.clone(),
        repr_index: *repr_idx,
    };
    self.location.transition(&loc_key, LocState::Dirty);
}
```

Note: `LocationTable::transition()` already exists for the Clean→Dirty transition.

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace -- --nocapture`
Expected: New dirty detection test passes. Existing UPDATE tests pass (no managed representations = no dirty marking).

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/
git commit -m "feat(executor): dirty detection on UPDATE — mark affected representations"
```

---

### Task 8: Background Recomputation

**Files:**
- Modify: `crates/trondb-core/src/lib.rs` (spawn cascade background task)
- Modify: `crates/trondb-core/src/executor.rs` (add recompute method)

**Context:** A background Tokio task periodically scans the Location Table for Dirty representations, calls the vectoriser to recompute them, writes the new vector (REPR_WRITE WAL record), and transitions the state back to Clean. Priority is by entity hotness (last_accessed), but for simplicity Phase 10 uses FIFO.

- [ ] **Step 1: Write failing test — dirty repr gets recomputed**

```rust
    #[tokio::test]
    async fn dirty_repr_recomputed_in_background() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = Engine::open(test_config(dir.path())).await.unwrap();

        // Setup collection with managed repr + mock vectoriser
        engine.execute("CREATE COLLECTION events (
            MODEL 'mock-model'
            FIELD name TEXT,
            REPRESENTATION semantic DIMENSIONS 8 FIELDS (name),
        );").await.unwrap();
        let mock = Arc::new(MockVectoriser::new(8));
        engine.vectoriser_registry().register("events", "semantic", mock);

        // INSERT
        engine.execute("INSERT INTO events (id, name) VALUES ('e1', 'Jazz');").await.unwrap();

        // UPDATE to trigger dirty
        engine.execute("UPDATE 'e1' IN events SET name = 'Blues';").await.unwrap();

        // Wait for background recomputation (or trigger manually)
        engine.trigger_cascade_recompute().await.unwrap();

        // Should be Clean again
        let loc = engine.location_table().get(&ReprKey {
            entity_id: LogicalId::from_string("e1"),
            repr_index: 0,
        });
        assert_eq!(loc.unwrap().state, LocState::Clean);
    }
```

- [ ] **Step 2: Implement recompute_dirty method on Executor**

In `crates/trondb-core/src/executor.rs`, add:

```rust
    /// Recompute all Dirty representations. Called by background task or manually.
    pub async fn recompute_dirty(&self) -> Result<usize, EngineError> {
        let dirty_keys: Vec<(ReprKey, LocationDescriptor)> = self.location
            .iter_dirty()
            .collect();

        let mut recomputed = 0;
        for (repr_key, _loc_desc) in &dirty_keys {
            // Find the entity
            // We need collection name — look up from schemas
            let collection = self.find_collection_for_entity(&repr_key.entity_id)?;
            let entity = self.store.get(&collection, &repr_key.entity_id)?;
            let schema = self.schemas.get(&collection)
                .ok_or_else(|| EngineError::CollectionNotFound(collection.clone()))?
                .clone();

            let repr_idx = repr_key.repr_index as usize;
            if repr_idx >= schema.representations.len() {
                continue;
            }
            let stored_repr = &schema.representations[repr_idx];
            if stored_repr.fields.is_empty() {
                continue; // passthrough — shouldn't be dirty, but skip
            }

            // Build FieldSet from current entity metadata
            let field_set: FieldSet = stored_repr.fields.iter()
                .filter_map(|f| {
                    entity.metadata.get(f).map(|v| (f.clone(), v.to_string()))
                })
                .collect();

            // Look up vectoriser
            let vectoriser = match self.vectoriser_registry.get(&collection, &stored_repr.name) {
                Some(v) => v,
                None => continue, // no vectoriser — can't recompute
            };

            // Transition to Recomputing
            self.location.transition(repr_key, LocState::Recomputing);

            // Encode
            let vector_data = vectoriser.encode(&field_set).await
                .map_err(|e| EngineError::Storage(format!("recompute failed: {e}")))?;

            // Update entity's representation vector
            let mut updated_entity = entity.clone();
            if repr_idx < updated_entity.representations.len() {
                updated_entity.representations[repr_idx].vector = vector_data;
                updated_entity.representations[repr_idx].state = ReprState::Clean;
            }

            // WAL: ReprWrite
            let tx_id = self.wal.next_tx_id();
            self.wal.append(RecordType::TxBegin, &collection, tx_id, 1, vec![]);
            let payload = rmp_serde::to_vec_named(&updated_entity)
                .map_err(|e| EngineError::Storage(e.to_string()))?;
            self.wal.append(RecordType::ReprWrite, &collection, tx_id, 1, payload);
            self.wal.commit(tx_id).await?;

            // Update Fjall
            self.store.insert(&collection, updated_entity.clone())?;
            self.store.persist()?;

            // Update HNSW index with new vector
            if let Some(repr) = updated_entity.representations.get(repr_idx) {
                if let VectorData::Dense(ref vec_f32) = repr.vector {
                    let hnsw_key = format!("{}:{}", collection, repr.name);
                    if let Some(hnsw) = self.indexes.get(&hnsw_key) {
                        hnsw.insert(&repr_key.entity_id, vec_f32);
                    }
                }
            }

            // Transition to Clean
            self.location.transition(repr_key, LocState::Clean);
            recomputed += 1;
        }

        Ok(recomputed)
    }
```

This requires `LocationTable::iter_dirty()` — add it:
In `crates/trondb-core/src/location.rs`:
```rust
    /// Return all representations in Dirty state.
    pub fn iter_dirty(&self) -> Vec<(ReprKey, LocationDescriptor)> {
        self.descriptors.iter()
            .filter(|entry| entry.value().state == LocState::Dirty)
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    }
```

Also add a reverse map from entity_id → collection name. The O(n*m) scan approach (iterating all schemas and checking Fjall) is too expensive for background recomputation. Add a `DashMap<LogicalId, String>` to Executor:

```rust
    // In Executor struct:
    entity_collections: DashMap<LogicalId, String>,  // entity_id → collection name
```

Populate it during INSERT (and replay). Then `find_collection_for_entity` is O(1):
```rust
    fn find_collection_for_entity(&self, entity_id: &LogicalId) -> Result<String, EngineError> {
        self.entity_collections.get(entity_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| EngineError::EntityNotFound(entity_id.to_string()))
    }
```

Add `self.entity_collections.insert(entity_id.clone(), p.collection.clone());` in the INSERT and EntityWrite replay paths.

- [ ] **Step 3: Add trigger_cascade_recompute to Engine**

In `crates/trondb-core/src/lib.rs`:
```rust
    pub async fn trigger_cascade_recompute(&self) -> Result<usize, EngineError> {
        self.executor.recompute_dirty().await
    }
```

- [ ] **Step 4: Spawn background cascade task in Engine::open**

In `Engine::open()`, after creating the executor, spawn a background task:

```rust
    let cascade_executor = /* clone/Arc of executor fields needed */;
    let _cascade_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            // recompute dirty representations
            if let Err(e) = cascade_executor.recompute_dirty().await {
                tracing::warn!("cascade recompute error: {e}");
            }
        }
    });
```

Note: The Executor needs to be shareable for this. Currently Engine owns it directly. We may need to wrap it in Arc or extract the recompute logic into a separate struct. For now, make the test use `trigger_cascade_recompute()` explicitly and defer the automatic background task to a sub-step.

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace -- --nocapture`
Expected: Dirty repr recomputation test passes.

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/
git commit -m "feat(cascade): background recomputation of dirty representations"
```

---

### Task 9: SEARCH Dirty Exclusion

**Files:**
- Modify: `crates/trondb-core/src/executor.rs` (SEARCH arm)

**Context:** Dirty representations produce stale vectors. SEARCH results should exclude entities whose relevant representation is Dirty. The entity is still queryable via FETCH (metadata is always current).

- [ ] **Step 1: Write failing test**

```rust
    #[tokio::test]
    async fn search_excludes_dirty_entities() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = Engine::open(test_config(dir.path())).await.unwrap();

        engine.execute("CREATE COLLECTION events (
            MODEL 'mock-model'
            FIELD name TEXT,
            REPRESENTATION semantic DIMENSIONS 8 FIELDS (name),
        );").await.unwrap();
        let mock = Arc::new(MockVectoriser::new(8));
        engine.vectoriser_registry().register("events", "semantic", mock.clone());

        engine.execute("INSERT INTO events (id, name) VALUES ('e1', 'Jazz Night');").await.unwrap();
        engine.execute("INSERT INTO events (id, name) VALUES ('e2', 'Rock Concert');").await.unwrap();

        // Make e1 dirty
        engine.execute("UPDATE 'e1' IN events SET name = 'Blues Night';").await.unwrap();

        // SEARCH should return only e2 (e1 is dirty)
        let query_vec = mock.encode_query("music").await.unwrap();
        // Build SEARCH with explicit vector
        let result = engine.execute("SEARCH events NEAR VECTOR [...] LIMIT 10;").await.unwrap();
        // e1 should be excluded
    }
```

(The exact vector literal will need to come from the mock — the test may need to use the engine's internal API rather than TQL for the search query vector. Alternatively, test via the recompute: insert, update, DON'T recompute, search, verify dirty excluded.)

- [ ] **Step 2: Add dirty check to SEARCH executor**

In the SEARCH arm of `executor.rs`, after collecting results from HNSW/Sparse/Hybrid, filter out entities whose **searched representation** (not all representations) is Dirty or Recomputing:

```rust
// Determine which repr_index was searched
let searched_repr_idx: Option<u32> = if let Some(schema) = self.schemas.get(&p.collection) {
    // If using_repr specified, find its index; otherwise find the repr that matches the strategy
    let repr_name = p.using_repr.as_deref()
        .or_else(|| {
            // For HNSW/NaturalLanguage: first dense repr; for Sparse: first sparse repr
            schema.representations.iter()
                .position(|r| match p.strategy {
                    SearchStrategy::Sparse => r.sparse,
                    _ => !r.sparse,
                })
                .map(|_| "") // placeholder — we want the index
        });
    schema.representations.iter()
        .position(|r| {
            if let Some(name) = p.using_repr.as_deref() {
                r.name == name
            } else {
                match p.strategy {
                    SearchStrategy::Sparse => r.sparse,
                    _ => !r.sparse && !r.fields.is_empty(), // prefer managed over passthrough
                }
            }
        })
        .map(|idx| idx as u32)
} else {
    None
};

// Filter out entities whose searched representation is Dirty/Recomputing
let results: Vec<_> = results.into_iter().filter(|(entity_id, _score)| {
    if let Some(repr_idx) = searched_repr_idx {
        let repr_key = ReprKey {
            entity_id: entity_id.clone(),
            repr_index: repr_idx,
        };
        if let Some(loc) = self.location.get(&repr_key) {
            if loc.state == LocState::Dirty || loc.state == LocState::Recomputing {
                return false; // exclude — stale vector
            }
        }
    }
    true
}).collect();
```

- [ ] **Step 3: Run tests**

Run: `cargo test --workspace -- --nocapture`
Expected: Dirty exclusion test passes. Existing SEARCH tests pass (no dirty entities in existing tests).

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "feat(search): exclude dirty/recomputing representations from SEARCH results"
```

---

## Chunk 4: Natural Language SEARCH + Vectoriser Implementations

### Task 10: Natural Language SEARCH — Parser

**Files:**
- Modify: `crates/trondb-tql/src/parser.rs`
- Modify: `crates/trondb-tql/src/ast.rs` (already done in Task 3)

**Context:** SEARCH currently requires `NEAR VECTOR [...]` or `NEAR SPARSE [...]`. This task adds `NEAR 'text query'` — the parser detects a string literal after NEAR (instead of VECTOR/SPARSE keywords) and stores it as `query_text`. Also adds `USING repr_name` to select which representation to search against.

New syntax:
```sql
SEARCH venues NEAR 'live jazz in Bristol' LIMIT 10;
SEARCH venues NEAR 'live jazz' USING semantic LIMIT 10;
```

- [ ] **Step 1: Update parse_search() to handle string literal after NEAR**

In `crates/trondb-tql/src/parser.rs`, update the `parse_search()` function. Inside the `while self.peek() == Some(&Token::Near)` loop, add a case for `StringLit`:

```rust
        while self.peek() == Some(&Token::Near) {
            self.advance(); // NEAR
            match self.peek() {
                Some(Token::Vector) => {
                    self.advance();
                    dense_vector = Some(self.parse_float_list()?);
                }
                Some(Token::Sparse) => {
                    self.advance();
                    sparse_vector = Some(self.parse_sparse_vector_list()?);
                }
                Some(Token::StringLit(_)) => {
                    if let Some((Token::StringLit(s), _)) = self.advance() {
                        query_text = Some(s);
                    }
                }
                // ... existing error handling
            }
        }
```

Add `let mut query_text = None;` and `let mut using_repr = None;` at the top.

After the NEAR loop, parse optional USING:
```rust
        let using_repr = if self.peek() == Some(&Token::Using) {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
```

Update the validation — SEARCH now requires at least one of: dense_vector, sparse_vector, or query_text:
```rust
        if dense_vector.is_none() && sparse_vector.is_none() && query_text.is_none() {
            return Err(ParseError::InvalidSyntax(
                "SEARCH requires at least one NEAR VECTOR, NEAR SPARSE, or NEAR 'text query' clause".into(),
            ));
        }
```

Update the SearchStmt construction:
```rust
        Ok(Statement::Search(SearchStmt {
            collection,
            fields: FieldList::All,
            dense_vector,
            sparse_vector,
            query_text,
            using_repr,
            filter,
            confidence,
            limit,
        }))
```

- [ ] **Step 2: Write parser tests**

```rust
    #[test]
    fn parse_search_near_text() {
        let stmt = parse("SEARCH venues NEAR 'live jazz in Bristol' LIMIT 10;").unwrap();
        match stmt {
            Statement::Search(s) => {
                assert_eq!(s.query_text.unwrap(), "live jazz in Bristol");
                assert!(s.dense_vector.is_none());
                assert!(s.sparse_vector.is_none());
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn parse_search_near_text_with_using() {
        let stmt = parse("SEARCH events NEAR 'jazz music' USING semantic LIMIT 5;").unwrap();
        match stmt {
            Statement::Search(s) => {
                assert_eq!(s.query_text.unwrap(), "jazz music");
                assert_eq!(s.using_repr.unwrap(), "semantic");
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn parse_search_near_text_with_where() {
        let stmt = parse("SEARCH venues WHERE city = 'Bristol' NEAR 'live jazz' LIMIT 10;").unwrap();
        match stmt {
            Statement::Search(s) => {
                assert!(s.filter.is_some());
                assert_eq!(s.query_text.unwrap(), "live jazz");
            }
            _ => panic!("expected Search"),
        }
    }
```

- [ ] **Step 3: Fix existing parser tests that construct SearchStmt**

Any test that pattern-matches on SearchStmt fields will need updating to account for `query_text` and `using_repr`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-tql -- --nocapture`
Expected: All parser tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-tql/
git commit -m "feat(parser): NEAR 'text query' and USING repr_name for natural language SEARCH"
```

---

### Task 11: Natural Language SEARCH — Executor + Planner

**Files:**
- Modify: `crates/trondb-core/src/planner.rs` (SearchPlan gains query_text, using_repr, new strategy)
- Modify: `crates/trondb-core/src/executor.rs` (SEARCH arm handles query_text)

**Context:** When SEARCH has a `query_text`, the executor looks up the collection's vectoriser, calls `encode_query()`, and uses the resulting vector for HNSW search. If `using_repr` is specified, search against that specific representation's HNSW index.

- [ ] **Step 1: Update SearchPlan**

In `crates/trondb-core/src/planner.rs`, add to SearchPlan:
```rust
    pub query_text: Option<String>,
    pub using_repr: Option<String>,
```

Add `NaturalLanguage` to SearchStrategy:
```rust
pub enum SearchStrategy {
    Hnsw,
    Sparse,
    Hybrid,
    NaturalLanguage,  // NEW — vectoriser encodes query text
}
```

Update `select_search_strategy()` to return NaturalLanguage when query_text is present.

Update the `plan()` function's Search arm to pass through query_text and using_repr from the AST.

- [ ] **Step 2: Implement NaturalLanguage strategy in executor**

In the SEARCH arm of `executor.rs`, add handling for `query_text`:

```rust
SearchStrategy::NaturalLanguage => {
    let query_text = p.query_text.as_ref().unwrap();

    // Determine which representation to use
    let repr_name = p.using_repr.as_ref()
        .or_else(|| {
            // Default: first managed (non-empty fields) dense representation
            schema.representations.iter()
                .find(|r| !r.fields.is_empty() && !r.sparse)
                .map(|r| &r.name)
        })
        .ok_or_else(|| EngineError::InvalidQuery(
            "no managed dense representation found for natural language SEARCH".into()
        ))?;

    // Look up vectoriser
    let vectoriser = self.vectoriser_registry
        .get(&p.collection, repr_name)
        .ok_or_else(|| EngineError::InvalidQuery(format!(
            "no vectoriser registered for {collection}:{repr_name}"
        )))?;

    // Encode query text
    let query_vector = vectoriser.encode_query(query_text).await
        .map_err(|e| EngineError::Storage(format!("encode_query failed: {e}")))?;

    // Use the encoded vector for HNSW search
    let query_f32 = match query_vector {
        VectorData::Dense(v) => v,
        _ => return Err(EngineError::InvalidQuery(
            "natural language SEARCH requires a dense vectoriser".into()
        )),
    };

    let hnsw_key = format!("{}:{}", p.collection, repr_name);
    let hnsw = self.indexes.get(&hnsw_key)
        .ok_or_else(|| EngineError::InvalidQuery(format!(
            "no HNSW index for {hnsw_key}"
        )))?;

    hnsw.search(&query_f32, fetch_k)
}
```

- [ ] **Step 3: Write integration test**

```rust
    #[tokio::test]
    async fn search_natural_language() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = Engine::open(test_config(dir.path())).await.unwrap();

        engine.execute("CREATE COLLECTION venues (
            MODEL 'mock-model'
            FIELD name TEXT,
            FIELD description TEXT,
            REPRESENTATION semantic DIMENSIONS 8 FIELDS (name, description),
        );").await.unwrap();
        let mock = Arc::new(MockVectoriser::new(8));
        engine.vectoriser_registry().register("venues", "semantic", mock);

        engine.execute("INSERT INTO venues (id, name, description) VALUES ('v1', 'Blue Note', 'Famous jazz club');").await.unwrap();
        engine.execute("INSERT INTO venues (id, name, description) VALUES ('v2', 'Rock Arena', 'Heavy metal venue');").await.unwrap();

        let result = engine.execute("SEARCH venues NEAR 'jazz music' LIMIT 10;").await.unwrap();
        assert!(!result.rows.is_empty());
    }
```

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace -- --nocapture`
Expected: All tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/
git commit -m "feat(search): natural language SEARCH via vectoriser encode_query"
```

---

### Task 12: OnnxDenseVectoriser

**Files:**
- Create: `crates/trondb-vectoriser/src/onnx_dense.rs`

**Context:** Local ONNX Runtime inference for dense embedding models (BGE, jina, nomic, etc.). Feature-gated behind `onnx`. Uses the `ort` crate for inference and `tokenizers` for text preprocessing. This is a substantial implementation — the model file path and tokenizer path are provided at construction time.

- [ ] **Step 1: Implement OnnxDenseVectoriser**

Create `crates/trondb-vectoriser/src/onnx_dense.rs`:

```rust
#[cfg(feature = "onnx")]
use async_trait::async_trait;
#[cfg(feature = "onnx")]
use ort::{Session, Value as OrtValue};
#[cfg(feature = "onnx")]
use tokenizers::Tokenizer;
#[cfg(feature = "onnx")]
use std::path::Path;

#[cfg(feature = "onnx")]
use trondb_core::types::VectorData;
#[cfg(feature = "onnx")]
use trondb_core::vectoriser::{FieldSet, VectorKind, Vectoriser, VectoriserError};

#[cfg(feature = "onnx")]
pub struct OnnxDenseVectoriser {
    id: String,
    model_id: String,
    session: Session,
    tokenizer: Tokenizer,
    dimensions: usize,
}

#[cfg(feature = "onnx")]
impl OnnxDenseVectoriser {
    pub fn new(
        model_id: &str,
        model_path: &Path,
        tokenizer_path: &Path,
        dimensions: usize,
    ) -> Result<Self, VectoriserError> {
        let session = Session::builder()
            .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?
            .commit_from_file(model_path)
            .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?;

        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?;

        Ok(Self {
            id: format!("onnx-dense:{model_id}"),
            model_id: model_id.to_string(),
            session,
            tokenizer,
            dimensions,
        })
    }

    fn encode_text(&self, text: &str) -> Result<Vec<f32>, VectoriserError> {
        let encoding = self.tokenizer.encode(text, true)
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        let input_ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
        let attention_mask: Vec<i64> = encoding.get_attention_mask().iter().map(|&m| m as i64).collect();
        let seq_len = input_ids.len();

        let input_ids_array = ndarray::Array2::from_shape_vec((1, seq_len), input_ids)
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;
        let attention_mask_array = ndarray::Array2::from_shape_vec((1, seq_len), attention_mask)
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        let outputs = self.session.run(ort::inputs![
            "input_ids" => input_ids_array,
            "attention_mask" => attention_mask_array,
        ].map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?)
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        // Extract embeddings — typically output[0] shape (1, seq_len, dims) or (1, dims)
        // Mean pooling over token dimension if needed
        let embeddings = outputs[0].try_extract_tensor::<f32>()
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        let view = embeddings.view();
        let shape = view.shape();

        let vector = if shape.len() == 3 {
            // (batch, seq_len, dims) — mean pool over seq_len
            let dims = shape[2];
            let mut pooled = vec![0.0f32; dims];
            let token_count = shape[1] as f32;
            for t in 0..shape[1] {
                for d in 0..dims {
                    pooled[d] += view[[0, t, d]];
                }
            }
            for d in &mut pooled {
                *d /= token_count;
            }
            pooled
        } else if shape.len() == 2 {
            // (batch, dims) — already pooled
            view.row(0).to_vec()
        } else {
            return Err(VectoriserError::EncodeFailed(
                format!("unexpected output shape: {shape:?}")
            ));
        };

        // L2 normalise
        let norm: f32 = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            Ok(vector.into_iter().map(|x| x / norm).collect())
        } else {
            Ok(vector)
        }
    }
}

#[cfg(feature = "onnx")]
#[async_trait]
impl Vectoriser for OnnxDenseVectoriser {
    fn id(&self) -> &str { &self.id }
    fn model_id(&self) -> &str { &self.model_id }
    fn output_size(&self) -> usize { self.dimensions }
    fn output_kind(&self) -> VectorKind { VectorKind::Dense }

    async fn encode(&self, fields: &FieldSet) -> Result<VectorData, VectoriserError> {
        // Concatenate fields with field-name prefixes for disambiguation
        let mut pairs: Vec<_> = fields.iter().collect();
        pairs.sort_by_key(|(k, _)| k.clone());
        let text: String = pairs.iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join(". ");

        let vector = self.encode_text(&text)?;
        Ok(VectorData::Dense(vector))
    }

    async fn encode_query(&self, query: &str) -> Result<VectorData, VectoriserError> {
        let vector = self.encode_text(query)?;
        Ok(VectorData::Dense(vector))
    }
}
```

Note: This requires `ndarray` as a dependency. Add to `crates/trondb-vectoriser/Cargo.toml`:
```toml
[dependencies.ndarray]
version = "0.16"
optional = true
```

Update the `onnx` feature: `onnx = ["dep:ort", "dep:tokenizers", "dep:ndarray"]`

- [ ] **Step 2: Write conditional test (only runs with onnx feature + model file)**

```rust
#[cfg(all(test, feature = "onnx"))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    #[ignore] // Requires model files — run manually with: cargo test -p trondb-vectoriser --features onnx -- --ignored
    async fn onnx_dense_encode() {
        let model_path = PathBuf::from(env!("TRONDB_TEST_MODEL_PATH"));
        let tokenizer_path = PathBuf::from(env!("TRONDB_TEST_TOKENIZER_PATH"));
        let v = OnnxDenseVectoriser::new("bge-small-en-v1.5", &model_path, &tokenizer_path, 384).unwrap();

        let fields = FieldSet::from([("name".into(), "Jazz Club".into())]);
        let result = v.encode(&fields).await.unwrap();
        match result {
            VectorData::Dense(d) => {
                assert_eq!(d.len(), 384);
                // Vector should be L2 normalised
                let norm: f32 = d.iter().map(|x| x * x).sum::<f32>().sqrt();
                assert!((norm - 1.0).abs() < 0.01);
            }
            _ => panic!("expected Dense"),
        }
    }
}
```

- [ ] **Step 3: Run tests (without onnx feature — just compilation)**

Run: `cargo test -p trondb-vectoriser -- --nocapture`
Expected: Passthrough + Mock tests pass. ONNX tests skipped (feature not enabled).

Run: `cargo check -p trondb-vectoriser --features onnx`
Expected: Compiles (may take a while to download ort).

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-vectoriser/
git commit -m "feat(vectoriser): OnnxDenseVectoriser with mean pooling and L2 normalisation"
```

---

### Task 13: OnnxSparseVectoriser

**Files:**
- Create: `crates/trondb-vectoriser/src/onnx_sparse.rs`

**Context:** Same as OnnxDense but produces sparse vectors (SPLADE-style). Output is a sparse vector of (dimension_index, weight) pairs. Feature-gated behind `onnx`.

- [ ] **Step 1: Implement OnnxSparseVectoriser**

Similar structure to OnnxDenseVectoriser but:
- Output is VectorData::Sparse
- Post-processing: ReLU + log1p on logits, then extract non-zero entries above min_weight threshold
- No L2 normalisation (sparse vectors use inner product scoring)

```rust
// Similar to onnx_dense.rs but with sparse output processing
// Key difference in encode_text():
//   1. Run model (same tokenization)
//   2. Apply ReLU + log1p to output logits
//   3. Max-pool over token dimension
//   4. Extract (dim_index, weight) pairs where weight > MIN_WEIGHT
```

- [ ] **Step 2: Write conditional test**

```rust
    #[tokio::test]
    #[ignore]
    async fn onnx_sparse_encode() {
        // Similar to dense test but checks for Sparse output
    }
```

- [ ] **Step 3: Run tests + commit**

```bash
git commit -m "feat(vectoriser): OnnxSparseVectoriser for SPLADE-style sparse models"
```

---

### Task 14: NetworkVectoriser + ExternalVectoriser

**Files:**
- Create content in: `crates/trondb-vectoriser/src/network.rs`
- Create: `crates/trondb-vectoriser/src/external.rs`

**Context:** NetworkVectoriser sends encode requests to model servers on the local network (Tier 2 — no auth). ExternalVectoriser sends to third-party APIs like OpenAI (Tier 3 — with auth). Both use HTTP via `reqwest`.

**Dependency note:** `reqwest` must be available for both NetworkVectoriser and ExternalVectoriser. Options:
- (a) Make `reqwest` a non-optional dep (simplest — both network and external need it)
- (b) Add a `network` feature alongside `external`

Recommended: make `reqwest` always available since NetworkVectoriser is not feature-gated. Update `Cargo.toml`:
```toml
[dependencies.reqwest]
version = "0.12"
features = ["json"]
```
Remove `optional = true` from reqwest. The `external` feature flag gates the `external.rs` module (auth logic), not reqwest itself.

- [ ] **Step 1: Implement NetworkVectoriser**

```rust
use async_trait::async_trait;
use trondb_core::types::VectorData;
use trondb_core::vectoriser::{FieldSet, VectorKind, Vectoriser, VectoriserError};

/// HTTP client for local network model servers (Tier 2).
/// No authentication required.
pub struct NetworkVectoriser {
    id: String,
    model_id: String,
    endpoint: String,
    dimensions: usize,
    client: reqwest::Client,
}

impl NetworkVectoriser {
    pub fn new(model_id: &str, endpoint: &str, dimensions: usize) -> Self {
        Self {
            id: format!("network:{model_id}"),
            model_id: model_id.to_string(),
            endpoint: endpoint.to_string(),
            dimensions,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Vectoriser for NetworkVectoriser {
    fn id(&self) -> &str { &self.id }
    fn model_id(&self) -> &str { &self.model_id }
    fn output_size(&self) -> usize { self.dimensions }
    fn output_kind(&self) -> VectorKind { VectorKind::Dense }

    async fn encode(&self, fields: &FieldSet) -> Result<VectorData, VectoriserError> {
        let mut pairs: Vec<_> = fields.iter().collect();
        pairs.sort_by_key(|(k, _)| k.clone());
        let text: String = pairs.iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join(". ");

        self.encode_query(&text).await
    }

    async fn encode_query(&self, query: &str) -> Result<VectorData, VectoriserError> {
        let body = serde_json::json!({
            "model": self.model_id,
            "input": query,
        });

        let resp = self.client.post(&self.endpoint)
            .json(&body)
            .send().await
            .map_err(|e| VectoriserError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(VectoriserError::Network(format!("{status}: {body}")));
        }

        let json: serde_json::Value = resp.json().await
            .map_err(|e| VectoriserError::Network(e.to_string()))?;

        // Expect {"embedding": [f32, ...]} or {"data": [{"embedding": [...]}]}
        let embedding = json.get("embedding")
            .or_else(|| json.get("data").and_then(|d| d.get(0)).and_then(|d| d.get("embedding")))
            .ok_or_else(|| VectoriserError::Network("no embedding in response".into()))?;

        let vector: Vec<f32> = embedding.as_array()
            .ok_or_else(|| VectoriserError::Network("embedding is not an array".into()))?
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect();

        Ok(VectorData::Dense(vector))
    }
}
```

- [ ] **Step 2: Implement ExternalVectoriser**

Similar but with auth header:
```rust
#[cfg(feature = "external")]
pub struct ExternalVectoriser {
    id: String,
    model_id: String,
    endpoint: String,
    dimensions: usize,
    auth_header: String,  // resolved from env var or direct value
    client: reqwest::Client,
}
```

Auth resolution: if auth starts with "env:", read from environment variable. Otherwise use as-is.

- [ ] **Step 3: Write tests (using mock HTTP server or #[ignore])**

For now, mark network tests as `#[ignore]` since they require a running server. The core logic is tested via MockVectoriser in integration tests.

- [ ] **Step 4: Run tests + commit**

```bash
git commit -m "feat(vectoriser): NetworkVectoriser (Tier 2) + ExternalVectoriser (Tier 3)"
```

---

### Task 15: CLI + Server Wiring + End-to-End Integration

**Files:**
- Modify: `crates/trondb-cli/Cargo.toml`
- Modify: `crates/trondb-cli/src/main.rs`
- Modify: `crates/trondb-server/Cargo.toml`

**Context:** The CLI and server binaries need to create vectoriser instances from collection schemas at startup. When a CREATE COLLECTION with vectoriser config is executed, the engine should register the appropriate vectoriser in the registry.

- [ ] **Step 1: Add trondb-vectoriser dependency to CLI**

In `crates/trondb-cli/Cargo.toml`:
```toml
trondb-vectoriser = { path = "../trondb-vectoriser" }
```

- [ ] **Step 2: Wire vectoriser creation into CREATE COLLECTION executor**

In the executor's CREATE COLLECTION arm, after persisting the schema, register a vectoriser for each managed representation:

```rust
// Register vectorisers for managed representations
for stored_repr in &schema_obj.representations {
    if stored_repr.fields.is_empty() {
        // Passthrough — register PassthroughVectoriser
        // (actually, passthrough doesn't need registration — encode() is never called)
        continue;
    }
    // Vectoriser creation is delegated to the binary (CLI/server) via a callback
    // For now, if a vectoriser is already registered (e.g. from startup), skip
    if !self.vectoriser_registry.has(&p.name, &stored_repr.name) {
        tracing::warn!(
            "managed representation {}.{} has no vectoriser registered — auto-vectorisation disabled until registered",
            p.name, stored_repr.name
        );
    }
}
```

The actual vectoriser instantiation (choosing ONNX vs Network vs External based on config) happens in the binary. Add a factory function to `trondb-vectoriser`:

```rust
// In crates/trondb-vectoriser/src/lib.rs:
pub fn create_vectoriser_from_config(
    config: &trondb_core::types::VectoriserConfig,
    repr: &trondb_core::types::StoredRepresentation,
) -> Result<Arc<dyn trondb_core::vectoriser::Vectoriser>, trondb_core::vectoriser::VectoriserError> {
    match config.vectoriser_type.as_deref() {
        Some("external") => {
            #[cfg(feature = "external")]
            {
                let endpoint = config.endpoint.as_deref()
                    .ok_or(VectoriserError::ModelNotLoaded("external vectoriser requires ENDPOINT".into()))?;
                let auth = config.auth.as_deref().unwrap_or("");
                let model = config.model.as_deref().unwrap_or("unknown");
                let dims = repr.dimensions.unwrap_or(0);
                Ok(Arc::new(external::ExternalVectoriser::new(model, endpoint, auth, dims)))
            }
            #[cfg(not(feature = "external"))]
            Err(VectoriserError::NotSupported("external feature not enabled".into()))
        }
        Some("network") => {
            let endpoint = config.endpoint.as_deref()
                .ok_or(VectoriserError::ModelNotLoaded("network vectoriser requires ENDPOINT".into()))?;
            let model = config.model.as_deref().unwrap_or("unknown");
            let dims = repr.dimensions.unwrap_or(0);
            Ok(Arc::new(network::NetworkVectoriser::new(model, endpoint, dims)))
        }
        _ => {
            // Default: local ONNX
            #[cfg(feature = "onnx")]
            {
                let model_path = config.model_path.as_deref()
                    .ok_or(VectoriserError::ModelNotLoaded("local vectoriser requires MODEL_PATH".into()))?;
                let model = config.model.as_deref().unwrap_or("unknown");
                let dims = repr.dimensions.unwrap_or(0);
                // Tokenizer path assumed next to model: same dir, tokenizer.json
                let model_dir = std::path::Path::new(model_path).parent().unwrap_or(std::path::Path::new("."));
                let tokenizer_path = model_dir.join("tokenizer.json");
                if repr.sparse {
                    Ok(Arc::new(onnx_sparse::OnnxSparseVectoriser::new(
                        model, std::path::Path::new(model_path), &tokenizer_path, dims
                    )?))
                } else {
                    Ok(Arc::new(onnx_dense::OnnxDenseVectoriser::new(
                        model, std::path::Path::new(model_path), &tokenizer_path, dims
                    )?))
                }
            }
            #[cfg(not(feature = "onnx"))]
            {
                // Without ONNX feature, fall back to mock in dev or error in prod
                Err(VectoriserError::NotSupported(
                    "onnx feature not enabled — cannot create local vectoriser".into()
                ))
            }
        }
    }
}
```

- [ ] **Step 3: Wire into CLI main**

In `crates/trondb-cli/src/main.rs`, after `Engine::open()`, iterate existing schemas and register vectorisers:

```rust
// Register vectorisers for existing collections with managed representations
for schema in engine.schemas() {
    if let Some(ref vc) = schema.vectoriser_config {
        for repr in &schema.representations {
            if !repr.fields.is_empty() {
                match trondb_vectoriser::create_vectoriser_from_config(vc, repr) {
                    Ok(v) => engine.vectoriser_registry().register(&schema.name, &repr.name, v),
                    Err(e) => eprintln!("warning: could not create vectoriser for {}.{}: {e}", schema.name, repr.name),
                }
            }
        }
    }
}
```

This requires `Engine::schemas()` — add a public method that returns an iterator over stored schemas.

- [ ] **Step 4: Write end-to-end integration test**

In `crates/trondb-core/src/` tests (using MockVectoriser — no ONNX required):

```rust
    #[tokio::test]
    async fn end_to_end_auto_vectorise_and_nl_search() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = Engine::open(test_config(dir.path())).await.unwrap();

        // Create collection
        engine.execute("CREATE COLLECTION venues (
            MODEL 'mock-model'
            FIELD name TEXT,
            FIELD description TEXT,
            REPRESENTATION semantic DIMENSIONS 8 FIELDS (name, description),
            REPRESENTATION identity DIMENSIONS 8,
        );").await.unwrap();

        // Register mock vectoriser
        let mock = Arc::new(trondb_vectoriser::MockVectoriser::new(8));
        engine.vectoriser_registry().register("venues", "semantic", mock);

        // INSERT with auto-vectorise (no explicit vector for 'semantic')
        // Plus explicit passthrough vector for 'identity'
        engine.execute("INSERT INTO venues (id, name, description) VALUES ('v1', 'Blue Note', 'Famous jazz club')
            REPRESENTATION identity VECTOR [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];").await.unwrap();

        engine.execute("INSERT INTO venues (id, name, description) VALUES ('v2', 'Rock Arena', 'Heavy metal venue')
            REPRESENTATION identity VECTOR [0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2];").await.unwrap();

        // Natural language SEARCH
        let result = engine.execute("SEARCH venues NEAR 'jazz music' USING semantic LIMIT 10;").await.unwrap();
        assert!(!result.rows.is_empty());

        // UPDATE triggers dirty
        engine.execute("UPDATE 'v1' IN venues SET name = 'Blue Note Jazz Club';").await.unwrap();

        // Recompute
        engine.trigger_cascade_recompute().await.unwrap();

        // Search again — v1 should be back (recomputed, clean)
        let result2 = engine.execute("SEARCH venues NEAR 'jazz club' USING semantic LIMIT 10;").await.unwrap();
        assert!(!result2.rows.is_empty());
    }
```

- [ ] **Step 5: Run full test suite**

Run: `cargo test --workspace -- --nocapture`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/
git commit -m "feat: end-to-end vectoriser wiring — CLI, factory, integration tests"
```

---

---

### Task 16: WAL Replay for ReprDirty + ReprWrite

**Files:**
- Modify: `crates/trondb-core/src/executor.rs` (replay_wal_records)

**Context:** Tasks 7-8 write `RecordType::ReprDirty` (0x21) and `RecordType::ReprWrite` (0x20) to the WAL. Without replay handlers, crash recovery would lose dirty state and recomputed vectors. The existing `replay_wal_records()` in executor.rs already handles `EntityWrite`, `EntityDelete`, etc. — this task adds handlers for the two new record types.

- [ ] **Step 1: Add ReprDirty replay handler**

In `crates/trondb-core/src/executor.rs`, in `replay_wal_records()`, add:

```rust
RecordType::ReprDirty => {
    let loc_key: ReprKey = rmp_serde::from_slice(&record.payload)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    // Mark the representation as Dirty in the Location Table
    // (if entity still exists — it may have been deleted after the dirty mark)
    if self.location.get(&loc_key).is_some() {
        self.location.transition(&loc_key, LocState::Dirty);
    }
    replayed += 1;
}
```

- [ ] **Step 2: Add ReprWrite replay handler**

```rust
RecordType::ReprWrite => {
    // ReprWrite carries the full updated entity (same as EntityWrite)
    let entity: Entity = rmp_serde::from_slice(&record.payload)
        .map_err(|e| EngineError::Storage(e.to_string()))?;
    self.store.insert(&record.collection, entity.clone())?;
    // Transition any Dirty/Recomputing reprs to Clean
    for (idx, repr) in entity.representations.iter().enumerate() {
        let loc_key = ReprKey {
            entity_id: entity.id.clone(),
            repr_index: idx as u32,
        };
        if let Some(loc) = self.location.get(&loc_key) {
            if loc.state == LocState::Dirty || loc.state == LocState::Recomputing {
                self.location.transition(&loc_key, LocState::Clean);
            }
        }
    }
    replayed += 1;
}
```

- [ ] **Step 3: Write test — crash recovery preserves dirty state**

```rust
    #[tokio::test]
    async fn wal_replay_restores_dirty_state() {
        // 1. Create engine, collection, insert entity, update to make dirty
        // 2. Drop engine (simulating crash)
        // 3. Re-open engine from same data dir
        // 4. Verify the representation is still Dirty in Location Table
    }
```

- [ ] **Step 4: Run tests + commit**

```bash
cargo test --workspace -- --nocapture
git add crates/trondb-core/
git commit -m "feat(wal): replay handlers for ReprDirty and ReprWrite records"
```

---

### Task 17: Proto/gRPC Updates for SearchPlan Changes

**Files:**
- Modify: `crates/trondb-proto/` (protobuf schema + conversion functions)

**Context:** The `trondb-proto` crate has bidirectional Plan/Statement conversions for gRPC transport. `SearchPlan` gained `query_text`, `using_repr`, and a `NaturalLanguage` strategy. The proto schema and conversion functions must be updated for multi-node SEARCH to work with natural language queries.

- [ ] **Step 1: Update proto schema**

In the `.proto` file, add `query_text` and `using_repr` fields to the SearchPlan message. Add `NATURAL_LANGUAGE = 3` to the SearchStrategy enum.

- [ ] **Step 2: Update conversion functions**

Update `From<SearchPlan> for proto::SearchPlan` and the reverse conversion to include the new fields.

- [ ] **Step 3: Run tests + commit**

```bash
cargo test -p trondb-proto -- --nocapture
git commit -m "feat(proto): add query_text, using_repr, NaturalLanguage strategy to SearchPlan"
```

---

## Post-Implementation

After all 17 tasks are complete:

1. Update `CLAUDE.md` to reflect Phase 10 completion — add `trondb-vectoriser` crate documentation
2. Run `cargo clippy --workspace` and fix warnings
3. Run `cargo test --workspace` for final verification
4. Use `superpowers:finishing-a-development-branch` skill
