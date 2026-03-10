# TronDB PoC Design Spec

**Status**: Approved
**Date**: 2026-03-10
**Scope**: Proof of concept — single-node, in-memory, no persistence

## Overview

TronDB is an inference-first storage engine. Its primary primitive is not retrieval but reasoning: it answers "what does this data imply?" rather than "what did I put in here?"

This PoC validates the core thesis: positioned entities can be stored and queried in deterministic (FETCH) and probabilistic (SEARCH) modes through a unified query language (TQL), with EXPLAIN as a first-class citizen.

## Decisions

- **Location**: Standalone project at `~/Projects/TronDB`
- **Scope**: Storage atom + in-memory store + TQL parser + HNSW index + CLI REPL
- **No**: edges, temporal model, WAL, persistence, distribution, inferred edges, composite vectors
- **Embedding**: Dimensionality-agnostic (collection declares dims at schema time). Testing with BGE-small 384-dim.
- **Approach**: Vertical slice — parser and storage built together, one verb at a time (FETCH → SEARCH → EXPLAIN)
- **Interface**: Library crate (`trondb-core`) + interactive CLI REPL (`trondb-cli`)
- **Async**: None. Synchronous only. Tokio layers on when WAL and background tasks arrive.

## Crate Structure

```
~/Projects/TronDB/
├── Cargo.toml              # workspace root
├── CLAUDE.md
├── crates/
│   ├── trondb-core/        # engine: types, storage, HNSW index, executor, planner
│   ├── trondb-tql/         # TQL parser: lexer (logos) + recursive descent → AST
│   └── trondb-cli/         # interactive REPL binary (rustyline)
```

**Dependency flow**: `trondb-cli` → `trondb-core` + `trondb-tql`. Core and TQL are independent of each other — the CLI bridges them.

## Storage Atom — Positioned Entity

```rust
struct LogicalId(Uuid);

struct Entity {
    id:               LogicalId,
    raw_data:         Bytes,                    // opaque blob, source of truth
    metadata:         HashMap<String, Value>,   // queryable structured properties
    representations:  Vec<Representation>,      // vector forms (may be empty)
    schema_version:   u32,
}

struct Representation {
    repr_type:     ReprType,          // Atomic | Composite
    fields:        Vec<String>,       // which fields contributed
    vector:        Vec<f32>,          // Float32 only for PoC
    recipe_hash:   [u8; 32],         // fingerprint of field config
    state:         ReprState,         // Clean | Dirty
}

struct Collection {
    name:          String,
    dimensions:    usize,             // e.g. 384 for BGE-small
    fields:        Vec<FieldDef>,     // schema definition
}

enum Value {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}
```

**In-memory store**: `DashMap<LogicalId, Entity>` per collection. Location Table is conceptually present but trivial — everything is Tier 0 (RAM).

**HNSW index**: One index per collection, keyed by LogicalId. Vectors added on entity insert, queried by SEARCH.

## TQL Grammar (PoC Subset)

Parser: `logos` lexer + hand-written recursive descent. No parser generator.

### AST

```rust
enum Statement {
    Fetch(FetchStatement),
    Search(SearchStatement),
    Explain(Box<Statement>),
    CreateCollection(CreateCollectionStatement),
    Insert(InsertStatement),
}

struct FetchStatement {
    collection:  String,
    fields:      FieldList,        // * or named fields
    filter:      Option<WhereClause>,
    limit:       Option<usize>,
}

struct SearchStatement {
    collection:  String,
    fields:      FieldList,
    near:        NearClause,       // vector literal
    confidence:  Option<f32>,      // threshold, default 0.0
    using:       Option<Vec<String>>,
    limit:       Option<usize>,    // default 10
}

enum WhereClause {
    Eq(String, Value),
    And(Box<WhereClause>, Box<WhereClause>),
    Or(Box<WhereClause>, Box<WhereClause>),
}
```

### Example TQL

```sql
CREATE COLLECTION venues WITH DIMENSIONS 384;

INSERT INTO venues (id, name, city)
  VALUES ('v1', 'The Jazz Cafe', 'London')
  VECTOR [0.12, -0.34, ...];

FETCH * FROM venues WHERE id = 'v1';
FETCH name, city FROM venues WHERE city = 'London';

SEARCH venues NEAR VECTOR [0.12, -0.34, ...] CONFIDENCE > 0.8 LIMIT 5;

EXPLAIN SEARCH venues NEAR VECTOR [0.12, ...] CONFIDENCE > 0.7;
```

## Query Execution Pipeline

```
TQL string → Lexer (logos) → Parser → AST → Planner → Plan → Executor → QueryResult
```

**Planner** (trivial for PoC):
- FETCH → `FetchPlan`: scan DashMap, apply WHERE filter
- SEARCH → `SearchPlan`: query HNSW index, filter by confidence threshold, resolve entities
- EXPLAIN → wraps any plan, returns plan description instead of executing

**Result type**:

```rust
struct QueryResult {
    columns:  Vec<String>,
    rows:     Vec<Row>,
    stats:    QueryStats,     // timing, entities scanned, tier (always RAM)
}
```

**EXPLAIN output** surfaces: query mode (Deterministic/Probabilistic), tier (RAM), encoding (Float32), strategy (single_pass), estimated candidates.

## CLI REPL

- `rustyline`-based interactive prompt: `trondb> `
- Multi-line input (detects incomplete statements via trailing `;`)
- Pretty-prints results as aligned tables via `comfy-table`
- Timing per query displayed after results
- Dot-commands: `.help`, `.quit`, `.collections`

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| `dashmap` | Concurrent in-memory entity store |
| `logos` | TQL lexer |
| `rustyline` | REPL |
| `uuid` | LogicalId |
| `instant-distance` | HNSW index (pure Rust, no C deps) |
| `serde` / `serde_json` | Value serialisation |
| `bytes` | raw_data blob |
| `sha2` | recipe_hash computation |
| `comfy-table` | CLI result formatting |

## What This PoC Validates

1. The positioned entity model works as a storage atom
2. Deterministic and probabilistic queries can coexist on the same data
3. TQL is parseable, familiar, and expressive enough for the three query modes
4. HNSW provides adequate recall for SEARCH at PoC scale
5. EXPLAIN surfaces useful operational information

## What This PoC Explicitly Does Not Address

- Edges (structural or inferred)
- Temporal model (bi-temporal, AS OF, VALID DURING)
- WAL / persistence / crash recovery
- Tiered memory (NVMe, SSD, Archive)
- Vector compression (MRL, RaBitQ, Int8)
- Distribution / routing / fabric
- Pluggable vectoriser engine
- Mutation cascade
- Composite vectors (schema-configured field amalgamation)
- Security / auth / multi-tenancy
