# Graph-Scoped Vector Search Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `WITHIN (TRAVERSE ...)` clause to SEARCH so vector search can be scoped to a graph neighbourhood in a single server-side operation.

**Architecture:** The WITHIN clause is an orthogonal modifier on SEARCH. The parser parses a TraverseMatchStmt inside `WITHIN(...)`, the planner carries it as an optional field on SearchPlan, and the executor resolves it at runtime — either brute-force cosine over small subgraphs (<500 entities) or HNSW with post-filter for larger ones.

**Tech Stack:** Rust 2021, Tokio async, logos lexer, recursive descent parser, Fjall LSM store, hnsw_rs, tonic/prost gRPC, comfy-table CLI

**Spec:** `docs/superpowers/specs/2026-03-13-graph-scoped-search-design.md`

---

## Chunk 1: Token, AST, and Parser

### Task 1: Add `Within` token to the lexer

**Files:**
- Modify: `crates/trondb-tql/src/token.rs:50` (after `With` token)

- [ ] **Step 1: Add the token variant**

In `crates/trondb-tql/src/token.rs`, add after line 50 (`With` variant):

```rust
    #[token("WITHIN", priority = 10, ignore(ascii_case))]
    Within,
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p trondb-tql`
Expected: compiles cleanly

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-tql/src/token.rs
git commit -m "feat(tql): add Within keyword token"
```

---

### Task 2: Add `within` field to SearchStmt AST

**Files:**
- Modify: `crates/trondb-tql/src/ast.rs:170-181`

- [ ] **Step 1: Add the field**

In `crates/trondb-tql/src/ast.rs`, add a new field to `SearchStmt` before the closing brace at line 181:

```rust
    pub within: Option<Box<TraverseMatchStmt>>,
```

- [ ] **Step 2: Fix all compilation errors**

Run: `cargo check -p trondb-tql`

Every place that constructs a `SearchStmt` will now fail. Fix each one by adding `within: None`. Key locations:
- `crates/trondb-tql/src/parser.rs:1279-1290` — the `parse_search()` return
- Any test files that construct `SearchStmt` directly

Run: `cargo check -p trondb-tql`
Expected: compiles cleanly

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-tql/src/ast.rs crates/trondb-tql/src/parser.rs
git commit -m "feat(tql): add within field to SearchStmt AST"
```

---

### Task 3: Parse WITHIN clause in parse_search()

**Files:**
- Modify: `crates/trondb-tql/src/parser.rs:1271-1290`
- Test: `crates/trondb-tql/src/parser.rs` (inline tests module)

- [ ] **Step 1: Write the failing test**

Add to the tests module at the bottom of `crates/trondb-tql/src/parser.rs`:

```rust
#[test]
fn parse_search_within_traverse() {
    let input = "SEARCH things NEAR VECTOR [1.0, 2.0] USING dense WITHIN (TRAVERSE FROM 'seed1' MATCH (a)-[e:knows]->(b) DEPTH 1..2) LIMIT 5;";
    let stmt = parse(input).unwrap();
    match stmt {
        Statement::Search(s) => {
            assert_eq!(s.collection, "things");
            assert!(s.within.is_some());
            let within = s.within.unwrap();
            assert_eq!(within.from_id, "seed1");
            assert_eq!(within.min_depth, 1);
            assert_eq!(within.max_depth, 2);
            assert_eq!(within.pattern.edge.edge_type, Some("knows".to_string()));
            assert_eq!(s.limit, Some(5));
        }
        _ => panic!("expected Search"),
    }
}

#[test]
fn parse_search_within_and_where() {
    let input = "SEARCH things NEAR VECTOR [1.0] USING dense WHERE color = 'red' WITHIN (TRAVERSE FROM 'x' MATCH (a)-[e]->(b) DEPTH 1..3) LIMIT 10;";
    let stmt = parse(input).unwrap();
    match stmt {
        Statement::Search(s) => {
            assert!(s.filter.is_some());
            assert!(s.within.is_some());
            assert_eq!(s.limit, Some(10));
        }
        _ => panic!("expected Search"),
    }
}

#[test]
fn parse_search_without_within() {
    let input = "SEARCH things NEAR VECTOR [1.0] USING dense LIMIT 5;";
    let stmt = parse(input).unwrap();
    match stmt {
        Statement::Search(s) => {
            assert!(s.within.is_none());
        }
        _ => panic!("expected Search"),
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p trondb-tql parse_search_within`
Expected: FAIL — the parser doesn't recognise WITHIN yet

- [ ] **Step 3: Implement WITHIN parsing**

In `crates/trondb-tql/src/parser.rs`, in `parse_search()`, after the LIMIT parsing (line 1276) and before the semicolon assertion (line 1278), add:

```rust
        // WITHIN (TRAVERSE ...) clause
        let within = if self.peek() == Some(&Token::Within) {
            self.advance(); // consume WITHIN
            self.expect(&Token::LParen)?;
            // Reuse the existing traverse match parser.
            // parse_traverse_match() expects the TRAVERSE token to already be consumed,
            // so consume it here.
            self.expect(&Token::Traverse)?;
            let traverse_stmt = self.parse_traverse_match_inner()?;
            self.expect(&Token::RParen)?;
            Some(Box::new(traverse_stmt))
        } else {
            None
        };
```

**Important:** The existing `parse_traverse_match()` consumes the `TRAVERSE` keyword and expects a trailing `;`. We need a variant that doesn't consume the keyword (already done above) and doesn't expect a `;` (because `)` follows instead). The cleanest approach is to extract the body of `parse_traverse_match()` into a `parse_traverse_match_inner()` helper that parses everything after `TRAVERSE` and doesn't consume a semicolon:

In `parse_traverse_match()` (line 922), refactor:

```rust
    fn parse_traverse_match(&mut self) -> Result<Statement, ParseError> {
        let stmt = self.parse_traverse_match_inner()?;
        self.expect(&Token::Semicolon)?;
        Ok(Statement::TraverseMatch(stmt))
    }

    /// Parses the body of a TRAVERSE MATCH statement (after the TRAVERSE keyword,
    /// without consuming a trailing semicolon). Used by both standalone TRAVERSE
    /// and SEARCH ... WITHIN (TRAVERSE ...).
    fn parse_traverse_match_inner(&mut self) -> Result<TraverseMatchStmt, ParseError> {
        // ... move the existing body of parse_traverse_match() here,
        // removing the self.expect(&Token::Semicolon) call and
        // returning TraverseMatchStmt instead of Statement::TraverseMatch(...)
    }
```

Then update the SearchStmt construction (line 1279-1290) to include `within`:

```rust
        Ok(Statement::Search(SearchStmt {
            collection,
            fields: FieldList::All,
            dense_vector,
            sparse_vector,
            filter,
            confidence,
            limit,
            query_text,
            using_repr,
            hints,
            within,
        }))
```

**Note on clause ordering:** The WITHIN clause must be parsed AFTER LIMIT in the current grammar. Check the actual order: CONFIDENCE, then LIMIT, then WITHIN. If you need WITHIN before LIMIT, adjust accordingly — but the spec says `[WHERE] [WITHIN] [LIMIT]`. Review the parse order and make WITHIN come after WHERE/CONFIDENCE and before LIMIT. You may need to move the WITHIN parsing block before the LIMIT parsing block.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p trondb-tql parse_search_within`
Expected: all 3 tests PASS

- [ ] **Step 5: Run full TQL test suite**

Run: `cargo test -p trondb-tql`
Expected: all existing tests still pass

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-tql/src/parser.rs
git commit -m "feat(tql): parse WITHIN (TRAVERSE ...) clause in SEARCH"
```

---

## Chunk 2: Planner and Proto

### Task 4: Add `within` field to SearchPlan

**Files:**
- Modify: `crates/trondb-core/src/planner.rs:117-131` (SearchPlan struct)
- Modify: `crates/trondb-core/src/planner.rs:474-488` (SearchPlan construction)

- [ ] **Step 1: Add the field to SearchPlan**

In `crates/trondb-core/src/planner.rs`, add to `SearchPlan` struct before line 131:

```rust
    pub within: Option<Box<TraverseMatchPlan>>,
```

- [ ] **Step 2: Fix all compilation errors**

Every place that constructs `SearchPlan` will fail. Fix each one by adding `within: None` or wiring through the AST value. Key locations:

- `crates/trondb-core/src/planner.rs:474-488` — the Statement::Search planning. Here, wire the value:

```rust
                within: s.within.as_ref().map(|w| {
                    Box::new(TraverseMatchPlan {
                        from_id: w.from_id.clone(),
                        pattern: w.pattern.clone(),
                        min_depth: w.min_depth,
                        max_depth: w.max_depth,
                        confidence_threshold: w.confidence_threshold,
                        temporal: w.temporal.clone(),
                        limit: w.limit,
                    })
                }),
```

- All test files that construct `SearchPlan` directly — add `within: None`
- `crates/trondb-proto/src/convert_plan.rs:926-944` — proto-to-plan conversion, add `within: None` temporarily

Run: `cargo check --workspace`
Expected: compiles cleanly

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-core/src/planner.rs crates/trondb-proto/src/convert_plan.rs
git commit -m "feat(planner): add within field to SearchPlan"
```

---

### Task 5: Wire WITHIN through Proto/gRPC

**Files:**
- Modify: `crates/trondb-proto/proto/trondb.proto:146-162` (SearchPlan message)
- Modify: `crates/trondb-proto/src/convert_plan.rs:554-581` (plan-to-proto)
- Modify: `crates/trondb-proto/src/convert_plan.rs:926-944` (proto-to-plan)

- [ ] **Step 1: Add proto field**

In `crates/trondb-proto/proto/trondb.proto`, add to `SearchPlan` message after line 161 (the `two_pass` field):

```protobuf
    optional TraverseMatchPlanProto within = 16;
```

- [ ] **Step 2: Add plan-to-proto conversion**

In `crates/trondb-proto/src/convert_plan.rs`, in the `Plan::Search` arm (line 554-581), add after line 580 (`two_pass`):

```rust
                    within: sp.within.as_ref().map(|w| {
                        let direction = match w.pattern.edge.direction {
                            trondb_tql::EdgeDirection::Forward => pb::EdgeDirectionProto::EdgeDirectionForward,
                            trondb_tql::EdgeDirection::Backward => pb::EdgeDirectionProto::EdgeDirectionBackward,
                            trondb_tql::EdgeDirection::Undirected => pb::EdgeDirectionProto::EdgeDirectionUndirected,
                        };
                        let edge_pattern = pb::EdgePatternProto {
                            variable: w.pattern.edge.variable.clone(),
                            edge_type: w.pattern.edge.edge_type.clone(),
                            direction: direction.into(),
                        };
                        let pattern = pb::MatchPatternProto {
                            source_var: w.pattern.source_var.clone(),
                            edge: Some(edge_pattern),
                            target_var: w.pattern.target_var.clone(),
                        };
                        pb::TraverseMatchPlanProto {
                            from_id: w.from_id.clone(),
                            pattern: Some(pattern),
                            min_depth: w.min_depth as u64,
                            max_depth: w.max_depth as u64,
                            confidence_threshold: w.confidence_threshold,
                            limit: w.limit.map(|l| l as u64),
                            temporal: w.temporal.as_ref().map(temporal_clause_to_proto),
                        }
                    }),
```

Note: This reuses the exact same conversion logic already in the `Plan::TraverseMatch` arm (lines 742-769). If you want to DRY this up, extract a helper `traverse_match_plan_to_proto(p: &TraverseMatchPlan) -> pb::TraverseMatchPlanProto` and call it from both locations.

- [ ] **Step 3: Add proto-to-plan conversion**

In `crates/trondb-proto/src/convert_plan.rs`, in the proto-to-plan `SearchPlan` construction (line 926-944), replace the `within: None` placeholder with:

```rust
                    within: sp.within.map(|w| {
                        let pattern_proto = w.pattern.unwrap_or_default();
                        let edge_proto = pattern_proto.edge.unwrap_or_default();
                        let direction = match pb::EdgeDirectionProto::try_from(edge_proto.direction).unwrap_or(pb::EdgeDirectionProto::EdgeDirectionForward) {
                            pb::EdgeDirectionProto::EdgeDirectionForward => trondb_tql::EdgeDirection::Forward,
                            pb::EdgeDirectionProto::EdgeDirectionBackward => trondb_tql::EdgeDirection::Backward,
                            pb::EdgeDirectionProto::EdgeDirectionUndirected => trondb_tql::EdgeDirection::Undirected,
                        };
                        Box::new(TraverseMatchPlan {
                            from_id: w.from_id,
                            pattern: trondb_tql::MatchPattern {
                                source_var: pattern_proto.source_var,
                                edge: trondb_tql::EdgePattern {
                                    variable: edge_proto.variable,
                                    edge_type: edge_proto.edge_type,
                                    direction,
                                },
                                target_var: pattern_proto.target_var,
                            },
                            min_depth: w.min_depth as usize,
                            max_depth: w.max_depth as usize,
                            confidence_threshold: w.confidence_threshold,
                            temporal: w.temporal.as_ref().map(proto_to_temporal_clause).transpose().ok().flatten(),
                            limit: w.limit.map(|l| l as usize),
                        })
                    }),
```

Note: Again, this mirrors the existing `PP::TraverseMatch` conversion (lines 1113-1140). DRY it up if you want.

- [ ] **Step 4: Verify compilation**

Run: `cargo check --workspace`
Expected: compiles cleanly

- [ ] **Step 5: Run proto round-trip tests**

Run: `cargo test -p trondb-proto`
Expected: all existing tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-proto/
git commit -m "feat(proto): wire WITHIN field through SearchPlan proto"
```

---

## Chunk 3: Executor — Extract traverse_match helper and WITHIN resolution

### Task 6: Extract execute_traverse_match into a reusable helper

**Files:**
- Modify: `crates/trondb-core/src/executor.rs:2658-2853` (TraverseMatch arm)

The `Plan::TraverseMatch` logic is currently inline in the `execute()` match arm. We need a helper method so the SEARCH executor can call it for the WITHIN subquery.

- [ ] **Step 1: Write the helper signature**

Add a new method to the executor impl block:

```rust
    /// Execute a TraverseMatch sub-plan and return the result.
    /// Used by both standalone TRAVERSE MATCH and SEARCH ... WITHIN.
    async fn execute_traverse_match_plan(
        &self,
        p: &TraverseMatchPlan,
    ) -> Result<QueryResult, EngineError> {
        let start = std::time::Instant::now();
        // ... move existing TraverseMatch body here ...
    }
```

- [ ] **Step 2: Move the TraverseMatch body into the helper**

Cut the body of `Plan::TraverseMatch(p) => { ... }` (lines 2659-2853) and paste it into `execute_traverse_match_plan()`. The `Plan::TraverseMatch` arm becomes:

```rust
            Plan::TraverseMatch(p) => {
                self.execute_traverse_match_plan(p).await
            }
```

- [ ] **Step 3: Verify compilation and tests**

Run: `cargo test -p trondb-core traverse`
Expected: all existing traverse tests pass

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "refactor(executor): extract execute_traverse_match_plan helper"
```

---

### Task 7: Implement WITHIN resolution in the SEARCH executor

**Files:**
- Modify: `crates/trondb-core/src/executor.rs:967-1214` (Plan::Search arm)

- [ ] **Step 1: Write the failing test**

Add to the test module in `crates/trondb-core/src/executor.rs`:

```rust
#[tokio::test]
async fn search_within_traverse_brute_force() {
    // Setup: create collection with 5 entities and edges forming a chain:
    // A -> B -> C (via "link" edges), plus D and E (disconnected)
    // All have dense vectors. Search WITHIN traverse from A depth 1..2
    // should only return B and C, not D or E.
    let (engine, _) = Engine::open(test_config()).await.unwrap();
    let schema_tql = r#"CREATE COLLECTION items (
        FIELD name TEXT,
        REPRESENTATION dense MODEL 'test' DIMENSIONS 3 METRIC COSINE
    );"#;
    engine.execute_tql(schema_tql).await.unwrap();
    engine.execute_tql("CREATE EDGE link FROM items TO items;").await.unwrap();

    // Insert 5 entities with known vectors
    let vectors = vec![
        ("a", [1.0, 0.0, 0.0]),
        ("b", [0.9, 0.1, 0.0]),
        ("c", [0.8, 0.2, 0.0]),
        ("d", [0.0, 0.0, 1.0]),  // far away
        ("e", [0.0, 1.0, 0.0]),  // far away
    ];
    for (name, vec) in &vectors {
        let vec_str = format!("[{}, {}, {}]", vec[0], vec[1], vec[2]);
        let tql = format!(
            "INSERT INTO items (id, name) VALUES ('{name}', '{name}') REPRESENTATION dense VECTOR {vec_str};"
        );
        engine.execute_tql(&tql).await.unwrap();
    }

    // Create edges: a->b, b->c
    engine.execute_tql("INSERT EDGE link FROM 'a' TO 'b';").await.unwrap();
    engine.execute_tql("INSERT EDGE link FROM 'b' TO 'c';").await.unwrap();

    // SEARCH WITHIN TRAVERSE: should find b and c (1-2 hops from a), ranked by similarity to [1,0,0]
    let result = engine.execute_tql(
        "SEARCH items NEAR VECTOR [1.0, 0.0, 0.0] USING dense WITHIN (TRAVERSE FROM 'a' MATCH (a)-[e:link]->(b) DEPTH 1..2) LIMIT 10;"
    ).await.unwrap();

    // Should return exactly b and c (not a, d, or e)
    let ids: Vec<String> = result.rows.iter()
        .filter_map(|r| r.values.get("id").map(|v| v.to_string()))
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"b".to_string()));
    assert!(ids.contains(&"c".to_string()));
    // b should rank higher (closer to [1,0,0])
    assert_eq!(ids[0], "b");
}

#[tokio::test]
async fn search_within_empty_subgraph() {
    let (engine, _) = Engine::open(test_config()).await.unwrap();
    engine.execute_tql(r#"CREATE COLLECTION items (
        FIELD name TEXT,
        REPRESENTATION dense MODEL 'test' DIMENSIONS 3 METRIC COSINE
    );"#).await.unwrap();
    engine.execute_tql("CREATE EDGE link FROM items TO items;").await.unwrap();
    engine.execute_tql(
        "INSERT INTO items (id, name) VALUES ('x', 'x') REPRESENTATION dense VECTOR [1.0, 0.0, 0.0];"
    ).await.unwrap();

    // TRAVERSE from a non-existent entity — empty subgraph, should return 0 results
    let result = engine.execute_tql(
        "SEARCH items NEAR VECTOR [1.0, 0.0, 0.0] USING dense WITHIN (TRAVERSE FROM 'nonexistent' MATCH (a)-[e:link]->(b) DEPTH 1..1) LIMIT 10;"
    ).await.unwrap();
    assert_eq!(result.rows.len(), 0);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p trondb-core search_within`
Expected: FAIL — the executor doesn't handle `within` yet

- [ ] **Step 3: Implement WITHIN resolution**

In `crates/trondb-core/src/executor.rs`, in the `Plan::Search(p)` arm, after the pre-filter resolution (line 991) and before the fetch_k calculation (line 994):

1. Change `let pre_filter_ids` to `let mut pre_filter_ids` on line 975.

2. Add the WITHIN resolution block:

```rust
                // Step 1b: WITHIN — resolve graph subgraph candidate set
                const BRUTE_FORCE_SUBGRAPH_THRESHOLD: usize = 500;

                if let Some(ref within_plan) = p.within {
                    let traverse_result = self.execute_traverse_match_plan(within_plan).await?;
                    let within_ids: HashSet<LogicalId> = traverse_result.rows.iter()
                        .filter_map(|row| row.values.get("id").map(|v| LogicalId::from_string(&v.to_string())))
                        .collect();

                    // Intersect with WHERE pre-filter if present
                    let final_candidates = match pre_filter_ids.take() {
                        Some(pf) => pf.intersection(&within_ids).cloned().collect::<HashSet<_>>(),
                        None => within_ids,
                    };

                    // Short-circuit on empty candidate set
                    if final_candidates.is_empty() {
                        return Ok(QueryResult {
                            columns: vec![],
                            rows: vec![],
                            stats: QueryStats {
                                elapsed: start.elapsed(),
                                entities_scanned: 0,
                                mode: QueryMode::Probabilistic,
                                tier: "Ram".into(),
                                cost: cost_estimate.clone(),
                                warnings: opt_warnings.clone(),
                            },
                        });
                    }

                    if final_candidates.len() < BRUTE_FORCE_SUBGRAPH_THRESHOLD {
                        // BruteForceSubgraph path
                        return self.execute_brute_force_subgraph(
                            p, &final_candidates, start, &cost_estimate, &opt_warnings,
                        ).await;
                    } else {
                        // Large subgraph: fall through to HNSW with post-filter
                        pre_filter_ids = Some(final_candidates);
                    }
                }
```

- [ ] **Step 4: Implement execute_brute_force_subgraph helper**

Add a new method to the executor impl block:

```rust
    async fn execute_brute_force_subgraph(
        &self,
        plan: &SearchPlan,
        candidates: &HashSet<LogicalId>,
        start: std::time::Instant,
        cost_estimate: &AcuEstimate,
        warnings: &[PlanWarning],
    ) -> Result<QueryResult, EngineError> {
        let repr_name = plan.using_repr.as_ref()
            .ok_or_else(|| EngineError::InvalidQuery("WITHIN brute-force requires USING repr".into()))?;

        // Resolve the query vector
        let query_vec: Vec<f32> = if let Some(ref dv) = plan.dense_vector {
            dv.iter().map(|&x| x as f32).collect()
        } else if let Some(ref qt) = plan.query_text {
            // NaturalLanguage: encode query text via vectoriser
            let vr = self.vectoriser_registry();
            let key = format!("{}:{}", plan.collection, repr_name);
            let vectoriser = vr.get(&key)
                .ok_or_else(|| EngineError::InvalidQuery(
                    format!("no vectoriser registered for {key}"),
                ))?;
            match vectoriser.encode_query(qt).await? {
                trondb_core::vectoriser::VectorData::Dense(v) => v,
                _ => return Err(EngineError::InvalidQuery("expected dense vector from encode_query".into())),
            }
        } else {
            return Err(EngineError::InvalidQuery(
                "WITHIN brute-force requires a dense vector or query text".into(),
            ));
        };

        // Compute query norm once
        let query_norm: f32 = query_vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if query_norm == 0.0 {
            return Err(EngineError::InvalidQuery("zero-norm query vector".into()));
        }

        // Score each candidate by cosine similarity
        let mut scored: Vec<(LogicalId, f32)> = Vec::with_capacity(candidates.len());

        for cid in candidates {
            // Fetch entity from store
            let entity = match self.store.get(&plan.collection, cid) {
                Ok(e) => e,
                Err(_) => continue, // entity may have been deleted
            };

            // Find the matching representation
            let repr = entity.representations.iter()
                .find(|r| r.name == *repr_name);
            let repr = match repr {
                Some(r) => r,
                None => continue,
            };

            // Check for Dirty/Recomputing state
            if let Some(loc) = self.location_table().get(cid, repr_name) {
                if loc.state == crate::location::LocState::Dirty
                    || loc.state == crate::location::LocState::Recomputing
                {
                    continue;
                }
            }

            // Extract dense vector
            let vec_data = match &repr.data {
                crate::types::VectorData::Dense(v) => v,
                _ => continue,
            };

            // Cosine similarity
            let dot: f32 = query_vec.iter().zip(vec_data.iter()).map(|(a, b)| a * b).sum();
            let cand_norm: f32 = vec_data.iter().map(|x| x * x).sum::<f32>().sqrt();
            if cand_norm == 0.0 {
                continue;
            }
            let score = dot / (query_norm * cand_norm);
            scored.push((cid.clone(), score));
        }

        // Sort by score descending, take top-k
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(plan.k);

        // Build result rows
        let mut rows = Vec::new();
        for (id, score) in &scored {
            if let Ok(entity) = self.store.get(&plan.collection, id) {
                let mut row = entity_to_row(&entity, &plan.fields);
                row.score = Some(*score);
                rows.push(row);
            }
        }

        let columns = build_columns(&rows, &plan.fields);
        Ok(QueryResult {
            columns,
            rows,
            stats: QueryStats {
                elapsed: start.elapsed(),
                entities_scanned: scored.len(),
                mode: QueryMode::Probabilistic,
                tier: "Ram".into(),
                cost: cost_estimate.clone(),
                warnings: warnings.to_vec(),
            },
        })
    }
```

**Note on imports:** You'll need access to `entity_to_row`, `build_columns`, `LogicalId`, `HashSet`, `SearchPlan`, `QueryResult`, `QueryStats`, `QueryMode`, `AcuEstimate`, `PlanWarning` — most are already in scope within the executor. Check the imports at the top of executor.rs and add any missing ones.

**Note on VectorData/location access:** The exact field names and access patterns (e.g., `entity.representations`, `repr.data`, `self.location_table()`) must match the actual codebase. Read the surrounding executor code for HNSW search (lines 997-1138) to see how entities and representations are accessed. Adapt the code above to match.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p trondb-core search_within`
Expected: both tests PASS

- [ ] **Step 6: Run full workspace tests**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 7: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "feat(executor): implement WITHIN graph-scoped search with brute-force subgraph"
```

---

## Chunk 4: Integration Tests and EXPLAIN

### Task 8: EXPLAIN support for WITHIN

**Files:**
- Modify: `crates/trondb-core/src/executor.rs` (EXPLAIN arm, around line 1216)

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn explain_search_within() {
    let (engine, _) = Engine::open(test_config()).await.unwrap();
    engine.execute_tql(r#"CREATE COLLECTION items (
        FIELD name TEXT,
        REPRESENTATION dense MODEL 'test' DIMENSIONS 3 METRIC COSINE
    );"#).await.unwrap();
    engine.execute_tql("CREATE EDGE link FROM items TO items;").await.unwrap();

    let result = engine.execute_tql(
        "EXPLAIN SEARCH items NEAR VECTOR [1.0, 0.0, 0.0] USING dense WITHIN (TRAVERSE FROM 'a' MATCH (a)-[e:link]->(b) DEPTH 1..2) LIMIT 5;"
    ).await.unwrap();

    // EXPLAIN should mention WITHIN / traverse subquery
    let output = format!("{:?}", result);
    assert!(output.contains("Within") || output.contains("within") || output.contains("WITHIN"),
        "EXPLAIN should mention WITHIN subquery: {output}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p trondb-core explain_search_within`
Expected: FAIL or no WITHIN info shown

- [ ] **Step 3: Add WITHIN info to EXPLAIN output**

In the `Plan::Explain` handler in executor.rs, find where `SearchPlan` fields are rendered into the explain output. Add a section for the `within` field:

```rust
if let Some(ref within) = sp.within {
    // Add WITHIN subquery info to explain output
    let edge_type = within.pattern.edge.edge_type.as_deref().unwrap_or("*");
    let direction = match within.pattern.edge.direction {
        trondb_tql::EdgeDirection::Forward => "->",
        trondb_tql::EdgeDirection::Backward => "<-",
        trondb_tql::EdgeDirection::Undirected => "-",
    };
    explain_rows.push(format!(
        "Within: TRAVERSE FROM '{}' MATCH -[{}:{}]{} DEPTH {}..{}",
        within.from_id, within.pattern.edge.variable.as_deref().unwrap_or("_"),
        edge_type, direction,
        within.min_depth, within.max_depth,
    ));
}
```

Adapt the exact variable names and format to match the existing EXPLAIN output style in the codebase.

- [ ] **Step 4: Run tests**

Run: `cargo test -p trondb-core explain_search_within`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "feat(explain): show WITHIN subquery in EXPLAIN output"
```

---

### Task 9: End-to-end gRPC test

**Files:**
- Test: `crates/trondb-core/src/executor.rs` (or integration test file)

- [ ] **Step 1: Write the test**

This tests the full path through `execute_tql()` which parses TQL, plans, and executes — the same path used by the gRPC `ExecuteTql` endpoint:

```rust
#[tokio::test]
async fn search_within_traverse_end_to_end() {
    let (engine, _) = Engine::open(test_config()).await.unwrap();

    // Setup schema, edges, data
    engine.execute_tql(r#"CREATE COLLECTION docs (
        FIELD title TEXT,
        REPRESENTATION dense MODEL 'test' DIMENSIONS 3 METRIC COSINE
    );"#).await.unwrap();
    engine.execute_tql("CREATE EDGE related FROM docs TO docs;").await.unwrap();

    // Insert entities
    for (id, title, v) in &[
        ("hub", "central hub", "[0.5, 0.5, 0.0]"),
        ("spoke1", "first spoke", "[0.9, 0.1, 0.0]"),
        ("spoke2", "second spoke", "[0.1, 0.9, 0.0]"),
        ("island", "isolated island", "[0.5, 0.5, 0.0]"),
    ] {
        engine.execute_tql(&format!(
            "INSERT INTO docs (id, title) VALUES ('{id}', '{title}') REPRESENTATION dense VECTOR {v};"
        )).await.unwrap();
    }

    // Edges: hub -> spoke1, hub -> spoke2 (island is disconnected)
    engine.execute_tql("INSERT EDGE related FROM 'hub' TO 'spoke1';").await.unwrap();
    engine.execute_tql("INSERT EDGE related FROM 'hub' TO 'spoke2';").await.unwrap();

    // Search for vector closest to spoke1, but only within hub's neighbourhood
    let result = engine.execute_tql(
        "SEARCH docs NEAR VECTOR [0.9, 0.1, 0.0] USING dense WITHIN (TRAVERSE FROM 'hub' MATCH (a)-[e:related]->(b) DEPTH 1..1) LIMIT 5;"
    ).await.unwrap();

    // Should find spoke1 and spoke2 but NOT island (even though island has similar vector)
    assert_eq!(result.rows.len(), 2);
    let ids: Vec<String> = result.rows.iter()
        .filter_map(|r| r.values.get("id").map(|v| v.to_string()))
        .collect();
    assert!(ids.contains(&"spoke1".to_string()));
    assert!(ids.contains(&"spoke2".to_string()));
    assert!(!ids.contains(&"island".to_string()));
    // spoke1 should be first (closest to query vector)
    assert_eq!(ids[0], "spoke1");
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p trondb-core search_within_traverse_end_to_end`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "test: end-to-end SEARCH WITHIN TRAVERSE integration test"
```

---

### Task 10: Update CLAUDE.md and benchmark

**Files:**
- Modify: `CLAUDE.md` (add Phase 16 section)
- Modify: `tools/johnny_benchmark.py` (add Scenario 4 TronDB path)

- [ ] **Step 1: Add Phase 16 to CLAUDE.md**

Add after the Phase 15 section:

```markdown
- Query Composition (Phase 16)
  - WITHIN clause on SEARCH: scope vector search to a graph subgraph
    - Syntax: SEARCH ... NEAR ... USING ... WITHIN (TRAVERSE FROM 'id' MATCH pattern DEPTH min..max) LIMIT k
    - All TRAVERSE features available inside WITHIN (direction, confidence, temporal, edge type filter)
    - Orthogonal modifier: works with Hnsw, Sparse, Hybrid, NaturalLanguage strategies
  - Two execution approaches, auto-selected at runtime:
    - BruteForceSubgraph (candidate set < 500): bypass HNSW, fetch vectors, compute cosine/inner-product directly
    - IndexPostFilter (candidate set >= 500): HNSW search with adaptive over-fetch, post-filter against candidate set
  - Combined WHERE + WITHIN: candidate sets intersected
  - WITHIN subquery shown in EXPLAIN output
  - Proto/gRPC: TraverseMatchPlanProto within field (field 16) on SearchPlan
  - execute_traverse_match_plan() extracted as reusable helper
```

- [ ] **Step 2: Update benchmark Scenario 4**

In `tools/johnny_benchmark.py`, in `cmd_bench_retrieval()`, update Scenario 4 to include TronDB timing now that WITHIN is available. Add a TronDB query using the new syntax and compare latency.

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md tools/johnny_benchmark.py
git commit -m "docs: add Phase 16 to CLAUDE.md, update benchmark for WITHIN"
```

- [ ] **Step 4: Rebuild and deploy**

```bash
CARGO_TARGET_DIR=/tmp/trondb-target cargo build -p trondb-cli
# Rebuild Docker image on NAS if desired
```

- [ ] **Step 5: Run the benchmark**

```bash
python3 tools/johnny_benchmark.py bench-retrieval
```

Verify Scenario 4 now shows TronDB times alongside DuckDB.
