# Phase 7b: Graph Depth, Entity Lifecycle, Range Queries, Edge Decay

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete deferred capabilities — multi-hop TRAVERSE, entity deletion, range FETCH, and edge confidence decay.

**Architecture:** Four independent features building on existing scaffolding. WhereClause already has Gt/Lt variants, FieldIndex has lookup_range(), RecordType::EntityDelete exists, DecayConfig is defined. Most work is wiring existing infrastructure through the planner/executor pipeline.

**Tech Stack:** Rust 2021, Tokio, Fjall (LSM), DashMap, rmp_serde (MessagePack), hnsw_rs

---

## Context for Implementers

**Crate workspace** (all paths relative to repo root):
- `crates/trondb-tql/` — Parser. No engine dependency.
- `crates/trondb-core/` — Engine core. Depends on trondb-wal + trondb-tql.
- `crates/trondb-wal/` — Write-Ahead Log.
- `crates/trondb-routing/` — Routing layer. Depends on trondb-core + trondb-wal + trondb-tql.
- `crates/trondb-cli/` — REPL binary.

**Key patterns:**
- Write path: WAL append → Fjall apply → in-memory index update → ack
- WAL API: `wal.next_tx_id()`, `wal.append(RecordType, collection, tx_id, schema_ver, payload)`, `wal.commit(tx_id).await?`
- All executor methods are `async fn execute(&self, plan: &Plan) -> Result<QueryResult, EngineError>`
- `QueryResult { columns: Vec<String>, rows: Vec<Row>, stats: QueryStats }`
- `Row { values: HashMap<String, Value>, score: Option<f32> }`
- `QueryStats { elapsed: Duration, entities_scanned: usize, mode: QueryMode, tier: String }`
- Tests: `cargo test --workspace` (or `cargo test -p trondb-tql` etc. for single crate)
- **TQL string literals use single quotes** (e.g., `'entity_id'`, NOT `"entity_id"`). The lexer only supports `'...'`.
- HNSW indexes are keyed as `"{collection}:{repr_name}"` (e.g., `"venues:default"`), NOT just collection name
- Sparse indexes use the same key pattern: `"{collection}:{repr_name}"`
- `Entity { id: LogicalId, raw_data: Bytes, metadata: HashMap<String, Value>, representations: Vec<Representation>, schema_version: u32 }`
- `VectorData::Dense(Vec<f32>)` or `VectorData::Sparse(Vec<(u32, f32)>)`
- `LogicalId::from_string(s)`, `id.as_str()`, `id.to_string()`

**Test helper pattern:** Tests in `executor.rs` construct `Plan` structs directly. There is no `parse_and_plan` helper. To test with TQL strings, define this helper in the test module:
```rust
fn plan_from_tql(executor: &Executor, tql: &str) -> Plan {
    let stmt = trondb_tql::parse(tql).unwrap();
    crate::planner::plan(&stmt, executor.schemas()).unwrap()
}
```
Or construct `Plan::Fetch(FetchPlan { ... })` directly as the existing tests do. The plan uses `plan_from_tql(&executor, "...")` shorthand — define it once in the test module.

**Error types:**
- `EngineError::EntityNotFound(String)`, `EngineError::CollectionNotFound(String)`
- `EngineError::InvalidQuery(String)`, `EngineError::UnsupportedOperation(String)`
- `EngineError::Storage(String)`, `EngineError::EdgeTypeNotFound(String)`
- `ParseError::UnexpectedToken { pos, expected, got }`, `ParseError::UnexpectedEof(String)`

---

## Chunk 1: Range FETCH

### Task 1: Add Gte/Lte to WhereClause + Parser

**Files:**
- Modify: `crates/trondb-tql/src/ast.rs:117-124` (WhereClause enum)
- Modify: `crates/trondb-tql/src/parser.rs:733-748` (parse_where_comparison)
- Test: `crates/trondb-tql/src/parser.rs` (mod tests at bottom)

- [ ] **Step 1: Write failing tests for >= and <= parsing**

Add these tests to the `mod tests` block at the bottom of `crates/trondb-tql/src/parser.rs`:

```rust
#[test]
fn parse_fetch_gte_filter() {
    let stmt = parse("FETCH * FROM venues WHERE score >= 80;").unwrap();
    match stmt {
        Statement::Fetch(f) => {
            assert_eq!(f.filter, Some(WhereClause::Gte("score".into(), Literal::Int(80))));
        }
        _ => panic!("expected Fetch"),
    }
}

#[test]
fn parse_fetch_lte_filter() {
    let stmt = parse("FETCH * FROM venues WHERE score <= 20;").unwrap();
    match stmt {
        Statement::Fetch(f) => {
            assert_eq!(f.filter, Some(WhereClause::Lte("score".into(), Literal::Int(20))));
        }
        _ => panic!("expected Fetch"),
    }
}

#[test]
fn parse_fetch_range_and() {
    let stmt = parse("FETCH * FROM venues WHERE score >= 10 AND score <= 90;").unwrap();
    match stmt {
        Statement::Fetch(f) => match f.filter {
            Some(WhereClause::And(left, right)) => {
                assert!(matches!(*left, WhereClause::Gte(..)));
                assert!(matches!(*right, WhereClause::Lte(..)));
            }
            _ => panic!("expected And(Gte, Lte)"),
        },
        _ => panic!("expected Fetch"),
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p trondb-tql -- parse_fetch_gte parse_fetch_lte parse_fetch_range_and`
Expected: FAIL — `WhereClause::Gte` does not exist

- [ ] **Step 3: Add Gte/Lte variants to WhereClause**

In `crates/trondb-tql/src/ast.rs`, change the WhereClause enum from:

```rust
pub enum WhereClause {
    Eq(String, Literal),
    Gt(String, Literal),
    Lt(String, Literal),
    And(Box<WhereClause>, Box<WhereClause>),
    Or(Box<WhereClause>, Box<WhereClause>),
}
```

to:

```rust
pub enum WhereClause {
    Eq(String, Literal),
    Gt(String, Literal),
    Lt(String, Literal),
    Gte(String, Literal),
    Lte(String, Literal),
    And(Box<WhereClause>, Box<WhereClause>),
    Or(Box<WhereClause>, Box<WhereClause>),
}
```

- [ ] **Step 4: Wire Gte/Lte in the parser**

In `crates/trondb-tql/src/parser.rs`, in `parse_where_comparison()` (around line 735-738), change:

```rust
Some((Token::Eq, _)) => Ok(WhereClause::Eq(field, self.parse_literal()?)),
Some((Token::Gt, _)) => Ok(WhereClause::Gt(field, self.parse_literal()?)),
Some((Token::Lt, _)) => Ok(WhereClause::Lt(field, self.parse_literal()?)),
Some((tok, pos)) => Err(ParseError::UnexpectedToken {
    pos,
    expected: "comparison operator (=, >, <)".to_string(),
    got: format!("{tok:?}"),
```

to:

```rust
Some((Token::Eq, _)) => Ok(WhereClause::Eq(field, self.parse_literal()?)),
Some((Token::Gt, _)) => Ok(WhereClause::Gt(field, self.parse_literal()?)),
Some((Token::Lt, _)) => Ok(WhereClause::Lt(field, self.parse_literal()?)),
Some((Token::Gte, _)) => Ok(WhereClause::Gte(field, self.parse_literal()?)),
Some((Token::Lte, _)) => Ok(WhereClause::Lte(field, self.parse_literal()?)),
Some((tok, pos)) => Err(ParseError::UnexpectedToken {
    pos,
    expected: "comparison operator (=, >, <, >=, <=)".to_string(),
    got: format!("{tok:?}"),
```

- [ ] **Step 5: Fix non-exhaustive match in first_field_in_clause**

In `crates/trondb-core/src/planner.rs`, function `first_field_in_clause()` (around line 218-226), add the missing arms:

```rust
fn first_field_in_clause(clause: &WhereClause) -> String {
    match clause {
        WhereClause::Eq(field, _) => field.clone(),
        WhereClause::Gt(field, _) => field.clone(),
        WhereClause::Lt(field, _) => field.clone(),
        WhereClause::Gte(field, _) => field.clone(),
        WhereClause::Lte(field, _) => field.clone(),
        WhereClause::And(left, _) => first_field_in_clause(left),
        WhereClause::Or(left, _) => first_field_in_clause(left),
    }
}
```

- [ ] **Step 6: Fix non-exhaustive match in entity_matches**

In `crates/trondb-core/src/executor.rs`, function `entity_matches()` (around line 1115-1147), add Gte/Lte arms after the Lt arm:

```rust
WhereClause::Gte(field, lit) => {
    let threshold = literal_to_value(lit);
    entity
        .metadata
        .get(field)
        .map(|v| value_gte(v, &threshold))
        .unwrap_or(false)
}
WhereClause::Lte(field, lit) => {
    let threshold = literal_to_value(lit);
    entity
        .metadata
        .get(field)
        .map(|v| value_lte(v, &threshold))
        .unwrap_or(false)
}
```

And add these helper functions after `value_lt()` (around line 1169):

```rust
fn value_gte(a: &Value, b: &Value) -> bool {
    value_gt(a, b) || a == b
}

fn value_lte(a: &Value, b: &Value) -> bool {
    value_lt(a, b) || a == b
}
```

- [ ] **Step 7: Run all tests**

Run: `cargo test --workspace`
Expected: All pass (including the 3 new parser tests)

- [ ] **Step 8: Commit**

```bash
git add crates/trondb-tql/src/ast.rs crates/trondb-tql/src/parser.rs crates/trondb-core/src/planner.rs crates/trondb-core/src/executor.rs
git commit -m "feat(tql): add >= and <= comparison operators to WHERE clause"
```

---

### Task 2: FieldIndexRange Strategy + Planner Routing

**Files:**
- Modify: `crates/trondb-core/src/planner.rs` (FetchStrategy enum, select_fetch_strategy)
- Test: `crates/trondb-core/src/planner.rs` (mod tests)

- [ ] **Step 1: Write failing test**

Add to `mod tests` in `crates/trondb-core/src/planner.rs`:

```rust
#[test]
fn plan_fetch_gt_uses_field_index_range() {
    use crate::types::{Metric, StoredField, StoredIndex, StoredRepresentation, FieldType};

    let schemas = DashMap::new();
    schemas.insert("venues".into(), CollectionSchema {
        name: "venues".into(),
        representations: vec![StoredRepresentation {
            name: "default".into(),
            model: None,
            dimensions: Some(3),
            metric: Metric::Cosine,
            sparse: false,
        }],
        fields: vec![StoredField {
            name: "score".into(),
            field_type: FieldType::Int,
        }],
        indexes: vec![StoredIndex {
            name: "idx_score".into(),
            fields: vec!["score".into()],
            partial_condition: None,
        }],
    });

    let stmt = Statement::Fetch(FetchStmt {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: Some(WhereClause::Gt("score".into(), Literal::Int(50))),
        limit: Some(10),
    });
    let p = plan(&stmt, &schemas).unwrap();
    match p {
        Plan::Fetch(fp) => {
            assert_eq!(fp.strategy, FetchStrategy::FieldIndexRange("idx_score".into()));
        }
        _ => panic!("expected FetchPlan"),
    }
}

#[test]
fn plan_fetch_gte_uses_field_index_range() {
    use crate::types::{Metric, StoredField, StoredIndex, StoredRepresentation, FieldType};

    let schemas = DashMap::new();
    schemas.insert("venues".into(), CollectionSchema {
        name: "venues".into(),
        representations: vec![StoredRepresentation {
            name: "default".into(),
            model: None,
            dimensions: Some(3),
            metric: Metric::Cosine,
            sparse: false,
        }],
        fields: vec![StoredField {
            name: "score".into(),
            field_type: FieldType::Int,
        }],
        indexes: vec![StoredIndex {
            name: "idx_score".into(),
            fields: vec!["score".into()],
            partial_condition: None,
        }],
    });

    let stmt = Statement::Fetch(FetchStmt {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: Some(WhereClause::Gte("score".into(), Literal::Int(80))),
        limit: None,
    });
    let p = plan(&stmt, &schemas).unwrap();
    match p {
        Plan::Fetch(fp) => {
            assert_eq!(fp.strategy, FetchStrategy::FieldIndexRange("idx_score".into()));
        }
        _ => panic!("expected FetchPlan"),
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p trondb-core -- plan_fetch_gt_uses plan_fetch_gte_uses`
Expected: FAIL — `FieldIndexRange` does not exist

- [ ] **Step 3: Add FieldIndexRange to FetchStrategy and update planner**

In `crates/trondb-core/src/planner.rs`, change FetchStrategy:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum FetchStrategy {
    FullScan,
    FieldIndexLookup(String), // index name
    FieldIndexRange(String),  // index name
}
```

Update `select_fetch_strategy()` to handle range operators (including `And` combinations):

```rust
fn select_fetch_strategy(
    filter: &Option<WhereClause>,
    schema: Option<&CollectionSchema>,
) -> FetchStrategy {
    if let (Some(clause), Some(schema)) = (filter, schema) {
        let field_name = first_field_in_clause(clause);
        for idx in &schema.indexes {
            if idx.fields.first().map(|f| f == &field_name).unwrap_or(false) {
                return match clause {
                    WhereClause::Eq(_, _) => FetchStrategy::FieldIndexLookup(idx.name.clone()),
                    WhereClause::Gt(_, _) | WhereClause::Lt(_, _)
                    | WhereClause::Gte(_, _) | WhereClause::Lte(_, _)
                    | WhereClause::And(_, _) => FetchStrategy::FieldIndexRange(idx.name.clone()),
                    _ => FetchStrategy::FullScan,
                };
            }
        }
    }
    FetchStrategy::FullScan
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-core`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/planner.rs
git commit -m "feat(planner): add FieldIndexRange strategy for Gt/Lt/Gte/Lte filters"
```

---

### Task 3: FieldIndexRange Executor Handler

**Files:**
- Modify: `crates/trondb-core/src/executor.rs:435-511` (Plan::Fetch handler)
- Test: `crates/trondb-core/src/executor.rs` (mod tests)

- [ ] **Step 1: Write failing test**

Add to `mod tests` in `crates/trondb-core/src/executor.rs`. Use the existing `setup_executor` and `create_collection` helpers if they exist, or create a setup that:
1. Creates a collection with a `score` INT field and an index on it
2. Inserts 5 entities with scores 10, 20, 30, 40, 50
3. Fetches with `WHERE score >= 30`
4. Expects 3 results (scores 30, 40, 50)

```rust
#[tokio::test]
async fn fetch_gte_via_field_index_range() {
    let (executor, _dir) = setup_executor().await;

    // Create collection with score field + index
    executor.execute(&parse_and_plan(&executor, r#"
        CREATE COLLECTION scores {
            REPRESENTATION default DIMENSIONS 3;
            FIELD score INT;
            INDEX idx_score ON (score);
        };
    "#)).await.unwrap();

    // Insert 5 entities with different scores
    for (id, score) in [("e1", 10), ("e2", 20), ("e3", 30), ("e4", 40), ("e5", 50)] {
        executor.execute(&parse_and_plan(&executor, &format!(
            "INSERT INTO scores (id, score) VALUES ('{id}', {score})
               REPRESENTATION default VECTOR [1.0, 0.0, 0.0];"
        ))).await.unwrap();
    }

    let result = executor.execute(&parse_and_plan(&executor,
        "FETCH * FROM scores WHERE score >= 30;"
    )).await.unwrap();

    assert_eq!(result.rows.len(), 3);
}
```

Note: If `setup_executor` or `parse_and_plan` helpers don't exist with these exact names, adapt to use whatever helpers exist in the test module. The executor's `parse_and_plan` is available via `trondb_tql::parse` → `crate::planner::plan()`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p trondb-core -- fetch_gte_via_field_index_range`
Expected: FAIL — unhandled match arm for FieldIndexRange

- [ ] **Step 3: Implement FieldIndexRange handler**

In `crates/trondb-core/src/executor.rs`, in the `Plan::Fetch` match block (around line 436), add a new arm after `FieldIndexLookup`:

```rust
FetchStrategy::FieldIndexRange(index_name) => {
    let fidx_key = format!("{}:{}", p.collection, index_name);
    let fidx = self.field_indexes.get(&fidx_key)
        .ok_or_else(|| EngineError::InvalidQuery(
            format!("field index '{}' not found", index_name),
        ))?;

    // Compute range bounds from the filter
    let entity_ids = match &p.filter {
        Some(clause) => {
            let (lower, upper) = range_bounds_from_clause(clause, fidx.field_types());
            fidx.lookup_range(&lower, &upper)?
        }
        None => {
            return Err(EngineError::InvalidQuery(
                "FieldIndexRange requires a range filter".into(),
            ));
        }
    };

    let mut rows = Vec::new();
    for eid in &entity_ids {
        if let Ok(entity) = self.store.get(&p.collection, eid) {
            // Post-filter: entity_matches handles strict Gt/Lt exclusion
            if p.filter.as_ref().map(|c| entity_matches(&entity, c)).unwrap_or(true) {
                rows.push(entity_to_row(&entity, &p.fields));
            }
        }
    }
    if let Some(limit) = p.limit {
        rows.truncate(limit);
    }
    let columns = build_columns(&rows, &p.fields);
    Ok(QueryResult {
        columns,
        rows,
        stats: QueryStats {
            elapsed: start.elapsed(),
            entities_scanned: entity_ids.len(),
            mode: QueryMode::Deterministic,
            tier: "FieldIndex".into(),
        },
    })
}
```

And add the helper function `range_bounds_from_clause` near the other helper functions (after `entity_matches`):

```rust
/// Extract lower/upper bound values from a range WhereClause for FieldIndex lookup.
/// Returns (lower_bound, upper_bound) as Value slices.
/// For open-ended ranges, uses extreme values for the missing bound.
fn range_bounds_from_clause(
    clause: &WhereClause,
    field_types: &[(String, crate::types::FieldType)],
) -> (Vec<Value>, Vec<Value>) {
    let type_min_max = |ft: &crate::types::FieldType| -> (Value, Value) {
        match ft {
            crate::types::FieldType::Int => (Value::Int(i64::MIN), Value::Int(i64::MAX)),
            crate::types::FieldType::Float => (Value::Float(f64::MIN), Value::Float(f64::MAX)),
            crate::types::FieldType::Text
            | crate::types::FieldType::DateTime
            | crate::types::FieldType::EntityRef(_) => {
                (Value::String(String::new()), Value::String("\u{10FFFF}".repeat(64)))
            }
            crate::types::FieldType::Bool => (Value::Bool(false), Value::Bool(true)),
        }
    };

    let ft = field_types.first().map(|(_, t)| t).expect("field index has at least one field");
    let (type_min, type_max) = type_min_max(ft);

    match clause {
        WhereClause::Gt(_, lit) | WhereClause::Gte(_, lit) => {
            (vec![literal_to_value(lit)], vec![type_max])
        }
        WhereClause::Lt(_, lit) | WhereClause::Lte(_, lit) => {
            (vec![type_min], vec![literal_to_value(lit)])
        }
        WhereClause::And(left, right) => {
            // Combine: e.g., score >= 30 AND score <= 80
            // One side provides the lower bound (Gt/Gte), the other the upper (Lt/Lte)
            let (l_lower, l_upper) = range_bounds_from_clause(left, field_types);
            let (r_lower, r_upper) = range_bounds_from_clause(right, field_types);
            // Take the more specific bound from each side
            // For AND(Gte(30), Lte(80)): left gives (30, MAX), right gives (MIN, 80)
            // Result: lower = max(30, MIN) = 30, upper = min(MAX, 80) = 80
            let lower = match (&l_lower[0], &r_lower[0]) {
                (Value::Int(a), Value::Int(b)) => if a > b { l_lower } else { r_lower },
                (Value::Float(a), Value::Float(b)) => if a > b { l_lower } else { r_lower },
                (Value::String(a), Value::String(b)) => if a > b { l_lower } else { r_lower },
                _ => l_lower,
            };
            let upper = match (&l_upper[0], &r_upper[0]) {
                (Value::Int(a), Value::Int(b)) => if a < b { l_upper } else { r_upper },
                (Value::Float(a), Value::Float(b)) => if a < b { l_upper } else { r_upper },
                (Value::String(a), Value::String(b)) => if a < b { l_upper } else { r_upper },
                _ => l_upper,
            };
            (lower, upper)
        }
        _ => (vec![type_min], vec![type_max]),
    }
}
```

Also update the `explain_plan` function's `Plan::Fetch` arm to handle FieldIndexRange:

```rust
FetchStrategy::FieldIndexRange(index_name) => {
    props.push(("strategy", format!("FieldIndexRange ({})", index_name)));
    props.push(("tier", "FieldIndex".into()));
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "feat(executor): add FieldIndexRange handler for range FETCH queries"
```

---

### Task 4: Range FETCH Integration Tests

**Files:**
- Modify: `crates/trondb-core/src/executor.rs` (mod tests)

- [ ] **Step 1: Write comprehensive range FETCH tests**

Add to `mod tests` in `crates/trondb-core/src/executor.rs`:

```rust
#[tokio::test]
async fn fetch_gt_excludes_boundary() {
    // Setup collection with score index, insert entities with scores 10-50
    // FETCH WHERE score > 30 should return 40, 50 only (not 30)
    let (executor, _dir) = setup_executor().await;
    // ... setup as in Task 3 ...
    let result = executor.execute(&parse_and_plan(&executor,
        "FETCH * FROM scores WHERE score > 30;"
    )).await.unwrap();
    assert_eq!(result.rows.len(), 2); // 40 and 50 only
}

#[tokio::test]
async fn fetch_lt_excludes_boundary() {
    // FETCH WHERE score < 30 should return 10, 20 only
    // ... setup ...
    let result = executor.execute(&parse_and_plan(&executor,
        "FETCH * FROM scores WHERE score < 30;"
    )).await.unwrap();
    assert_eq!(result.rows.len(), 2); // 10 and 20 only
}

#[tokio::test]
async fn fetch_range_and_combination() {
    // FETCH WHERE score >= 20 AND score <= 40 should return 20, 30, 40
    // ... setup ...
    let result = executor.execute(&parse_and_plan(&executor,
        "FETCH * FROM scores WHERE score >= 20 AND score <= 40;"
    )).await.unwrap();
    assert_eq!(result.rows.len(), 3);
}

#[tokio::test]
async fn fetch_gte_lte_fullscan_fallback() {
    // Without an index, range queries should fall back to FullScan
    // Create collection WITHOUT an index, insert entities, FETCH WHERE score >= 30
    // Should still work via entity_matches FullScan path
    // ... setup without index ...
    let result = executor.execute(&parse_and_plan(&executor,
        "FETCH * FROM scores WHERE score >= 30;"
    )).await.unwrap();
    assert_eq!(result.rows.len(), 3); // 30, 40, 50
}
```

Note: Adapt these tests to use the exact helper patterns already in the test module. The key behaviour to test:
1. `>` excludes boundary value (strict greater-than)
2. `<` excludes boundary value (strict less-than)
3. `>=`/`<=` includes boundary
4. `AND` combination narrows range
5. FullScan fallback works for unindexed fields

- [ ] **Step 2: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "test: add range FETCH integration tests — boundary exclusion, AND, fullscan fallback"
```

---

## Chunk 2: Multi-hop TRAVERSE

### Task 5: BFS Multi-hop TRAVERSE

**Files:**
- Modify: `crates/trondb-core/src/executor.rs:823-860` (Plan::Traverse handler)
- Test: `crates/trondb-core/src/executor.rs` (mod tests)

- [ ] **Step 1: Write failing test for multi-hop TRAVERSE**

Create a test that builds a graph chain: A→B→C→D via edge type "knows", then traverses from A with DEPTH 3.

```rust
#[tokio::test]
async fn traverse_multi_hop_depth_3() {
    let (executor, _dir) = setup_executor().await;

    // Create two collections (people→people edge)
    executor.execute(&parse_and_plan(&executor, r#"
        CREATE COLLECTION people {
            REPRESENTATION default DIMENSIONS 3;
            FIELD name TEXT;
        };
    "#)).await.unwrap();

    // Insert 4 people
    for (id, name) in [("a", "Alice"), ("b", "Bob"), ("c", "Carol"), ("d", "Dave")] {
        executor.execute(&parse_and_plan(&executor, &format!(
            "INSERT INTO people (id, name) VALUES ('{id}', '{name}')
               REPRESENTATION default VECTOR [1.0, 0.0, 0.0];"
        ))).await.unwrap();
    }

    // Create edge type + chain: a→b→c→d
    executor.execute(&parse_and_plan(&executor,
        "CREATE EDGE knows FROM people TO people;"
    )).await.unwrap();
    for (from, to) in [("a", "b"), ("b", "c"), ("c", "d")] {
        executor.execute(&parse_and_plan(&executor, &format!(
            "INSERT EDGE knows FROM '{from}' TO '{to}';"
        ))).await.unwrap();
    }

    // Traverse depth 3 from "a" — should reach b, c, d
    let result = executor.execute(&parse_and_plan(&executor,
        "TRAVERSE knows FROM 'a' DEPTH 3;"
    )).await.unwrap();

    assert_eq!(result.rows.len(), 3); // b, c, d
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p trondb-core -- traverse_multi_hop_depth_3`
Expected: FAIL — "TRAVERSE DEPTH > 1 requires Phase 6+"

- [ ] **Step 3: Implement BFS multi-hop TRAVERSE**

Replace the `Plan::Traverse` handler in `crates/trondb-core/src/executor.rs` (around line 823-860):

```rust
Plan::Traverse(p) => {
    // Validate edge type exists
    let edge_type = self.store.get_edge_type(&p.edge_type)?;

    // Cap depth at 10
    let max_depth = p.depth.min(10);

    let from_id = LogicalId::from_string(&p.from_id);
    let mut visited: HashSet<LogicalId> = HashSet::new();
    visited.insert(from_id.clone());

    let mut frontier = vec![from_id];
    let mut rows = Vec::new();
    let limit = p.limit.unwrap_or(usize::MAX);

    for _hop in 0..max_depth {
        if frontier.is_empty() || rows.len() >= limit {
            break;
        }

        let mut next_frontier = Vec::new();

        for node_id in &frontier {
            let entries = self.adjacency.get(node_id, &p.edge_type);
            for entry in &entries {
                if visited.contains(&entry.to_id) {
                    continue;
                }
                visited.insert(entry.to_id.clone());

                if let Ok(entity) = self.store.get(&edge_type.to_collection, &entry.to_id) {
                    rows.push(entity_to_row(&entity, &FieldList::All));
                    if rows.len() >= limit {
                        break;
                    }
                }
                next_frontier.push(entry.to_id.clone());
            }
            if rows.len() >= limit {
                break;
            }
        }

        frontier = next_frontier;
    }

    let scanned = rows.len();
    Ok(QueryResult {
        columns: build_columns(&rows, &FieldList::All),
        rows,
        stats: QueryStats {
            elapsed: start.elapsed(),
            entities_scanned: scanned,
            mode: QueryMode::Deterministic,
            tier: "Ram".into(),
        },
    })
}
```

Make sure `HashSet` is imported at the top of the file (it likely already is for other uses — check `use std::collections::HashSet;`).

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "feat(executor): implement BFS multi-hop TRAVERSE with cycle detection"
```

---

### Task 6: Multi-hop TRAVERSE Edge Cases

**Files:**
- Test: `crates/trondb-core/src/executor.rs` (mod tests)

- [ ] **Step 1: Write edge case tests**

```rust
#[tokio::test]
async fn traverse_cycle_detection() {
    // Build cycle: a→b→c→a, traverse from a depth 10
    // Should return b, c only (not infinite loop, a excluded as start node)
    let (executor, _dir) = setup_executor().await;
    // ... setup collection + 3 entities + edge type + cycle edges ...
    let result = executor.execute(&parse_and_plan(&executor,
        "TRAVERSE knows FROM 'a' DEPTH 10;"
    )).await.unwrap();
    assert_eq!(result.rows.len(), 2); // b and c
}

#[tokio::test]
async fn traverse_depth_1_single_hop() {
    // Verify depth 1 still works (backward compat)
    // a→b→c, traverse from a depth 1 → only b
    // ... setup ...
    let result = executor.execute(&parse_and_plan(&executor,
        "TRAVERSE knows FROM 'a' DEPTH 1;"
    )).await.unwrap();
    assert_eq!(result.rows.len(), 1);
}

#[tokio::test]
async fn traverse_with_limit() {
    // a→b, a→c, a→d, traverse from a depth 1 limit 2
    // Should return exactly 2 results
    // ... setup ...
    let result = executor.execute(&parse_and_plan(&executor,
        "TRAVERSE knows FROM 'a' DEPTH 1 LIMIT 2;"
    )).await.unwrap();
    assert_eq!(result.rows.len(), 2);
}

#[tokio::test]
async fn traverse_depth_exceeds_graph() {
    // a→b (no more edges), traverse depth 10
    // Should return just b (no error)
    // ... setup ...
    let result = executor.execute(&parse_and_plan(&executor,
        "TRAVERSE knows FROM 'a' DEPTH 10;"
    )).await.unwrap();
    assert_eq!(result.rows.len(), 1);
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "test: add multi-hop TRAVERSE tests — cycles, depth-1 compat, limit, sparse graph"
```

---

## Chunk 3: Entity Deletion

### Task 7: AdjacencyIndex Backward Index

**Files:**
- Modify: `crates/trondb-core/src/edge.rs:59-114` (AdjacencyIndex)
- Test: `crates/trondb-core/src/edge.rs` (mod tests)

- [ ] **Step 1: Write failing tests for backward index**

Add to `mod tests` in `crates/trondb-core/src/edge.rs`:

```rust
#[test]
fn backward_index_populated_on_insert() {
    let idx = AdjacencyIndex::new();
    idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0);
    let backwards = idx.get_backward(&make_id("v2"), "knows");
    assert_eq!(backwards.len(), 1);
    assert_eq!(backwards[0], make_id("v1"));
}

#[test]
fn backward_index_remove() {
    let idx = AdjacencyIndex::new();
    idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0);
    idx.remove(&make_id("v1"), "knows", &make_id("v2"));
    let backwards = idx.get_backward(&make_id("v2"), "knows");
    assert!(backwards.is_empty());
}

#[test]
fn edges_involving_entity() {
    let idx = AdjacencyIndex::new();
    idx.insert(&make_id("v1"), "knows", &make_id("v2"), 1.0);
    idx.insert(&make_id("v3"), "knows", &make_id("v1"), 1.0);
    idx.insert(&make_id("v1"), "likes", &make_id("v4"), 1.0);

    // v1 is involved in all 3 edges
    let (forward, backward) = idx.edges_involving(&make_id("v1"));
    assert_eq!(forward.len(), 2); // knows→v2, likes→v4
    assert_eq!(backward.len(), 1); // v3→knows→v1
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p trondb-core -- backward_index edges_involving`
Expected: FAIL — methods don't exist

- [ ] **Step 3: Add backward index and new methods**

In `crates/trondb-core/src/edge.rs`, modify AdjacencyIndex:

```rust
pub struct AdjacencyIndex {
    forward: DashMap<(LogicalId, String), Vec<AdjEntry>>,
    backward: DashMap<(LogicalId, String), Vec<LogicalId>>,
}

impl AdjacencyIndex {
    pub fn new() -> Self {
        Self {
            forward: DashMap::new(),
            backward: DashMap::new(),
        }
    }

    pub fn insert(&self, from_id: &LogicalId, edge_type: &str, to_id: &LogicalId, confidence: f32) {
        let fwd_key = (from_id.clone(), edge_type.to_string());
        let entry = AdjEntry {
            to_id: to_id.clone(),
            confidence,
        };
        self.forward.entry(fwd_key).or_default().push(entry);

        // Backward index
        let bwd_key = (to_id.clone(), edge_type.to_string());
        self.backward.entry(bwd_key).or_default().push(from_id.clone());
    }

    pub fn remove(&self, from_id: &LogicalId, edge_type: &str, to_id: &LogicalId) {
        // Forward
        let fwd_key = (from_id.clone(), edge_type.to_string());
        if let Some(mut entries) = self.forward.get_mut(&fwd_key) {
            entries.retain(|e| e.to_id != *to_id);
            if entries.is_empty() {
                drop(entries);
                self.forward.remove(&fwd_key);
            }
        }

        // Backward
        let bwd_key = (to_id.clone(), edge_type.to_string());
        if let Some(mut entries) = self.backward.get_mut(&bwd_key) {
            entries.retain(|id| id != from_id);
            if entries.is_empty() {
                drop(entries);
                self.backward.remove(&bwd_key);
            }
        }
    }

    pub fn get(&self, from_id: &LogicalId, edge_type: &str) -> Vec<AdjEntry> {
        let key = (from_id.clone(), edge_type.to_string());
        self.forward.get(&key).map(|v| v.clone()).unwrap_or_default()
    }

    pub fn get_backward(&self, to_id: &LogicalId, edge_type: &str) -> Vec<LogicalId> {
        let key = (to_id.clone(), edge_type.to_string());
        self.backward.get(&key).map(|v| v.clone()).unwrap_or_default()
    }

    /// Find all edges involving an entity (as source or target).
    /// Returns (forward_edges, backward_edges) as (Vec<(edge_type, to_id)>, Vec<(edge_type, from_id)>).
    pub fn edges_involving(&self, entity_id: &LogicalId) -> (Vec<(String, LogicalId)>, Vec<(String, LogicalId)>) {
        let mut forward = Vec::new();
        for entry in self.forward.iter() {
            let (ref fwd_from, ref edge_type) = *entry.key();
            if fwd_from == entity_id {
                for adj in entry.value() {
                    forward.push((edge_type.clone(), adj.to_id.clone()));
                }
            }
        }

        let mut backward = Vec::new();
        for entry in self.backward.iter() {
            let (ref bwd_to, ref edge_type) = *entry.key();
            if bwd_to == entity_id {
                for from_id in entry.value() {
                    backward.push((edge_type.clone(), from_id.clone()));
                }
            }
        }

        (forward, backward)
    }

    pub fn len(&self) -> usize {
        self.forward.iter().map(|e| e.value().len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/edge.rs
git commit -m "feat(edge): add backward index to AdjacencyIndex for entity cascade deletion"
```

---

### Task 8: DELETE Statement — TQL Parser + AST

**Files:**
- Modify: `crates/trondb-tql/src/ast.rs` (DeleteStmt, Statement::Delete)
- Modify: `crates/trondb-tql/src/token.rs` (Delete token — check if it exists)
- Modify: `crates/trondb-tql/src/parser.rs` (parse_delete)
- Test: `crates/trondb-tql/src/parser.rs` (mod tests)

- [ ] **Step 1: Write failing parser tests**

```rust
#[test]
fn parse_delete_entity() {
    let stmt = parse("DELETE 'e1' FROM venues;").unwrap();
    match stmt {
        Statement::Delete(d) => {
            assert_eq!(d.entity_id, "e1");
            assert_eq!(d.collection, "venues");
        }
        _ => panic!("expected Delete"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p trondb-tql -- parse_delete_entity`
Expected: FAIL — Statement::Delete doesn't exist

- [ ] **Step 3: Add DeleteStmt to AST**

In `crates/trondb-tql/src/ast.rs`, add after `DeleteEdgeStmt`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    pub entity_id: String,
    pub collection: String,
}
```

Add `Delete(DeleteStmt)` to the `Statement` enum.

- [ ] **Step 4: Add Delete token if needed and implement parser**

Check if `Token::Delete` exists in `crates/trondb-tql/src/token.rs`. It likely does (used for DELETE EDGE). If so, no token changes needed.

In `crates/trondb-tql/src/parser.rs`, the DELETE keyword currently routes to `parse_delete_edge()`. Modify the DELETE handling to check what follows:
- If next token is `EDGE` → `parse_delete_edge()` (existing)
- If next token is a string literal → `parse_delete_entity()` (new)

Add the parse_delete_entity method:

```rust
fn parse_delete_entity(&mut self) -> Result<Statement, ParseError> {
    // DELETE 'entity_id' FROM collection;
    let entity_id = match self.advance() {
        Some((Token::StringLit(s), _)) => s,
        Some((tok, pos)) => return Err(ParseError::UnexpectedToken {
            pos,
            expected: "entity ID string".to_string(),
            got: format!("{tok:?}"),
        }),
        None => return Err(ParseError::UnexpectedEof("expected entity ID".to_string())),
    };
    self.expect(&Token::From)?;
    let collection = self.expect_ident()?;
    self.expect(&Token::Semicolon)?;
    Ok(Statement::Delete(DeleteStmt { entity_id, collection }))
}
```

Update the DELETE dispatch to peek at the next token and route accordingly.

- [ ] **Step 5: Re-export DeleteStmt from trondb-tql lib.rs**

In `crates/trondb-tql/src/lib.rs`, ensure `DeleteStmt` is re-exported (follow existing pattern — check if types are re-exported via `pub use ast::*;`).

- [ ] **Step 6: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 7: Commit**

```bash
git add crates/trondb-tql/src/ast.rs crates/trondb-tql/src/parser.rs crates/trondb-tql/src/token.rs crates/trondb-tql/src/lib.rs
git commit -m "feat(tql): add DELETE entity statement parsing"
```

---

### Task 9: FjallStore delete_entity Method

**Files:**
- Modify: `crates/trondb-core/src/store.rs` (new method)
- Test: `crates/trondb-core/src/store.rs` (mod tests)

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn delete_entity_removes_from_fjall() {
    let (store, _dir) = open_store();
    let schema = make_schema("venues", 3);
    store.create_collection_schema(&schema).unwrap();

    let entity = Entity::new(LogicalId::from_string("e1"))
        .with_metadata("name", Value::String("Test".into()));
    store.insert("venues", entity).unwrap();
    store.persist().unwrap();

    store.delete_entity("venues", &LogicalId::from_string("e1")).unwrap();
    store.persist().unwrap();

    assert!(store.get("venues", &LogicalId::from_string("e1")).is_err());
}
```

Note: Uses existing `open_store()` and `make_schema()` helpers from the store test module.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p trondb-core -- delete_entity_removes`
Expected: FAIL — `delete_entity` doesn't exist

- [ ] **Step 3: Implement delete_entity**

In `crates/trondb-core/src/store.rs`, add:

```rust
pub fn delete_entity(&self, collection: &str, id: &LogicalId) -> Result<(), EngineError> {
    if !self.has_collection(collection) {
        return Err(EngineError::CollectionNotFound(collection.to_owned()));
    }

    let partition = self
        .keyspace
        .open_partition(collection, PartitionCreateOptions::default())
        .map_err(|e| EngineError::Storage(e.to_string()))?;

    let key = format!("{ENTITY_PREFIX}{id}");
    partition
        .remove(&key)
        .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?;

    Ok(())
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/store.rs
git commit -m "feat(store): add delete_entity method for Fjall entity removal"
```

---

### Task 10: DeleteEntity Plan + Executor Handler

**Files:**
- Modify: `crates/trondb-core/src/planner.rs` (DeleteEntityPlan, Plan::DeleteEntity)
- Modify: `crates/trondb-core/src/executor.rs` (DeleteEntity handler)
- Test: `crates/trondb-core/src/executor.rs` (mod tests)

- [ ] **Step 1: Write failing test**

```rust
#[tokio::test]
async fn delete_entity_cascading() {
    let (executor, _dir) = setup_executor().await;

    // Create collection, insert entity, delete, verify gone
    // ... setup collection + entity ...

    let result = executor.execute(&parse_and_plan(&executor,
        "DELETE 'e1' FROM venues;"
    )).await.unwrap();

    // Verify entity is gone
    let fetch = executor.execute(&parse_and_plan(&executor,
        "FETCH * FROM venues;"
    )).await.unwrap();
    assert_eq!(fetch.rows.len(), 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL — Plan::DeleteEntity doesn't exist

- [ ] **Step 3: Add DeleteEntityPlan to planner**

In `crates/trondb-core/src/planner.rs`:

Add the plan struct:
```rust
#[derive(Debug, Clone)]
pub struct DeleteEntityPlan {
    pub entity_id: String,
    pub collection: String,
}
```

Add `DeleteEntity(DeleteEntityPlan)` to the `Plan` enum.

Add the match arm in `plan()`:
```rust
Statement::Delete(s) => Ok(Plan::DeleteEntity(DeleteEntityPlan {
    entity_id: s.entity_id.clone(),
    collection: s.collection.clone(),
})),
```

- [ ] **Step 4: Implement DeleteEntity executor handler**

In `crates/trondb-core/src/executor.rs`, add a new match arm in `execute()`:

```rust
Plan::DeleteEntity(p) => {
    let entity_id = LogicalId::from_string(&p.entity_id);

    // Step 1: Read entity before deletion (needed for index cleanup)
    let entity = self.store.get(&p.collection, &entity_id)?;

    // Step 2: WAL log
    let tx_id = self.wal.next_tx_id();
    self.wal.append(RecordType::TxBegin, &p.collection, tx_id, 1, vec![]);
    #[derive(serde::Serialize)]
    struct EntityDeletePayload<'a> {
        entity_id: &'a str,
        collection: &'a str,
    }
    let payload = rmp_serde::to_vec_named(&EntityDeletePayload {
        entity_id: &p.entity_id,
        collection: &p.collection,
    })
    .map_err(|e| EngineError::Storage(e.to_string()))?;
    self.wal.append(RecordType::EntityDelete, &p.collection, tx_id, 1, payload);
    self.wal.commit(tx_id).await?;

    // Step 3: HNSW tombstone — indexes keyed as "{collection}:{repr_name}"
    let hnsw_prefix = format!("{}:", p.collection);
    for entry in self.indexes.iter() {
        if entry.key().starts_with(&hnsw_prefix) {
            entry.value().remove(&entity_id);
        }
    }

    // Step 4: Field index removal
    let schema = self.schemas.get(&p.collection);
    if let Some(schema) = &schema {
        for idx_def in &schema.indexes {
            let fidx_key = format!("{}:{}", p.collection, idx_def.name);
            if let Some(fidx) = self.field_indexes.get(&fidx_key) {
                let values: Vec<Value> = idx_def.fields.iter()
                    .map(|f| entity.metadata.get(f).cloned().unwrap_or(Value::Null))
                    .collect();
                let _ = fidx.remove(&entity_id, &values);
            }
        }
    }

    // Step 5: Sparse index removal — keyed as "{collection}:{repr_name}"
    for repr in &entity.representations {
        if let crate::types::VectorData::Sparse(ref sv) = repr.vector {
            let sparse_key = format!("{}:{}", p.collection, repr.name);
            if let Some(sparse_idx) = self.sparse_indexes.get(&sparse_key) {
                sparse_idx.remove(&entity_id, sv);
            }
        }
    }

    // Step 6: Location table removal (remove_entity removes all reprs at once)
    self.location.remove_entity(&entity_id);

    // Step 7: Fjall delete
    self.store.delete_entity(&p.collection, &entity_id)?;

    // Step 8: Edge cleanup — find and delete all edges involving this entity
    let (forward_edges, backward_edges) = self.adjacency.edges_involving(&entity_id);
    for (edge_type, to_id) in &forward_edges {
        self.store.delete_edge(edge_type, entity_id.as_str(), to_id.as_str())?;
        self.adjacency.remove(&entity_id, edge_type, to_id);
    }
    for (edge_type, from_id) in &backward_edges {
        self.store.delete_edge(edge_type, from_id.as_str(), entity_id.as_str())?;
        self.adjacency.remove(from_id, edge_type, &entity_id);
    }

    // Step 9: Tiered storage cleanup
    use crate::location::Tier;
    let _ = self.store.delete_from_tier(&p.collection, &entity_id, Tier::NVMe);
    let _ = self.store.delete_from_tier(&p.collection, &entity_id, Tier::Archive);

    self.store.persist()?;

    Ok(QueryResult {
        columns: vec!["result".into()],
        rows: vec![Row {
            values: HashMap::from([(
                "result".into(),
                Value::String(format!("Entity '{}' deleted from '{}'", p.entity_id, p.collection)),
            )]),
            score: None,
        }],
        stats: QueryStats {
            elapsed: start.elapsed(),
            entities_scanned: 1,
            mode: QueryMode::Deterministic,
            tier: "Fjall".into(),
        },
    })
}
```

Also add `DeleteEntity` to `explain_plan()`:
```rust
Plan::DeleteEntity(p) => {
    props.push(("mode", "Deterministic".into()));
    props.push(("verb", "DELETE".into()));
    props.push(("entity_id", p.entity_id.clone()));
    props.push(("collection", p.collection.clone()));
    props.push(("tier", "Fjall".into()));
}
```

`LocationTable::remove_entity(&LogicalId)` exists at `location.rs:223` — removes all representations for an entity from the DashMap.

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/src/planner.rs crates/trondb-core/src/executor.rs
git commit -m "feat(executor): implement cascading DELETE entity with index cleanup"
```

---

### Task 11: Entity Deletion Integration Tests

**Files:**
- Test: `crates/trondb-core/src/executor.rs` (mod tests)

- [ ] **Step 1: Write comprehensive deletion tests**

```rust
#[tokio::test]
async fn delete_nonexistent_entity_errors() {
    // DELETE of entity that doesn't exist should return EntityNotFound
    let (executor, _dir) = setup_executor().await;
    // ... setup collection ...
    let result = executor.execute(&parse_and_plan(&executor,
        "DELETE 'nonexistent' FROM venues;"
    )).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn delete_entity_removes_from_hnsw() {
    // Insert entity, verify it appears in SEARCH, delete, verify gone from SEARCH
    // ... setup + insert + search (should find) + delete + search (should not find) ...
}

#[tokio::test]
async fn delete_entity_cascades_edges() {
    // a→b edge, delete b, verify edge is gone
    // ... setup + edge + delete b + traverse from a (should be empty) ...
}

#[tokio::test]
async fn delete_entity_removes_from_field_index() {
    // Insert with indexed field, FETCH by field (finds it), delete, FETCH again (empty)
    // ... setup ...
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "test: add entity deletion integration tests — errors, HNSW, edges, field index"
```

---

## Chunk 4: Edge Decay

### Task 12: Edge created_at + AdjEntry Extension

**Files:**
- Modify: `crates/trondb-core/src/edge.rs` (Edge struct, AdjEntry struct)
- Modify: `crates/trondb-core/src/executor.rs` (InsertEdge handler — set created_at)
- Test: `crates/trondb-core/src/edge.rs` (mod tests)

- [ ] **Step 1: Write test for created_at on Edge**

```rust
#[test]
fn edge_created_at_default_zero() {
    // Deserialise an edge without created_at — should default to 0
    let edge = Edge {
        from_id: make_id("a"),
        to_id: make_id("b"),
        edge_type: "knows".into(),
        confidence: 1.0,
        metadata: HashMap::new(),
        created_at: 0,
    };
    let bytes = rmp_serde::to_vec_named(&edge).unwrap();
    let restored: Edge = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(restored.created_at, 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL — Edge has no `created_at` field

- [ ] **Step 3: Add created_at to Edge and AdjEntry**

In `crates/trondb-core/src/edge.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from_id: LogicalId,
    pub to_id: LogicalId,
    pub edge_type: String,
    pub confidence: f32,
    pub metadata: HashMap<String, Value>,
    #[serde(default)]
    pub created_at: u64,
}
```

```rust
#[derive(Debug, Clone)]
pub struct AdjEntry {
    pub to_id: LogicalId,
    pub confidence: f32,
    pub created_at: u64,
}
```

Update `AdjacencyIndex::insert()` to accept created_at:

```rust
pub fn insert(&self, from_id: &LogicalId, edge_type: &str, to_id: &LogicalId, confidence: f32, created_at: u64) {
    let fwd_key = (from_id.clone(), edge_type.to_string());
    let entry = AdjEntry {
        to_id: to_id.clone(),
        confidence,
        created_at,
    };
    self.forward.entry(fwd_key).or_default().push(entry);
    let bwd_key = (to_id.clone(), edge_type.to_string());
    self.backward.entry(bwd_key).or_default().push(from_id.clone());
}
```

- [ ] **Step 4: Fix all callers of AdjacencyIndex::insert**

The signature changed — now requires `created_at`. Search for all calls:
- `executor.rs` InsertEdge handler: `self.adjacency.insert(&from_id, &p.edge_type, &to_id, 1.0)` → add created_at timestamp
- `executor.rs` or `lib.rs` startup rebuild: when rebuilding adjacency from Fjall edges on startup
- Edge tests in `edge.rs`

For InsertEdge in executor.rs, set `created_at` on the Edge struct and pass to adjacency:

```rust
let now_millis = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_millis() as u64;

let edge = Edge {
    from_id: from_id.clone(),
    to_id: to_id.clone(),
    edge_type: p.edge_type.clone(),
    confidence: 1.0,
    metadata,
    created_at: now_millis,
};
// ...
self.adjacency.insert(&from_id, &p.edge_type, &to_id, 1.0, now_millis);
```

For startup rebuild (in `lib.rs` or wherever edges are loaded): use `edge.created_at` from the deserialised edge.

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace`
Expected: All pass (existing tests updated to pass 0 for created_at where needed)

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/src/edge.rs crates/trondb-core/src/executor.rs crates/trondb-core/src/lib.rs
git commit -m "feat(edge): add created_at timestamp to Edge + AdjEntry for decay support"
```

---

### Task 13: Decay Functions + TRAVERSE Integration

**Files:**
- Modify: `crates/trondb-core/src/edge.rs` (decay computation functions)
- Modify: `crates/trondb-core/src/executor.rs` (TRAVERSE applies decay filtering)
- Test: `crates/trondb-core/src/edge.rs` (mod tests)

- [ ] **Step 1: Write decay function tests**

```rust
#[test]
fn exponential_decay() {
    let config = DecayConfig {
        decay_fn: Some(DecayFn::Exponential),
        decay_rate: Some(0.001), // per second
        floor: Some(0.1),
        promote_threshold: None,
        prune_threshold: Some(0.05),
    };
    // After 1000 seconds: e^(-0.001 * 1000) = e^(-1) ≈ 0.368
    let result = effective_confidence(1.0, 1000_000, &config); // 1000 seconds in millis
    assert!((result - 0.368).abs() < 0.01);
}

#[test]
fn linear_decay() {
    let config = DecayConfig {
        decay_fn: Some(DecayFn::Linear),
        decay_rate: Some(0.001),
        floor: Some(0.1),
        promote_threshold: None,
        prune_threshold: None,
    };
    // After 500 seconds: 1.0 * (1 - 0.001 * 500) = 0.5
    let result = effective_confidence(1.0, 500_000, &config);
    assert!((result - 0.5).abs() < 0.01);
}

#[test]
fn linear_decay_respects_floor() {
    let config = DecayConfig {
        decay_fn: Some(DecayFn::Linear),
        decay_rate: Some(0.01),
        floor: Some(0.2),
        promote_threshold: None,
        prune_threshold: None,
    };
    // After 1000 seconds: 1.0 * (1 - 0.01*1000) = -9.0, clamped to floor 0.2
    let result = effective_confidence(1.0, 1_000_000, &config);
    assert!((result - 0.2).abs() < 0.01);
}

#[test]
fn step_decay() {
    let config = DecayConfig {
        decay_fn: Some(DecayFn::Step),
        decay_rate: Some(3600.0), // threshold: 3600 seconds
        floor: Some(0.1),
        promote_threshold: None,
        prune_threshold: None,
    };
    // Before threshold (1000s) → original confidence
    assert!((effective_confidence(1.0, 1_000_000, &config) - 1.0).abs() < 0.01);
    // After threshold (5000s) → floor
    assert!((effective_confidence(1.0, 5_000_000, &config) - 0.1).abs() < 0.01);
}

#[test]
fn no_decay_config_returns_original() {
    let config = DecayConfig::default(); // all None
    let result = effective_confidence(1.0, 999_999_999, &config);
    assert!((result - 1.0).abs() < 0.001);
}

#[test]
fn zero_created_at_no_decay() {
    let config = DecayConfig {
        decay_fn: Some(DecayFn::Exponential),
        decay_rate: Some(0.1),
        floor: Some(0.0),
        promote_threshold: None,
        prune_threshold: None,
    };
    // created_at=0 means legacy edge — no decay applied
    // We test this via the elapsed_millis=0 path
    let result = effective_confidence(1.0, 0, &config);
    assert!((result - 1.0).abs() < 0.001);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Expected: FAIL — `effective_confidence` doesn't exist

- [ ] **Step 3: Implement decay functions**

In `crates/trondb-core/src/edge.rs`, add:

```rust
/// Compute effective confidence after decay.
/// `base_confidence`: original confidence (typically 1.0)
/// `elapsed_millis`: time since edge creation in milliseconds
/// `config`: the edge type's decay configuration
pub fn effective_confidence(base_confidence: f32, elapsed_millis: u64, config: &DecayConfig) -> f32 {
    if elapsed_millis == 0 || config.decay_fn.is_none() {
        return base_confidence;
    }

    let elapsed_secs = elapsed_millis as f64 / 1000.0;
    let rate = config.decay_rate.unwrap_or(0.0);
    let floor = config.floor.unwrap_or(0.0);

    let decayed = match config.decay_fn.as_ref().unwrap() {
        DecayFn::Exponential => (base_confidence as f64) * (-rate * elapsed_secs).exp(),
        DecayFn::Linear => (base_confidence as f64) * (1.0 - rate * elapsed_secs),
        DecayFn::Step => {
            if elapsed_secs > rate {
                floor
            } else {
                base_confidence as f64
            }
        }
    };

    decayed.max(floor).min(1.0) as f32
}
```

- [ ] **Step 4: Integrate decay into TRAVERSE**

In `crates/trondb-core/src/executor.rs`, in the TRAVERSE handler's inner loop, after getting `entries` from the adjacency index, apply decay filtering:

```rust
for entry in &entries {
    if visited.contains(&entry.to_id) {
        continue;
    }

    // Apply edge decay if edge type has decay config
    let effective_conf = if entry.created_at > 0 {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let elapsed = now_ms.saturating_sub(entry.created_at);
        crate::edge::effective_confidence(entry.confidence, elapsed, &edge_type.decay_config)
    } else {
        entry.confidence
    };

    // Skip edges below prune threshold
    if let Some(prune) = edge_type.decay_config.prune_threshold {
        if effective_conf < prune as f32 {
            continue;
        }
    }

    visited.insert(entry.to_id.clone());
    // ... rest of entity fetch + row building ...
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/src/edge.rs crates/trondb-core/src/executor.rs
git commit -m "feat: add edge decay functions + integrate into TRAVERSE filtering"
```

---

### Task 14: CREATE EDGE DECAY Syntax

**Files:**
- Modify: `crates/trondb-tql/src/ast.rs` (CreateEdgeTypeStmt)
- Modify: `crates/trondb-tql/src/token.rs` (Decay, Rate, Floor, Prune tokens if needed)
- Modify: `crates/trondb-tql/src/parser.rs` (parse_create_edge)
- Modify: `crates/trondb-core/src/planner.rs` (CreateEdgeTypePlan)
- Modify: `crates/trondb-core/src/executor.rs` (CreateEdgeType handler)
- Test: `crates/trondb-tql/src/parser.rs` (mod tests)

- [ ] **Step 1: Write failing parser test**

```rust
#[test]
fn parse_create_edge_with_decay() {
    let stmt = parse(
        "CREATE EDGE knows FROM people TO people DECAY EXPONENTIAL RATE 0.001 FLOOR 0.1 PRUNE 0.05;"
    ).unwrap();
    match stmt {
        Statement::CreateEdgeType(s) => {
            assert_eq!(s.name, "knows");
            let dc = s.decay_config.unwrap();
            assert!(matches!(dc.decay_fn, Some(DecayFnDecl::Exponential)));
            assert!((dc.decay_rate.unwrap() - 0.001).abs() < 1e-6);
            assert!((dc.floor.unwrap() - 0.1).abs() < 1e-6);
            assert!((dc.prune_threshold.unwrap() - 0.05).abs() < 1e-6);
        }
        _ => panic!("expected CreateEdgeType"),
    }
}

#[test]
fn parse_create_edge_without_decay() {
    // Existing syntax should still work
    let stmt = parse("CREATE EDGE likes FROM people TO people;").unwrap();
    match stmt {
        Statement::CreateEdgeType(s) => {
            assert_eq!(s.name, "likes");
            assert!(s.decay_config.is_none());
        }
        _ => panic!("expected CreateEdgeType"),
    }
}
```

Note: `DecayFn` is from `trondb_core::edge`, but the parser crate (`trondb-tql`) has no dependency on core. You'll need to define a parallel `DecayFn` enum in the AST, or define a simpler representation. The cleanest approach: add `DecayFn` and `DecayConfigDecl` to `trondb-tql/src/ast.rs` (parser-level types), and convert in the planner.

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL — `decay_config` field doesn't exist on CreateEdgeTypeStmt

- [ ] **Step 3: Add AST types for decay**

In `crates/trondb-tql/src/ast.rs`, add:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum DecayFnDecl {
    Exponential,
    Linear,
    Step,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecayConfigDecl {
    pub decay_fn: Option<DecayFnDecl>,
    pub decay_rate: Option<f64>,
    pub floor: Option<f64>,
    pub promote_threshold: Option<f64>,
    pub prune_threshold: Option<f64>,
}
```

Modify `CreateEdgeTypeStmt`:
```rust
#[derive(Debug, Clone, PartialEq)]
pub struct CreateEdgeTypeStmt {
    pub name: String,
    pub from_collection: String,
    pub to_collection: String,
    pub decay_config: Option<DecayConfigDecl>,
}
```

- [ ] **Step 4: Add tokens (Decay, Rate, Floor, Prune)**

In `crates/trondb-tql/src/token.rs`, add if not present:

```rust
#[token("DECAY", priority = 10, ignore(ascii_case))]
Decay,

#[token("RATE", priority = 10, ignore(ascii_case))]
Rate,

#[token("FLOOR", priority = 10, ignore(ascii_case))]
TokenFloor,

#[token("PRUNE", priority = 10, ignore(ascii_case))]
Prune,

#[token("EXPONENTIAL", priority = 10, ignore(ascii_case))]
Exponential,

#[token("LINEAR", priority = 10, ignore(ascii_case))]
Linear,

#[token("STEP", priority = 10, ignore(ascii_case))]
Step,
```

Each keyword needs `priority = 10` to beat the `Ident` regex (priority 1). Use `TokenFloor` to avoid Rust name conflicts (follows existing convention: `TokenBool`, etc.).

- [ ] **Step 5: Update parser**

Modify `parse_create_edge()` in `crates/trondb-tql/src/parser.rs`:

```rust
fn parse_create_edge(&mut self) -> Result<Statement, ParseError> {
    self.advance(); // EDGE
    let name = self.expect_ident()?;
    self.expect(&Token::From)?;
    let from_collection = self.expect_ident()?;
    self.expect(&Token::To)?;
    let to_collection = self.expect_ident()?;

    // Optional DECAY clause
    let decay_config = if self.peek() == Some(&Token::Decay) {
        self.advance(); // DECAY
        let decay_fn = match self.advance() {
            Some((Token::Exponential, _)) => DecayFnDecl::Exponential,
            Some((Token::Linear, _)) => DecayFnDecl::Linear,
            Some((Token::Step, _)) => DecayFnDecl::Step,
            Some((tok, pos)) => return Err(ParseError::UnexpectedToken {
                pos,
                expected: "EXPONENTIAL, LINEAR, or STEP".to_string(),
                got: format!("{tok:?}"),
            }),
            None => return Err(ParseError::UnexpectedEof("expected decay function".to_string())),
        };

        let mut rate = None;
        let mut floor = None;
        let mut prune = None;

        // Parse optional RATE, FLOOR, PRUNE in any order
        loop {
            match self.peek() {
                Some(&Token::Rate) => {
                    self.advance();
                    rate = Some(self.expect_float_or_int()?);
                }
                Some(&Token::TokenFloor) => {
                    self.advance();
                    floor = Some(self.expect_float_or_int()?);
                }
                Some(&Token::Prune) => {
                    self.advance();
                    prune = Some(self.expect_float_or_int()?);
                }
                _ => break,
            }
        }

        Some(DecayConfigDecl {
            decay_fn: Some(decay_fn),
            decay_rate: rate,
            floor,
            promote_threshold: None,
            prune_threshold: prune,
        })
    } else {
        None
    };

    self.expect(&Token::Semicolon)?;
    Ok(Statement::CreateEdgeType(CreateEdgeTypeStmt {
        name,
        from_collection,
        to_collection,
        decay_config,
    }))
}
```

You may need a helper `expect_float_or_int()`:
```rust
fn expect_float_or_int(&mut self) -> Result<f64, ParseError> {
    match self.advance() {
        Some((Token::FloatLit(f), _)) => Ok(f),
        Some((Token::IntLit(n), _)) => Ok(n as f64),
        Some((tok, pos)) => Err(ParseError::UnexpectedToken {
            pos,
            expected: "number".to_string(),
            got: format!("{tok:?}"),
        }),
        None => Err(ParseError::UnexpectedEof("expected number".to_string())),
    }
}
```

- [ ] **Step 6: Update planner to pass decay config**

In `crates/trondb-core/src/planner.rs`, add `decay_config` to `CreateEdgeTypePlan`:

```rust
pub struct CreateEdgeTypePlan {
    pub name: String,
    pub from_collection: String,
    pub to_collection: String,
    pub decay_config: Option<trondb_tql::DecayConfigDecl>,
}
```

Update the `Statement::CreateEdgeType` match arm to pass it through.

- [ ] **Step 7: Update executor to convert DecayConfigDecl → DecayConfig**

In the `Plan::CreateEdgeType` handler in `crates/trondb-core/src/executor.rs`, convert the AST decay config to core DecayConfig:

```rust
let decay_config = p.decay_config.as_ref().map(|dc| {
    crate::edge::DecayConfig {
        decay_fn: dc.decay_fn.as_ref().map(|f| match f {
            trondb_tql::DecayFnDecl::Exponential => crate::edge::DecayFn::Exponential,
            trondb_tql::DecayFnDecl::Linear => crate::edge::DecayFn::Linear,
            trondb_tql::DecayFnDecl::Step => crate::edge::DecayFn::Step,
        }),
        decay_rate: dc.decay_rate,
        floor: dc.floor,
        promote_threshold: dc.promote_threshold,
        prune_threshold: dc.prune_threshold,
    }
}).unwrap_or_default();
```

Use this `decay_config` when constructing the `EdgeType` struct.

- [ ] **Step 8: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 9: Commit**

```bash
git add crates/trondb-tql/src/ast.rs crates/trondb-tql/src/token.rs crates/trondb-tql/src/parser.rs crates/trondb-core/src/planner.rs crates/trondb-core/src/executor.rs
git commit -m "feat(tql): add DECAY clause to CREATE EDGE syntax"
```

---

### Task 15: DecaySweeper Background Task

**Files:**
- Create: `crates/trondb-routing/src/sweeper.rs`
- Modify: `crates/trondb-routing/src/lib.rs` (pub mod sweeper)
- Modify: `crates/trondb-cli/src/main.rs` (wire sweeper)
- Test: `crates/trondb-routing/src/lib.rs` (integration tests)

- [ ] **Step 1: Create sweeper.rs**

```rust
use std::sync::Arc;
use trondb_core::Engine;
use trondb_core::edge::effective_confidence;

pub struct DecaySweeper {
    pub(crate) engine: Arc<Engine>,
    pub(crate) interval_secs: u64,
}

impl DecaySweeper {
    pub fn new(engine: Arc<Engine>, interval_secs: u64) -> Self {
        Self { engine, interval_secs }
    }

    /// Run one sweep cycle: scan edge types with decay config, prune expired edges.
    pub async fn sweep_once(&self) -> usize {
        let edge_types = self.engine.list_edge_types();
        let mut pruned = 0;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        for et in &edge_types {
            if et.decay_config.prune_threshold.is_none() {
                continue;
            }
            let prune_threshold = et.decay_config.prune_threshold.unwrap();

            let edges = match self.engine.scan_edges(&et.name) {
                Ok(e) => e,
                Err(_) => continue,
            };

            for edge in &edges {
                if edge.created_at == 0 {
                    continue; // legacy edge, no decay
                }
                let elapsed = now_ms.saturating_sub(edge.created_at);
                let conf = effective_confidence(edge.confidence, elapsed, &et.decay_config);
                if conf < prune_threshold as f32 {
                    // Delete via TQL execution (TQL lexer only supports single-quote strings)
                    let tql = format!(
                        "DELETE EDGE {} FROM '{}' TO '{}';",
                        et.name,
                        edge.from_id.as_str(),
                        edge.to_id.as_str()
                    );
                    if let Ok(plan) = self.engine.parse_and_plan(&tql) {
                        let _ = self.engine.execute(&plan).await;
                        pruned += 1;
                    }
                }
            }
        }

        pruned
    }

    /// Spawn the background sweep loop.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(self.interval_secs)).await;
                let pruned = self.sweep_once().await;
                if pruned > 0 {
                    eprintln!("[DecaySweeper] pruned {pruned} expired edges");
                }
            }
        })
    }
}
```

**Important:** `Engine` does NOT expose `executor()`. You MUST add delegate methods to `crates/trondb-core/src/lib.rs`:

```rust
pub fn list_edge_types(&self) -> Vec<crate::edge::EdgeType> {
    self.executor.list_edge_types()
}

pub fn scan_edges(&self, edge_type: &str) -> Result<Vec<crate::edge::Edge>, EngineError> {
    self.executor.scan_edges(edge_type)
}
```

These delegate to the existing public methods on `Executor` (at executor.rs:991 and executor.rs:995).

- [ ] **Step 2: Add pub mod sweeper**

In `crates/trondb-routing/src/lib.rs`, add `pub mod sweeper;`

- [ ] **Step 3: Wire in CLI**

In `crates/trondb-cli/src/main.rs`, after creating the engine, construct and spawn the sweeper:

```rust
let sweeper = Arc::new(DecaySweeper::new(Arc::clone(&engine), 60));
let _sweeper_handle = Arc::clone(&sweeper).spawn();
```

- [ ] **Step 4: Write integration test**

In `crates/trondb-routing/src/lib.rs` tests:

```rust
#[tokio::test]
async fn decay_sweeper_prunes_expired_edges() {
    // Create engine with edge type that has very aggressive decay
    // Insert edge with created_at in the past (simulate old edge)
    // Run sweep_once()
    // Verify edge is deleted
}
```

Note: To test this properly, you may need to insert an edge with a `created_at` far in the past. Since InsertEdge sets `created_at` to now, you might need to modify the edge directly in Fjall or use a test-only method.

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/src/lib.rs crates/trondb-routing/src/sweeper.rs crates/trondb-routing/src/lib.rs crates/trondb-cli/src/main.rs
git commit -m "feat: add DecaySweeper background task for edge pruning"
```

---

### Task 16: CLAUDE.md Update + Clippy Clean

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Fix any warnings.

- [ ] **Step 2: Run all tests**

Run: `cargo test --workspace`
Expected: All pass

- [ ] **Step 3: Update CLAUDE.md**

Update the header to "Phase 7b" and add documentation for the new features:

- Under "Edges" section: update to mention multi-hop TRAVERSE (BFS, depth cap 10, cycle detection), entity deletion (cascading cleanup), edge confidence decay (lazy on read, background sweeper)
- Under "Field Index" section: mention range FETCH support (Gt/Lt/Gte/Lte → FieldIndexRange strategy)
- Remove "TRAVERSE returns connected entities (single-hop, DEPTH > 1 gated)"
- Remove "DecayConfig fields defined but not driven until Phase 6"

- [ ] **Step 4: Run clippy + tests again**

Run: `cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: Clean

- [ ] **Step 5: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: update CLAUDE.md for Phase 7b — multi-hop TRAVERSE, entity deletion, range FETCH, edge decay"
```
