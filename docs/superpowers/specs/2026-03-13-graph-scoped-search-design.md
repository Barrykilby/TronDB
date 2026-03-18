# Phase 16: Query Composition — Graph-Scoped Vector Search

**Date:** 2026-03-13
**Status:** Draft
**Motivation:** Benchmark of Johnny's 23K memory cards + 326K edges on TronDB showed that DuckDB can combine recursive CTE graph traversal with cosine similarity ranking in a single SQL query (~26ms). TronDB cannot — it requires multiple gRPC round-trips and client-side stitching. This is the critical retrieval pattern for a memory system: "within this topic neighbourhood, what's most relevant to my question?"

## Problem Statement

TronDB has all the right primitives — HNSW index for vector search, AdjacencyIndex for graph traversal, field indexes for scalar filtering — but no way to compose them in a single operation. The executor processes each Plan independently; there is no subquery, pipeline, or composition mechanism.

A memory retrieval system needs to express queries like:
- "Among cards semantically linked to this topic (2 hops), which are most similar to my question?"
- "Search for 'rust pipeline architecture' but only within the graph neighbourhood of this seed entity"
- "Vector search scoped to a project's knowledge cluster"

Currently this requires: (1) execute TRAVERSE, (2) extract IDs client-side, (3) execute SEARCH, (4) intersect results. Each step is a separate gRPC call with ~5ms overhead.

## TQL Syntax

### Basic form

```sql
SEARCH collection NEAR 'query text' USING repr_name
  WITHIN (TRAVERSE FROM 'seed_id' MATCH (a)-[e:edge_type]->(b) DEPTH min..max)
  LIMIT k;
```

### With explicit vector

```sql
SEARCH collection NEAR VECTOR [0.1, 0.2, ...] USING repr_name
  WITHIN (TRAVERSE FROM 'seed_id' MATCH (a)-[e]->(b) DEPTH 1..3)
  LIMIT k;
```

### Combined with WHERE (intersection of both filters)

```sql
SEARCH collection NEAR 'query' USING repr_name
  WHERE project = 'universe'
  WITHIN (TRAVERSE FROM 'seed_id' MATCH (a)-[e:semantic_link]->(b) DEPTH 1..2)
  LIMIT k;
```

### Full TRAVERSE features available in WITHIN

The inner TRAVERSE supports all existing TRAVERSE MATCH features:
- Edge type filter: `[e:semantic_link]`
- Direction: `->` (forward), `<-` (backward), `-` (undirected)
- Depth range: `DEPTH 1..3`
- Confidence threshold: `CONFIDENCE > 0.5`
- Temporal filter: `AS OF '2026-01-01T00:00:00Z'`

The parser reuses the existing `parse_traverse_match()` function inside the WITHIN parentheses, so all TRAVERSE features are automatically available — no special-casing needed.

### Clause ordering

`WITHIN` appears after `WHERE` (if present) and before `LIMIT`:

```
SEARCH collection NEAR ... USING ...
  [WHERE ...]
  [WITHIN (TRAVERSE ...)]
  [LIMIT k];
```

## Execution Strategy

The `WITHIN` clause is an **orthogonal modifier** on SEARCH, not a replacement for the underlying search strategy. The planner still selects `Hnsw`, `Sparse`, `Hybrid`, or `NaturalLanguage` based on the query vectors. The `within` field on `SearchPlan` is carried alongside the strategy and resolved at execution time.

The executor auto-selects between two subgraph evaluation approaches based on candidate set size:

### Approach 1: BruteForceSubgraph (candidate set < 500 entities)

For dense and natural-language searches:

1. Execute the inner TRAVERSE, collect entity IDs into `HashSet<LogicalId>`
2. If candidate set is empty, return 0 results immediately (short-circuit)
3. For each candidate ID, deserialise the `Entity` from the collection's Fjall partition, find the `Representation` matching `plan.using_repr`, extract the `VectorData::Dense` vector
4. Skip entities whose representation is in `Dirty` or `Recomputing` state (same exclusion as existing SEARCH path)
5. Compute cosine similarity: `dot(query, candidate) / (norm(query) * norm(candidate))`
6. Sort by similarity descending, take top-k
7. Fetch full entities for result rows

For sparse searches, step 5 uses inner product scoring instead of cosine similarity, matching the existing `SparseIndex::search()` behaviour.

For hybrid searches, both dense and sparse scores are computed, then merged via RRF (k=60) — same as the existing hybrid path but over the subgraph only.

This bypasses HNSW/SparseIndex entirely. For small subgraphs (<500), sequential computation is faster than index search + post-filter attrition.

### Approach 2: IndexPostFilter (candidate set >= 500 entities)

1. Execute the inner TRAVERSE, collect entity IDs into `HashSet<LogicalId>`
2. If candidate set is empty, return 0 results immediately (short-circuit)
3. Compute adaptive over-fetch ratio: `fetch_k = k * max(4, ceil(total_collection_size / candidate_set_size))`
4. Run the underlying search strategy (HNSW, Sparse, Hybrid, or NaturalLanguage) with `fetch_k`
5. Post-filter results against the candidate set (existing `pre_filter_ids` mechanism)
6. Trim to k results

This leverages the existing pre-filter post-filtering path in the executor. The strategy-specific pipeline (including NaturalLanguage vectoriser encoding) runs exactly as it does today — the candidate set just narrows the post-filter.

### Approach threshold

The 500-entity threshold is a compile-time constant (`BRUTE_FORCE_SUBGRAPH_THRESHOLD`). At 384 dimensions, brute-force cosine over 500 vectors takes <1ms; the crossover point where index + post-filter becomes faster depends on post-filter attrition ratio.

### Combined WHERE + WITHIN

When both `WHERE` pre-filter and `WITHIN` are present:
1. Resolve WHERE candidates via FieldIndex → `Set<LogicalId>`
2. Resolve WITHIN candidates via TRAVERSE → `Set<LogicalId>`
3. Intersect the two sets → `final_candidates`
4. If `final_candidates` is empty, return 0 results immediately
5. Apply the appropriate approach (BruteForce or IndexPostFilter) using the intersected set

## Architecture Changes

### Parser (`crates/trondb-tql/`)

**New token:** `Within`

**Lexer change** (`lexer.rs`): Add `Within` keyword token.

**AST change** (`ast.rs`): Add field to `SearchStmt`:
```rust
pub struct SearchStmt {
    // ... existing fields ...
    pub within: Option<Box<TraverseMatchStmt>>,
}
```

**Parser change** (`parser.rs`): In `parse_search()`, after parsing optional `WHERE` clause, check for `WITHIN` token. If present, expect `LParen`, call existing `parse_traverse_match()`, expect `RParen`. This reuses the full TRAVERSE MATCH parser including direction, confidence, temporal, and depth handling.

### Planner (`crates/trondb-core/src/planner.rs`)

**SearchPlan extension:**
```rust
pub struct SearchPlan {
    // ... existing fields ...
    pub within: Option<Box<TraverseMatchPlan>>,
}
```

**No new strategy variant.** The existing `SearchStrategy` enum (Hnsw, Sparse, Hybrid, NaturalLanguage) remains unchanged. The `within` field is orthogonal — it narrows the candidate set regardless of which search strategy is used. The planner continues to select strategy via `select_search_strategy()` based on query vector types.

**Planning logic:** If `SearchStmt.within` is `Some`, plan the inner `TraverseMatchStmt` into a `TraverseMatchPlan` and set `plan.within = Some(Box::new(inner_plan))`. The planner does NOT resolve the subgraph size at plan time — that happens at execution time because the subgraph depends on the live graph state.

**Cost model:**
- When `within` is present: ACU = traverse_cost + search_cost
- traverse_cost: `1.0 * max_depth * estimated_fan_out` (heuristic: fan_out = 10)
- search_cost: existing strategy cost (unchanged)
- Warning emitted if `max_depth >= 4` (subgraph likely too large for meaningful scoping)

### Executor (`crates/trondb-core/src/executor.rs`)

In the SEARCH execution path (around line 967), add a new block before the strategy dispatch. The existing `pre_filter_ids` declaration must change from `let` to `let mut`:

```rust
// Resolve WITHIN subquery if present
if let Some(ref within_plan) = plan.within {
    // 1. Execute TRAVERSE sub-plan
    let traverse_result = self.execute_traverse_match(within_plan).await?;
    let within_ids: HashSet<LogicalId> = traverse_result.rows.iter()
        .filter_map(|row| row.values.get("id").map(|v| LogicalId::from(v.to_string())))
        .collect();

    // 2. Intersect with WHERE pre-filter candidates if present
    let final_candidates = match pre_filter_ids.take() {
        Some(pf) => pf.intersection(&within_ids).cloned().collect(),
        None => within_ids,
    };

    // 3. Short-circuit on empty candidate set
    if final_candidates.is_empty() {
        return Ok(QueryResult::empty(&plan.fields, &schema));
    }

    // 4. Select approach based on candidate set size
    if final_candidates.len() < BRUTE_FORCE_SUBGRAPH_THRESHOLD {
        return self.execute_brute_force_subgraph(plan, &final_candidates).await;
    } else {
        pre_filter_ids = Some(final_candidates);
        // Fall through to existing strategy dispatch with adapted over-fetch
    }
}
```

**New helper method: `execute_brute_force_subgraph()`**

```rust
async fn execute_brute_force_subgraph(
    &self,
    plan: &SearchPlan,
    candidates: &HashSet<LogicalId>,
) -> Result<QueryResult, EngineError>
```

Implementation:
1. Determine the representation name from `plan.using_repr` (required for brute-force, error if None)
2. Resolve the query vector:
   - If `plan.dense_vector` is Some: use it directly
   - If `plan.query_text` is Some (NaturalLanguage): call `vectoriser.encode_query()` first to get the dense vector
   - If sparse-only or hybrid: see step 5
3. For each candidate ID in the set:
   a. Fetch the `Entity` from the collection's Fjall partition (same `store.get()` used by FETCH)
   b. Find the `Representation` matching `plan.using_repr` in `entity.representations`
   c. Check Location Table: skip if representation state is `Dirty` or `Recomputing`
   d. Extract `VectorData::Dense` vector (or `VectorData::Sparse` for sparse queries)
4. For dense/natural-language: compute cosine similarity for each candidate vector against the query vector
5. For sparse: compute inner product score for each candidate's sparse vector against the query sparse vector
6. For hybrid: compute both dense and sparse scores, merge via RRF (k=60, 1-based ranking) — same as existing hybrid merge
7. Sort by score descending, take top-k
8. Build `QueryResult` rows with score attached, `QueryMode::Probabilistic`

### Proto/gRPC (`crates/trondb-proto/`)

**Protobuf changes** (`trondb.proto`):
```protobuf
message SearchPlanProto {
    // ... existing fields (1-14) ...
    // Field 15 is TwoPassConfigProto two_pass
    optional TraverseMatchPlanProto within = 16;
}
```

Note: Field 15 is already used by `two_pass`. The `within` field uses field number 16.

**Conversion changes:** Wire `within` field through `From<SearchPlan> for SearchPlanProto` and the reverse conversion. The `TraverseMatchPlanProto` message already exists. No new strategy proto variant needed since the strategy enum is unchanged.

### EXPLAIN Output

```
Plan: Search
  Collection: memory_cards
  Strategy: Hnsw
  Within: TRAVERSE FROM 'seed_id' MATCH (a)-[e:semantic_link]->(b) DEPTH 1..2
  Estimated ACU: 12.5
  Notes:
    - Subgraph approach selected at execution time (BruteForceSubgraph if <500 candidates, IndexPostFilter otherwise)
    - WHERE pre-filter: idx_project (project = 'universe') — intersected with WITHIN candidates
```

At execution time, stats show:
```
10 row(s) | 147 scanned | probabilistic | Hnsw+Within(BruteForceSubgraph) | 3.21ms
```

### WAL / Persistence

No WAL changes needed. The WITHIN clause is a read-only query modifier — it doesn't create or modify any state. The inner TRAVERSE reads from the existing AdjacencyIndex.

### CLI / Display

No changes needed. The existing `format_result()` and `format_grpc_response()` functions handle any QueryResult. The stats line already shows strategy information.

## Test Plan

### Unit tests (executor)

1. **BruteForceSubgraph strategy:** Create collection with 10 entities and edges, SEARCH WITHIN TRAVERSE, verify results are scoped to subgraph and ranked by similarity
2. **IndexPostFilter strategy:** Create collection with 1000+ entities, verify HNSW + post-filter path is taken
3. **Combined WHERE + WITHIN:** Verify intersection of field-index candidates and graph candidates
4. **Empty subgraph:** TRAVERSE returns 0 nodes → SEARCH returns 0 results (not an error)
5. **Subgraph contains seed:** Verify seed entity is included/excluded correctly based on depth range
6. **Confidence threshold in WITHIN:** Verify low-confidence edges are excluded from candidate set
7. **Directed vs undirected vs backward in WITHIN:** Verify direction affects candidate set
8. **Dirty/Recomputing exclusion:** Verify entities with dirty representations are excluded from brute-force path
9. **NaturalLanguage + WITHIN:** Verify vectoriser encodes query text before brute-force cosine
10. **Sparse SEARCH + WITHIN:** Verify inner product scoring used instead of cosine for sparse representations

### Integration tests (CLI / gRPC)

11. **End-to-end gRPC:** Execute SEARCH WITHIN via ExecuteTql RPC, verify correct results
12. **EXPLAIN output:** Verify WITHIN subquery shown in explain output
13. **Benchmark regression:** Run Scenario 4 from johnny_benchmark.py, verify TronDB can now answer it

### Edge cases

14. **WITHIN with no matching edge type:** Candidate set empty, returns 0 results
15. **WITHIN depth 0:** Invalid (min_depth must be >= 1), parser rejects
16. **Very large subgraph (>5000 entities):** Verify IndexPostFilter path works with high over-fetch ratio
17. **Hybrid SEARCH + WITHIN:** Verify RRF merge works correctly over brute-force subgraph

## Scope Boundaries

### In scope
- `WITHIN` clause on SEARCH statements
- Two execution approaches (BruteForceSubgraph, IndexPostFilter) with auto-selection
- All search strategies (Hnsw, Sparse, Hybrid, NaturalLanguage) work with WITHIN
- Combined WHERE + WITHIN (intersection)
- Proto/gRPC support
- EXPLAIN output
- Cost model extension

### Out of scope (future work)
- `WITHIN` on FETCH — trivially addable later using the same candidate set mechanism
- Nested WITHIN (TRAVERSE within TRAVERSE) — use depth range instead
- General CTE/subquery syntax — wait for demand
- `SEARCH ... WITHIN (SEARCH ...)` — vector-scoped vector search, unclear use case
- New Plan variant — extends existing SearchPlan, no Composed/Pipeline plan type
- Multi-node scatter-gather with WITHIN — in a distributed deployment, the TRAVERSE sub-plan runs on the local node's AdjacencyIndex which may not have edges for entities on other nodes. Distributed graph-scoped search would require scatter-gather of the TRAVERSE step, which is a separate feature. Single-node deployments (including the current Johnny benchmark setup) are unaffected.
