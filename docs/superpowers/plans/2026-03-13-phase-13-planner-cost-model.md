# Phase 13: Planner & Cost Model -- Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the current rule-based planner with an ACU (Abstract Cost Unit) cost model. Enable the engine to make intelligent plan selection, warn about degraded plans, and surface costs via EXPLAIN. Wire up the existing MAX_ACU query hint so the planner can enforce a cost budget.

**Architecture:** The cost model is layered on top of the existing planner. A `CostProvider` trait defines per-operation ACU costs. `ConstantCostProvider` ships as the sole implementation (hardcoded defaults). The planner annotates every plan with an `AcuEstimate`, each plan carries optional `PlanWarning`s, and the executor threads warnings through to `QueryResult`. EXPLAIN shows cost breakdown. Five optimisation rules run in sequence, each individually disableable.

**Tech Stack:** Rust 2021, async (Tokio runtime), DashMap, Fjall, hnsw_rs.

**Build command:** `CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace`

**Known flaky test:** `similarity_score_range` -- pre-existing, ignore.

---

## File Structure

| File | Role | Tasks |
|------|------|-------|
| `crates/trondb-core/src/cost.rs` | NEW: CostProvider trait, ConstantCostProvider, AcuEstimate, AcuBreakdown | 1 |
| `crates/trondb-core/src/warning.rs` | NEW: PlanWarning, WarningSeverity | 2 |
| `crates/trondb-core/src/optimise.rs` | NEW: OptimisationRule trait, 5 rule implementations, apply_rules() | 5-9 |
| `crates/trondb-core/src/planner.rs` | Modify: integrate CostProvider, AcuEstimate on plans, MAX_ACU enforcement | 3 |
| `crates/trondb-core/src/result.rs` | Modify: add warnings + cost fields to QueryResult/QueryStats | 2 |
| `crates/trondb-core/src/executor.rs` | Modify: thread cost/warnings through, EXPLAIN cost breakdown, two-pass SEARCH | 4, 10 |
| `crates/trondb-core/src/lib.rs` | Modify: add `pub mod cost;`, `pub mod warning;`, `pub mod optimise;` | 1 |
| `crates/trondb-core/src/error.rs` | Modify: add `AcuBudgetExceeded` variant | 3 |
| `crates/trondb-proto/proto/trondb.proto` | Modify: add cost/warning proto messages | 11 |
| `crates/trondb-proto/src/convert_plan.rs` | Modify: bidirectional conversions for cost/warning | 11 |

---

## Chunk 1: CostProvider Trait & ConstantCostProvider

Foundation of the entire cost model. Defines the ACU cost table and the data structures for cost estimates.

### Task 1: CostProvider, AcuEstimate, ConstantCostProvider

**Files:**
- Create: `crates/trondb-core/src/cost.rs`
- Modify: `crates/trondb-core/src/lib.rs`

- [ ] **Step 1: Register the new module**

In `crates/trondb-core/src/lib.rs`, add the module declaration after the existing `pub mod vectoriser;` line:

```rust
pub mod cost;
```

- [ ] **Step 2: Write failing tests**

Create `crates/trondb-core/src/cost.rs` with the full module. Start with tests at the bottom:

```rust
// ---------------------------------------------------------------------------
// ACU Cost Model — Abstract Cost Units
// ---------------------------------------------------------------------------
//
// Base calibration: 1 hot-tier FETCH = 1.0 ACU.
// All other operations are multiples of this baseline.

use std::fmt;

// ---------------------------------------------------------------------------
// AcuBreakdown — itemised cost for a single plan
// ---------------------------------------------------------------------------

/// One line-item in an ACU cost breakdown.
#[derive(Debug, Clone)]
pub struct AcuLineItem {
    pub operation: String,
    pub count: usize,
    pub unit_cost: f64,
    pub total: f64,
}

/// Full cost breakdown for a planned query.
#[derive(Debug, Clone)]
pub struct AcuEstimate {
    pub items: Vec<AcuLineItem>,
    pub total_acu: f64,
}

impl AcuEstimate {
    pub fn zero() -> Self {
        Self {
            items: Vec::new(),
            total_acu: 0.0,
        }
    }

    pub fn single(operation: &str, count: usize, unit_cost: f64) -> Self {
        let total = count as f64 * unit_cost;
        Self {
            items: vec![AcuLineItem {
                operation: operation.to_string(),
                count,
                unit_cost,
                total,
            }],
            total_acu: total,
        }
    }

    pub fn add(&mut self, operation: &str, count: usize, unit_cost: f64) {
        let total = count as f64 * unit_cost;
        self.items.push(AcuLineItem {
            operation: operation.to_string(),
            count,
            unit_cost,
            total,
        });
        self.total_acu += total;
    }

    /// Merge another estimate into this one.
    pub fn merge(&mut self, other: &AcuEstimate) {
        self.items.extend(other.items.iter().cloned());
        self.total_acu += other.total_acu;
    }
}

impl fmt::Display for AcuEstimate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for item in &self.items {
            writeln!(
                f,
                "  {} x {} @ {:.1} ACU = {:.1} ACU",
                item.count, item.operation, item.unit_cost, item.total
            )?;
        }
        write!(f, "  TOTAL: {:.1} ACU", self.total_acu)
    }
}

// ---------------------------------------------------------------------------
// CostProvider trait
// ---------------------------------------------------------------------------

/// Provides per-operation ACU costs. Trait methods return defaults;
/// implementations override as needed. Statistics hooks are built in
/// for a future runtime-stats-backed provider.
pub trait CostProvider: Send + Sync {
    /// Cost of fetching one entity from the hot tier (Fjall/Ram).
    fn fetch_hot_acu(&self) -> f64 {
        1.0
    }

    /// Cost of fetching one entity from the warm tier (NVMe, Int8 dequantise).
    fn fetch_warm_acu(&self) -> f64 {
        25.0
    }

    /// Cost of fetching one entity from the archive tier (Binary, low-quality).
    fn fetch_archive_acu(&self) -> f64 {
        100.0
    }

    /// Cost of one HNSW search (per collection, per representation).
    fn search_hnsw_acu(&self) -> f64 {
        50.0
    }

    /// Cost of one sparse index search.
    fn search_sparse_acu(&self) -> f64 {
        20.0
    }

    /// Cost of a hybrid search (RRF merge of dense + sparse).
    fn search_hybrid_acu(&self) -> f64 {
        75.0
    }

    /// Cost of a natural language search (vectorise query + HNSW).
    fn search_natural_language_acu(&self) -> f64 {
        80.0
    }

    /// Cost of a full collection scan per entity.
    fn full_scan_per_entity_acu(&self) -> f64 {
        0.5
    }

    /// Cost of a field index lookup (point or range).
    fn field_index_lookup_acu(&self) -> f64 {
        2.0
    }

    /// Cost of one TRAVERSE hop through the adjacency index.
    fn traverse_hop_acu(&self) -> f64 {
        3.0
    }

    /// Cost of an INFER operation (vector search + scoring).
    fn infer_acu(&self) -> f64 {
        60.0
    }

    /// Cost of a scalar pre-filter pass (field index + over-fetch).
    fn pre_filter_acu(&self) -> f64 {
        5.0
    }

    /// Cost of the two-pass rescore step (binary/Int8 first pass, full-precision second).
    fn two_pass_rescore_acu(&self) -> f64 {
        15.0
    }

    /// Cost of promoting a warm entity to hot (dequantise + HNSW re-insert).
    fn promotion_acu(&self) -> f64 {
        30.0
    }

    /// Write operations have a fixed base cost.
    fn write_base_acu(&self) -> f64 {
        5.0
    }
}

// ---------------------------------------------------------------------------
// ConstantCostProvider — all defaults, no runtime stats
// ---------------------------------------------------------------------------

/// Ships with Phase 13. Uses the trait defaults. A future
/// `StatisticalCostProvider` will override based on runtime metrics.
#[derive(Debug, Clone, Default)]
pub struct ConstantCostProvider;

impl CostProvider for ConstantCostProvider {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_provider_defaults() {
        let p = ConstantCostProvider;
        assert!((p.fetch_hot_acu() - 1.0).abs() < f64::EPSILON);
        assert!((p.fetch_warm_acu() - 25.0).abs() < f64::EPSILON);
        assert!((p.fetch_archive_acu() - 100.0).abs() < f64::EPSILON);
        assert!((p.search_hnsw_acu() - 50.0).abs() < f64::EPSILON);
        assert!((p.search_sparse_acu() - 20.0).abs() < f64::EPSILON);
        assert!((p.search_hybrid_acu() - 75.0).abs() < f64::EPSILON);
        assert!((p.search_natural_language_acu() - 80.0).abs() < f64::EPSILON);
        assert!((p.full_scan_per_entity_acu() - 0.5).abs() < f64::EPSILON);
        assert!((p.field_index_lookup_acu() - 2.0).abs() < f64::EPSILON);
        assert!((p.traverse_hop_acu() - 3.0).abs() < f64::EPSILON);
        assert!((p.infer_acu() - 60.0).abs() < f64::EPSILON);
        assert!((p.pre_filter_acu() - 5.0).abs() < f64::EPSILON);
        assert!((p.two_pass_rescore_acu() - 15.0).abs() < f64::EPSILON);
        assert!((p.promotion_acu() - 30.0).abs() < f64::EPSILON);
        assert!((p.write_base_acu() - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn acu_estimate_zero() {
        let est = AcuEstimate::zero();
        assert_eq!(est.items.len(), 0);
        assert!((est.total_acu - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn acu_estimate_single() {
        let est = AcuEstimate::single("fetch_hot", 10, 1.0);
        assert_eq!(est.items.len(), 1);
        assert!((est.total_acu - 10.0).abs() < f64::EPSILON);
        assert_eq!(est.items[0].operation, "fetch_hot");
        assert_eq!(est.items[0].count, 10);
    }

    #[test]
    fn acu_estimate_add() {
        let mut est = AcuEstimate::zero();
        est.add("search_hnsw", 1, 50.0);
        est.add("fetch_hot", 10, 1.0);
        assert_eq!(est.items.len(), 2);
        assert!((est.total_acu - 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn acu_estimate_merge() {
        let mut est1 = AcuEstimate::single("search_hnsw", 1, 50.0);
        let est2 = AcuEstimate::single("fetch_hot", 10, 1.0);
        est1.merge(&est2);
        assert_eq!(est1.items.len(), 2);
        assert!((est1.total_acu - 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn acu_estimate_display() {
        let mut est = AcuEstimate::zero();
        est.add("search_hnsw", 1, 50.0);
        est.add("fetch_hot", 5, 1.0);
        let display = format!("{}", est);
        assert!(display.contains("search_hnsw"));
        assert!(display.contains("fetch_hot"));
        assert!(display.contains("TOTAL: 55.0 ACU"));
    }

    #[test]
    fn custom_cost_provider() {
        struct ExpensiveProvider;
        impl CostProvider for ExpensiveProvider {
            fn fetch_hot_acu(&self) -> f64 {
                10.0
            }
        }
        let p = ExpensiveProvider;
        assert!((p.fetch_hot_acu() - 10.0).abs() < f64::EPSILON);
        // Non-overridden methods still use defaults
        assert!((p.fetch_warm_acu() - 25.0).abs() < f64::EPSILON);
    }
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core cost::tests`

Expected: PASS -- the module is self-contained, no dependencies on other new code.

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-core/src/cost.rs crates/trondb-core/src/lib.rs
git commit -m "phase 13: add CostProvider trait, ConstantCostProvider, AcuEstimate"
```

---

## Chunk 2: PlanWarning System & QueryResult Integration

Degraded plans emit warnings attached to the query result.

### Task 2: PlanWarning + QueryResult/QueryStats Changes

**Files:**
- Create: `crates/trondb-core/src/warning.rs`
- Modify: `crates/trondb-core/src/lib.rs`
- Modify: `crates/trondb-core/src/result.rs`

- [ ] **Step 1: Register the new module**

In `crates/trondb-core/src/lib.rs`, add after the `pub mod cost;` line:

```rust
pub mod warning;
```

- [ ] **Step 2: Create warning.rs with PlanWarning**

Create `crates/trondb-core/src/warning.rs`:

```rust
// ---------------------------------------------------------------------------
// PlanWarning — degraded plan notifications
// ---------------------------------------------------------------------------
//
// The engine always tries to give an answer. When the answer is expensive
// or suboptimal, it tells you what happened and how to fix it.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarningSeverity {
    /// Informational: the plan is fine but could be cheaper.
    Info,
    /// Warning: the plan is significantly more expensive than optimal.
    Warning,
    /// Critical: the plan is extremely expensive or lossy.
    Critical,
}

impl fmt::Display for WarningSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WarningSeverity::Info => write!(f, "INFO"),
            WarningSeverity::Warning => write!(f, "WARN"),
            WarningSeverity::Critical => write!(f, "CRIT"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlanWarning {
    pub severity: WarningSeverity,
    pub message: String,
    pub suggestion: Option<String>,
    pub acu_impact: Option<f64>,
}

impl PlanWarning {
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            severity: WarningSeverity::Info,
            message: message.into(),
            suggestion: None,
            acu_impact: None,
        }
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            severity: WarningSeverity::Warning,
            message: message.into(),
            suggestion: None,
            acu_impact: None,
        }
    }

    pub fn critical(message: impl Into<String>) -> Self {
        Self {
            severity: WarningSeverity::Critical,
            message: message.into(),
            suggestion: None,
            acu_impact: None,
        }
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }

    pub fn with_acu_impact(mut self, acu: f64) -> Self {
        self.acu_impact = Some(acu);
        self
    }
}

impl fmt::Display for PlanWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.severity, self.message)?;
        if let Some(acu) = self.acu_impact {
            write!(f, " (+{:.1} ACU)", acu)?;
        }
        if let Some(ref sugg) = self.suggestion {
            write!(f, " -- suggestion: {}", sugg)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_builder() {
        let w = PlanWarning::warning("full scan on 10k entities")
            .with_suggestion("add an index on 'city'")
            .with_acu_impact(5000.0);
        assert_eq!(w.severity, WarningSeverity::Warning);
        assert!(w.message.contains("full scan"));
        assert!(w.suggestion.as_ref().unwrap().contains("index"));
        assert!((w.acu_impact.unwrap() - 5000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn warning_display() {
        let w = PlanWarning::critical("archive-only results")
            .with_acu_impact(100.0)
            .with_suggestion("PROMOTE hot entities");
        let s = format!("{}", w);
        assert!(s.contains("[CRIT]"));
        assert!(s.contains("archive-only"));
        assert!(s.contains("+100.0 ACU"));
        assert!(s.contains("PROMOTE"));
    }

    #[test]
    fn info_no_suggestion() {
        let w = PlanWarning::info("using over-fetch 4x for pre-filter");
        let s = format!("{}", w);
        assert!(s.contains("[INFO]"));
        assert!(!s.contains("suggestion"));
    }
}
```

- [ ] **Step 3: Extend QueryResult and QueryStats**

In `crates/trondb-core/src/result.rs`, add the new fields:

Add this import at the top of the file:

```rust
use crate::cost::AcuEstimate;
use crate::warning::PlanWarning;
```

Add two new fields to `QueryStats`:

```rust
#[derive(Debug, Clone)]
pub struct QueryStats {
    pub elapsed: Duration,
    pub entities_scanned: usize,
    pub mode: QueryMode,
    pub tier: String,
    pub cost: Option<AcuEstimate>,
    pub warnings: Vec<PlanWarning>,
}
```

This is a breaking change to every `QueryStats { ... }` construction in the codebase. Every existing construction must add:

```rust
cost: None,
warnings: vec![],
```

There are many call sites in `executor.rs`. Use find-and-replace for the pattern. Every occurrence of:

```
tier: "Fjall".into(),
```

...or:

```
tier: "FieldIndex".into(),
```

...or:

```
tier: "Ram".into(),
```

...followed by the closing `}` of a QueryStats struct, needs the two new fields. Search for `QueryStats {` in executor.rs and add the fields to each one. There should be approximately 15-20 occurrences in executor.rs, plus 1 or 2 in other files.

- [ ] **Step 4: Fix all compilation errors**

Search the workspace for all `QueryStats {` constructions and add the two new fields. Files to check:

- `crates/trondb-core/src/executor.rs` -- the bulk (many arms of `execute()`)
- `crates/trondb-proto/src/convert_plan.rs` (if it constructs QueryStats)
- `crates/trondb-routing/` (if it constructs QueryStats)

For every instance, add:

```rust
cost: None,
warnings: vec![],
```

- [ ] **Step 5: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace`

Expected: PASS -- all existing tests compile and pass with the new fields defaulted.

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/src/warning.rs crates/trondb-core/src/lib.rs crates/trondb-core/src/result.rs crates/trondb-core/src/executor.rs
# Add any other files that needed QueryStats fixes
git commit -m "phase 13: add PlanWarning system, extend QueryStats with cost + warnings"
```

---

## Chunk 3: Planner Cost Integration & MAX_ACU Enforcement

The planner gains a `CostProvider` reference and computes `AcuEstimate` for every plan. MAX_ACU hint becomes functional.

### Task 3: Planner Gains CostProvider + AcuEstimate

**Files:**
- Modify: `crates/trondb-core/src/planner.rs`
- Modify: `crates/trondb-core/src/error.rs`

- [ ] **Step 1: Add AcuBudgetExceeded error variant**

In `crates/trondb-core/src/error.rs`, add to the `EngineError` enum:

```rust
#[error("ACU budget exceeded: estimated {estimated:.1} ACU, budget {budget:.1} ACU")]
AcuBudgetExceeded { estimated: f64, budget: f64 },
```

- [ ] **Step 2: Write failing tests for cost estimation**

In `crates/trondb-core/src/planner.rs`, add to the `#[cfg(test)] mod tests` block:

```rust
use crate::cost::{AcuEstimate, ConstantCostProvider, CostProvider};

#[test]
fn estimate_fetch_full_scan_cost() {
    let provider = ConstantCostProvider;
    let plan = Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: None,
        order_by: vec![],
        limit: None,
        strategy: FetchStrategy::FullScan,
        hints: vec![],
    });
    let est = estimate_plan_cost(&plan, &provider, 1000);
    // FullScan on 1000 entities: 1000 * 0.5 = 500 ACU
    assert!((est.total_acu - 500.0).abs() < f64::EPSILON);
}

#[test]
fn estimate_fetch_field_index_cost() {
    let provider = ConstantCostProvider;
    let plan = Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: None,
        order_by: vec![],
        limit: None,
        strategy: FetchStrategy::FieldIndexLookup("idx_city".into()),
        hints: vec![],
    });
    let est = estimate_plan_cost(&plan, &provider, 1000);
    // FieldIndexLookup: 2.0 ACU
    assert!((est.total_acu - 2.0).abs() < f64::EPSILON);
}

#[test]
fn estimate_search_hnsw_cost() {
    let provider = ConstantCostProvider;
    let plan = Plan::Search(SearchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        dense_vector: Some(vec![1.0, 0.0, 0.0]),
        sparse_vector: None,
        filter: None,
        pre_filter: None,
        k: 10,
        confidence_threshold: 0.8,
        strategy: SearchStrategy::Hnsw,
        query_text: None,
        using_repr: None,
        hints: vec![],
    });
    let est = estimate_plan_cost(&plan, &provider, 1000);
    // HNSW search: 50.0 + fetch k results: 10 * 1.0 = 60.0 ACU
    assert!((est.total_acu - 60.0).abs() < f64::EPSILON);
}

#[test]
fn estimate_search_with_pre_filter_cost() {
    let provider = ConstantCostProvider;
    let plan = Plan::Search(SearchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        dense_vector: Some(vec![1.0, 0.0, 0.0]),
        sparse_vector: None,
        filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
        pre_filter: Some(PreFilter {
            index_name: "idx_city".into(),
            clause: WhereClause::Eq("city".into(), Literal::String("London".into())),
        }),
        k: 10,
        confidence_threshold: 0.0,
        strategy: SearchStrategy::Hnsw,
        query_text: None,
        using_repr: None,
        hints: vec![],
    });
    let est = estimate_plan_cost(&plan, &provider, 1000);
    // HNSW: 50 + pre_filter: 5 + fetch k: 10*1 = 65 ACU
    assert!((est.total_acu - 65.0).abs() < f64::EPSILON);
}

#[test]
fn estimate_traverse_cost() {
    let provider = ConstantCostProvider;
    let plan = Plan::Traverse(TraversePlan {
        edge_type: "similar_to".into(),
        from_id: "v1".into(),
        depth: 3,
        limit: None,
    });
    let est = estimate_plan_cost(&plan, &provider, 0);
    // 3 hops * 3.0 ACU = 9.0 ACU
    assert!((est.total_acu - 9.0).abs() < f64::EPSILON);
}

#[test]
fn max_acu_budget_enforced() {
    let provider = ConstantCostProvider;
    let plan = Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: None,
        order_by: vec![],
        limit: None,
        strategy: FetchStrategy::FullScan,
        hints: vec![QueryHint::MaxAcu(10.0)],
    });
    let est = estimate_plan_cost(&plan, &provider, 1000);
    let result = check_acu_budget(&plan, &est);
    assert!(result.is_err());
    match result.unwrap_err() {
        EngineError::AcuBudgetExceeded { estimated, budget } => {
            assert!((estimated - 500.0).abs() < f64::EPSILON);
            assert!((budget - 10.0).abs() < f64::EPSILON);
        }
        other => panic!("expected AcuBudgetExceeded, got {:?}", other),
    }
}

#[test]
fn max_acu_budget_within_limit_passes() {
    let provider = ConstantCostProvider;
    let plan = Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: None,
        order_by: vec![],
        limit: None,
        strategy: FetchStrategy::FieldIndexLookup("idx_city".into()),
        hints: vec![QueryHint::MaxAcu(10.0)],
    });
    let est = estimate_plan_cost(&plan, &provider, 1000);
    let result = check_acu_budget(&plan, &est);
    assert!(result.is_ok());
}

#[test]
fn estimate_write_plans_have_base_cost() {
    let provider = ConstantCostProvider;
    let plan = Plan::Insert(InsertPlan {
        collection: "venues".into(),
        fields: vec![],
        values: vec![],
        vectors: vec![],
        collocate_with: None,
        affinity_group: None,
    });
    let est = estimate_plan_cost(&plan, &provider, 0);
    assert!((est.total_acu - 5.0).abs() < f64::EPSILON);
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core planner::tests::estimate`

Expected: FAIL -- `estimate_plan_cost` and `check_acu_budget` do not exist.

- [ ] **Step 4: Implement estimate_plan_cost and check_acu_budget**

Add to `crates/trondb-core/src/planner.rs`, above the tests module:

```rust
use crate::cost::{AcuEstimate, CostProvider};

// ---------------------------------------------------------------------------
// Cost estimation
// ---------------------------------------------------------------------------

/// Estimate the ACU cost of a plan. `collection_size` is the approximate
/// number of entities in the target collection (0 if unknown or not applicable).
pub fn estimate_plan_cost(
    plan: &Plan,
    cost: &dyn CostProvider,
    collection_size: usize,
) -> AcuEstimate {
    match plan {
        Plan::Fetch(p) => {
            let mut est = AcuEstimate::zero();
            match &p.strategy {
                FetchStrategy::FullScan => {
                    est.add("full_scan", collection_size, cost.full_scan_per_entity_acu());
                }
                FetchStrategy::FieldIndexLookup(_) => {
                    est.add("field_index_lookup", 1, cost.field_index_lookup_acu());
                }
                FetchStrategy::FieldIndexRange(_) => {
                    est.add("field_index_lookup", 1, cost.field_index_lookup_acu());
                }
            }
            est
        }
        Plan::Search(p) => {
            let mut est = AcuEstimate::zero();
            match &p.strategy {
                SearchStrategy::Hnsw => {
                    est.add("search_hnsw", 1, cost.search_hnsw_acu());
                }
                SearchStrategy::Sparse => {
                    est.add("search_sparse", 1, cost.search_sparse_acu());
                }
                SearchStrategy::Hybrid => {
                    est.add("search_hybrid", 1, cost.search_hybrid_acu());
                }
                SearchStrategy::NaturalLanguage => {
                    est.add("search_natural_language", 1, cost.search_natural_language_acu());
                }
            }
            if p.pre_filter.is_some() {
                est.add("pre_filter", 1, cost.pre_filter_acu());
            }
            // Post-search: fetching k result entities from hot tier
            est.add("fetch_hot", p.k, cost.fetch_hot_acu());
            est
        }
        Plan::Traverse(p) => {
            let mut est = AcuEstimate::zero();
            est.add("traverse_hop", p.depth, cost.traverse_hop_acu());
            est
        }
        Plan::Infer(_) => {
            AcuEstimate::single("infer", 1, cost.infer_acu())
        }
        // Write operations
        Plan::Insert(_) | Plan::CreateCollection(_) | Plan::CreateEdgeType(_)
        | Plan::InsertEdge(_) | Plan::DeleteEntity(_) | Plan::DeleteEdge(_)
        | Plan::CreateAffinityGroup(_) | Plan::AlterEntityDropAffinity(_)
        | Plan::Demote(_) | Plan::Promote(_) | Plan::UpdateEntity(_)
        | Plan::ConfirmEdge(_) | Plan::DropCollection(_) | Plan::DropEdgeType(_) => {
            AcuEstimate::single("write", 1, cost.write_base_acu())
        }
        // Metadata / explain operations are free
        Plan::Explain(_) | Plan::ExplainTiers(_) | Plan::ExplainHistory(_) => {
            AcuEstimate::zero()
        }
    }
}

/// Extract MAX_ACU hint from a plan's hint list.
fn extract_max_acu(plan: &Plan) -> Option<f64> {
    let hints = match plan {
        Plan::Fetch(p) => &p.hints,
        Plan::Search(p) => &p.hints,
        _ => return None,
    };
    hints.iter().find_map(|h| match h {
        QueryHint::MaxAcu(v) => Some(*v),
        _ => None,
    })
}

/// Check if the estimated cost exceeds the MAX_ACU budget (if set).
/// Returns Ok(()) if within budget or no budget set.
pub fn check_acu_budget(plan: &Plan, estimate: &AcuEstimate) -> Result<(), EngineError> {
    if let Some(budget) = extract_max_acu(plan) {
        if estimate.total_acu > budget {
            return Err(EngineError::AcuBudgetExceeded {
                estimated: estimate.total_acu,
                budget,
            });
        }
    }
    Ok(())
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core planner::tests`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/trondb-core/src/planner.rs crates/trondb-core/src/error.rs
git commit -m "phase 13: planner cost estimation + MAX_ACU budget enforcement"
```

---

## Chunk 4: Executor Integration -- Cost in EXPLAIN + Budget Check

The executor computes costs before execution, attaches them to QueryStats, and enriches EXPLAIN output.

### Task 4: Executor Cost Threading + EXPLAIN Enhancement

**Files:**
- Modify: `crates/trondb-core/src/executor.rs`

- [ ] **Step 1: Write failing tests**

Add to the test module in `crates/trondb-core/src/executor.rs`:

```rust
#[tokio::test]
async fn explain_shows_cost_breakdown() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "venues", 3).await;

    // Insert a few entities so we have a non-zero collection size
    insert_entity(&exec, "venues", "v1", "The Fleece", vec![1.0, 0.0, 0.0]).await;
    insert_entity(&exec, "venues", "v2", "The Exchange", vec![0.0, 1.0, 0.0]).await;

    let search_plan = Plan::Search(SearchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        dense_vector: Some(vec![1.0, 0.0, 0.0]),
        sparse_vector: None,
        filter: None,
        pre_filter: None,
        k: 10,
        confidence_threshold: 0.0,
        strategy: SearchStrategy::Hnsw,
        query_text: None,
        using_repr: None,
        hints: vec![],
    });

    let result = exec
        .execute(&Plan::Explain(Box::new(search_plan)))
        .await
        .unwrap();

    // Should have cost-related rows in the EXPLAIN output
    let has_cost_row = result.rows.iter().any(|r| {
        r.values.get("property").map(|v| v.to_string()) == Some("estimated_acu".to_string())
    });
    assert!(has_cost_row, "EXPLAIN should include estimated_acu");

    let has_breakdown = result.rows.iter().any(|r| {
        r.values.get("property").map(|v| v.to_string()) == Some("cost_breakdown".to_string())
    });
    assert!(has_breakdown, "EXPLAIN should include cost_breakdown");
}

#[tokio::test]
async fn max_acu_rejects_expensive_query() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "venues", 3).await;

    // Insert enough entities that a full scan is expensive
    for i in 0..50 {
        insert_entity(
            &exec,
            "venues",
            &format!("v{i}"),
            &format!("Venue {i}"),
            vec![1.0, 0.0, 0.0],
        )
        .await;
    }

    let plan = Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: None,
        order_by: vec![],
        limit: None,
        strategy: FetchStrategy::FullScan,
        hints: vec![QueryHint::MaxAcu(5.0)], // budget of 5 ACU, but scan will cost 50*0.5=25 ACU
    });
    let result = exec.execute(&plan).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        EngineError::AcuBudgetExceeded { .. } => {}
        other => panic!("expected AcuBudgetExceeded, got {:?}", other),
    }
}

#[tokio::test]
async fn query_result_includes_cost() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "venues", 3).await;
    insert_entity(&exec, "venues", "v1", "The Fleece", vec![1.0, 0.0, 0.0]).await;

    let plan = Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: None,
        order_by: vec![],
        limit: None,
        strategy: FetchStrategy::FullScan,
        hints: vec![],
    });
    let result = exec.execute(&plan).await.unwrap();
    assert!(result.stats.cost.is_some(), "QueryStats should include cost estimate");
    let cost = result.stats.cost.unwrap();
    assert!(cost.total_acu > 0.0, "Cost should be non-zero for a scan");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core executor::tests::explain_shows_cost_breakdown executor::tests::max_acu_rejects_expensive_query executor::tests::query_result_includes_cost`

Expected: FAIL -- cost is always None, no ACU budget check, no cost rows in EXPLAIN.

- [ ] **Step 3: Add CostProvider to Executor and wire cost computation**

Add a field to the `Executor` struct:

```rust
use crate::cost::{AcuEstimate, ConstantCostProvider, CostProvider};
```

```rust
pub struct Executor {
    // ... existing fields ...
    cost_provider: Arc<dyn CostProvider>,
}
```

In `Executor::new()`, add the default provider:

```rust
cost_provider: Arc::new(ConstantCostProvider),
```

Add a helper method to get collection size:

```rust
impl Executor {
    /// Approximate collection size for cost estimation.
    fn collection_size(&self, collection: &str) -> usize {
        self.store.scan(collection).map(|v| v.len()).unwrap_or(0)
    }
}
```

**NOTE:** `store.scan()` is expensive for large collections. For Phase 13 this is acceptable because cost estimation only runs at plan time and we only have `ConstantCostProvider`. A future phase can add a cached entity count. An alternative is to check the HNSW index `len()` for the collection, which is O(1). Implementation may use whichever approach is simpler -- `scan().len()` is more accurate; HNSW `len()` is cheaper but only counts hot-tier dense-indexed entities. Prefer HNSW len if the collection has an index, fall back to scan otherwise:

```rust
fn collection_size(&self, collection: &str) -> usize {
    // Fast path: check HNSW index len (hot tier, O(1))
    let prefix = format!("{collection}:");
    for entry in self.indexes.iter() {
        if entry.key().starts_with(&prefix) {
            return entry.value().len();
        }
    }
    // Slow path: Fjall scan count
    self.store.scan(collection).map(|v| v.len()).unwrap_or(0)
}
```

Now, at the top of `execute()`, before the `match plan` block, add cost estimation and budget check for read queries:

```rust
pub async fn execute(&self, plan: &Plan) -> Result<QueryResult, EngineError> {
    let start = Instant::now();

    // Cost estimation + budget check (skip for EXPLAIN -- it reports cost, doesn't enforce)
    let cost_estimate = match plan {
        Plan::Explain(_) => None,
        _ => {
            let collection_name = plan_collection_name(plan);
            let size = collection_name
                .map(|c| self.collection_size(c))
                .unwrap_or(0);
            let est = crate::planner::estimate_plan_cost(plan, self.cost_provider.as_ref(), size);
            crate::planner::check_acu_budget(plan, &est)?;
            Some(est)
        }
    };

    match plan {
        // ... existing arms ...
    }
}
```

Add the helper:

```rust
/// Extract the primary collection name from a plan (for cost estimation).
fn plan_collection_name(plan: &Plan) -> Option<&str> {
    match plan {
        Plan::Fetch(p) => Some(&p.collection),
        Plan::Search(p) => Some(&p.collection),
        Plan::Insert(p) => Some(&p.collection),
        Plan::DeleteEntity(p) => Some(&p.collection),
        Plan::UpdateEntity(p) => Some(&p.collection),
        Plan::Demote(p) => Some(&p.collection),
        Plan::Promote(p) => Some(&p.collection),
        Plan::ExplainTiers(p) => Some(&p.collection),
        _ => None,
    }
}
```

Then, in every `Ok(QueryResult { ... })` return, replace:

```rust
cost: None,
```

with:

```rust
cost: cost_estimate.clone(),
```

For the `Plan::Explain` arm, enhance `explain_plan()` to include cost:

In the `Plan::Explain(inner)` arm, change to:

```rust
Plan::Explain(inner) => {
    let collection_name = plan_collection_name(inner);
    let size = collection_name
        .map(|c| self.collection_size(c))
        .unwrap_or(0);
    let est = crate::planner::estimate_plan_cost(inner, self.cost_provider.as_ref(), size);

    let mut rows = explain_plan(inner);

    // Add cost breakdown rows
    rows.push(Row {
        values: HashMap::from([
            ("property".into(), Value::String("estimated_acu".into())),
            ("value".into(), Value::String(format!("{:.1}", est.total_acu))),
        ]),
        score: None,
    });
    let breakdown_str = est.items.iter()
        .map(|item| format!("{}x {} @ {:.1} = {:.1}", item.count, item.operation, item.unit_cost, item.total))
        .collect::<Vec<_>>()
        .join("; ");
    rows.push(Row {
        values: HashMap::from([
            ("property".into(), Value::String("cost_breakdown".into())),
            ("value".into(), Value::String(breakdown_str)),
        ]),
        score: None,
    });

    Ok(QueryResult {
        columns: vec!["property".into(), "value".into()],
        rows,
        stats: QueryStats {
            elapsed: start.elapsed(),
            entities_scanned: 0,
            mode: QueryMode::Deterministic,
            tier: "Fjall".into(),
            cost: Some(est),
            warnings: vec![],
        },
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core executor::tests`

Expected: PASS -- all existing tests pass (cost is non-None but doesn't break anything), new tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "phase 13: executor cost integration, EXPLAIN cost breakdown, MAX_ACU enforcement"
```

---

## Chunk 5: Optimisation Rules Framework + ScalarPreFilter ACU Integration

Establish the optimisation rule framework and integrate the existing ScalarPreFilter with ACU costing.

### Task 5: Optimisation Rule Framework + ScalarPreFilter

**Files:**
- Create: `crates/trondb-core/src/optimise.rs`
- Modify: `crates/trondb-core/src/lib.rs`

- [ ] **Step 1: Register the new module**

In `crates/trondb-core/src/lib.rs`, add after `pub mod warning;`:

```rust
pub mod optimise;
```

- [ ] **Step 2: Create optimise.rs with rule framework and ScalarPreFilter**

Create `crates/trondb-core/src/optimise.rs`:

```rust
// ---------------------------------------------------------------------------
// Optimisation Rules
// ---------------------------------------------------------------------------
//
// Five rules, all enabled by default, all individually disableable.
// Each rule takes a Plan + context and returns a (possibly modified) Plan
// plus any PlanWarnings generated.

use crate::cost::{AcuEstimate, CostProvider};
use crate::planner::{Plan, SearchPlan, SearchStrategy, FetchPlan, FetchStrategy, TraversePlan};
use crate::warning::PlanWarning;
use trondb_tql::QueryHint;

// ---------------------------------------------------------------------------
// Rule config
// ---------------------------------------------------------------------------

/// Controls which optimisation rules are active.
#[derive(Debug, Clone)]
pub struct OptimiserConfig {
    pub scalar_pre_filter: bool,
    pub confidence_pushdown: bool,
    pub traverse_hop_reorder: bool,
    pub on_demand_promotion: bool,
    pub batched_fetch_after_search: bool,
}

impl Default for OptimiserConfig {
    fn default() -> Self {
        Self {
            scalar_pre_filter: true,
            confidence_pushdown: true,
            traverse_hop_reorder: true,
            on_demand_promotion: true,
            batched_fetch_after_search: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Rule results
// ---------------------------------------------------------------------------

/// The output of running optimisation rules on a plan.
#[derive(Debug, Clone)]
pub struct OptimisedPlan {
    pub plan: Plan,
    pub warnings: Vec<PlanWarning>,
    pub rules_applied: Vec<String>,
}

// ---------------------------------------------------------------------------
// apply_rules — run all enabled rules in sequence
// ---------------------------------------------------------------------------

pub fn apply_rules(
    plan: Plan,
    config: &OptimiserConfig,
    cost_provider: &dyn CostProvider,
    collection_size: usize,
) -> OptimisedPlan {
    let mut result = OptimisedPlan {
        plan,
        warnings: Vec::new(),
        rules_applied: Vec::new(),
    };

    if config.scalar_pre_filter {
        apply_scalar_pre_filter(&mut result, cost_provider, collection_size);
    }
    if config.confidence_pushdown {
        apply_confidence_pushdown(&mut result, cost_provider);
    }
    if config.traverse_hop_reorder {
        apply_traverse_hop_reorder(&mut result, cost_provider);
    }
    if config.on_demand_promotion {
        apply_on_demand_promotion(&mut result, cost_provider);
    }
    if config.batched_fetch_after_search {
        apply_batched_fetch_after_search(&mut result, cost_provider);
    }

    result
}

// ---------------------------------------------------------------------------
// Rule 1: ScalarPreFilter ACU integration
// ---------------------------------------------------------------------------
// The pre-filter itself is already selected by the planner. This rule
// generates warnings when:
// - A SEARCH has a WHERE but no pre-filter (unindexed field, forced off by hint)
// - The full-scan fallback will be expensive

fn apply_scalar_pre_filter(
    result: &mut OptimisedPlan,
    cost_provider: &dyn CostProvider,
    collection_size: usize,
) {
    if let Plan::Search(ref p) = result.plan {
        if p.filter.is_some() && p.pre_filter.is_none() {
            // SEARCH with WHERE but no pre-filter -- post-filter only
            let scan_cost = collection_size as f64 * cost_provider.full_scan_per_entity_acu();
            if scan_cost > 100.0 {
                result.warnings.push(
                    PlanWarning::warning(format!(
                        "SEARCH WHERE without pre-filter: post-filtering {} entities ({:.0} ACU)",
                        collection_size, scan_cost
                    ))
                    .with_suggestion("CREATE INDEX on the filtered field to enable ScalarPreFilter")
                    .with_acu_impact(scan_cost),
                );
            }
            result.rules_applied.push("ScalarPreFilter(warn:no_index)".into());
        } else if p.pre_filter.is_some() {
            result.rules_applied.push("ScalarPreFilter(active)".into());
        }
    }
}

// ---------------------------------------------------------------------------
// Rule 2: ConfidencePushdown
// ---------------------------------------------------------------------------
// If the SEARCH has a high confidence threshold, we could theoretically
// tell HNSW to use a lower ef_search. For now, this rule only generates
// advisory info when confidence > 0.9 (high selectivity = fewer candidates).

fn apply_confidence_pushdown(
    result: &mut OptimisedPlan,
    _cost_provider: &dyn CostProvider,
) {
    if let Plan::Search(ref p) = result.plan {
        if p.confidence_threshold > 0.9 {
            result.warnings.push(
                PlanWarning::info(format!(
                    "high confidence threshold ({:.2}) enables early termination",
                    p.confidence_threshold
                ))
            );
            result.rules_applied.push("ConfidencePushdown(advisory)".into());
        }
    }
}

// ---------------------------------------------------------------------------
// Rule 3: TraverseHopReorder
// ---------------------------------------------------------------------------
// For TRAVERSE queries, warn about deep traversals (expensive).
// Future: reorder edges by structural-first (cheap) vs inferred (expensive).

fn apply_traverse_hop_reorder(
    result: &mut OptimisedPlan,
    cost_provider: &dyn CostProvider,
) {
    if let Plan::Traverse(ref p) = result.plan {
        let hop_cost = p.depth as f64 * cost_provider.traverse_hop_acu();
        if p.depth > 5 {
            result.warnings.push(
                PlanWarning::warning(format!(
                    "deep TRAVERSE (depth={}) costs {:.0} ACU",
                    p.depth, hop_cost
                ))
                .with_suggestion("reduce depth or add LIMIT")
                .with_acu_impact(hop_cost),
            );
        }
        result.rules_applied.push("TraverseHopReorder(checked)".into());
    }
}

// ---------------------------------------------------------------------------
// Rule 4: OnDemandPromotion
// ---------------------------------------------------------------------------
// When a FETCH targets entities that may be in warm/archive tiers,
// warn about promotion cost. The actual promotion is handled by the
// executor (warm->hot on access). This rule warns if NO_PROMOTE is
// not set and the query might hit warm-tier entities.

fn apply_on_demand_promotion(
    result: &mut OptimisedPlan,
    cost_provider: &dyn CostProvider,
) {
    if let Plan::Fetch(ref p) = result.plan {
        let has_no_promote = p.hints.contains(&QueryHint::NoPromote);
        if !has_no_promote {
            // We can't know at plan time whether entities are warm, but
            // we can inform the user that promotion may occur.
            result.rules_applied.push("OnDemandPromotion(enabled)".into());
        } else {
            result.warnings.push(
                PlanWarning::info(
                    "NO_PROMOTE active: warm-tier entities will be served dequantised without promotion"
                )
            );
            result.rules_applied.push("OnDemandPromotion(suppressed)".into());
        }
    }
}

// ---------------------------------------------------------------------------
// Rule 5: BatchedFetchAfterSearch
// ---------------------------------------------------------------------------
// After a SEARCH returns k candidates, we need to fetch their full data.
// When k > 50, warn about batching cost.

fn apply_batched_fetch_after_search(
    result: &mut OptimisedPlan,
    cost_provider: &dyn CostProvider,
) {
    if let Plan::Search(ref p) = result.plan {
        let fetch_cost = p.k as f64 * cost_provider.fetch_hot_acu();
        if p.k > 50 {
            result.warnings.push(
                PlanWarning::info(format!(
                    "large result set (k={}): batched fetch costs {:.0} ACU",
                    p.k, fetch_cost
                ))
                .with_acu_impact(fetch_cost),
            );
        }
        result.rules_applied.push("BatchedFetchAfterSearch(checked)".into());
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::ConstantCostProvider;
    use crate::planner::PreFilter;
    use trondb_tql::{FieldList, Literal, WhereClause};

    fn default_config() -> OptimiserConfig {
        OptimiserConfig::default()
    }

    #[test]
    fn all_rules_enabled_by_default() {
        let config = default_config();
        assert!(config.scalar_pre_filter);
        assert!(config.confidence_pushdown);
        assert!(config.traverse_hop_reorder);
        assert!(config.on_demand_promotion);
        assert!(config.batched_fetch_after_search);
    }

    #[test]
    fn scalar_pre_filter_warns_on_missing_index() {
        let cost = ConstantCostProvider;
        let plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            pre_filter: None, // no pre-filter
            k: 10,
            confidence_threshold: 0.0,
            strategy: SearchStrategy::Hnsw,
            query_text: None,
            using_repr: None,
            hints: vec![],
        });

        let result = apply_rules(plan, &default_config(), &cost, 10_000);
        // Should warn because 10k * 0.5 = 5000 ACU > 100
        assert!(result.warnings.iter().any(|w| w.message.contains("pre-filter")));
        assert!(result.rules_applied.iter().any(|r| r.contains("ScalarPreFilter")));
    }

    #[test]
    fn scalar_pre_filter_no_warn_with_filter() {
        let cost = ConstantCostProvider;
        let plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            pre_filter: Some(PreFilter {
                index_name: "idx_city".into(),
                clause: WhereClause::Eq("city".into(), Literal::String("London".into())),
            }),
            k: 10,
            confidence_threshold: 0.0,
            strategy: SearchStrategy::Hnsw,
            query_text: None,
            using_repr: None,
            hints: vec![],
        });

        let result = apply_rules(plan, &default_config(), &cost, 10_000);
        // No warning -- pre-filter is active
        assert!(result.warnings.is_empty());
        assert!(result.rules_applied.iter().any(|r| r.contains("ScalarPreFilter(active)")));
    }

    #[test]
    fn confidence_pushdown_fires_on_high_threshold() {
        let cost = ConstantCostProvider;
        let plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: None,
            pre_filter: None,
            k: 10,
            confidence_threshold: 0.95,
            strategy: SearchStrategy::Hnsw,
            query_text: None,
            using_repr: None,
            hints: vec![],
        });

        let result = apply_rules(plan, &default_config(), &cost, 100);
        assert!(result.rules_applied.iter().any(|r| r.contains("ConfidencePushdown")));
        assert!(result.warnings.iter().any(|w| w.message.contains("early termination")));
    }

    #[test]
    fn traverse_warns_on_deep_depth() {
        let cost = ConstantCostProvider;
        let plan = Plan::Traverse(TraversePlan {
            edge_type: "similar_to".into(),
            from_id: "v1".into(),
            depth: 8,
            limit: None,
        });

        let result = apply_rules(plan, &default_config(), &cost, 0);
        assert!(result.warnings.iter().any(|w| w.message.contains("deep TRAVERSE")));
    }

    #[test]
    fn traverse_no_warn_on_shallow() {
        let cost = ConstantCostProvider;
        let plan = Plan::Traverse(TraversePlan {
            edge_type: "similar_to".into(),
            from_id: "v1".into(),
            depth: 2,
            limit: None,
        });

        let result = apply_rules(plan, &default_config(), &cost, 0);
        assert!(result.warnings.iter().all(|w| !w.message.contains("deep TRAVERSE")));
    }

    #[test]
    fn on_demand_promotion_suppressed_by_hint() {
        let cost = ConstantCostProvider;
        let plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            order_by: vec![],
            limit: None,
            strategy: FetchStrategy::FullScan,
            hints: vec![QueryHint::NoPromote],
        });

        let result = apply_rules(plan, &default_config(), &cost, 100);
        assert!(result.rules_applied.iter().any(|r| r.contains("suppressed")));
        assert!(result.warnings.iter().any(|w| w.message.contains("NO_PROMOTE")));
    }

    #[test]
    fn batched_fetch_warns_on_large_k() {
        let cost = ConstantCostProvider;
        let plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: None,
            pre_filter: None,
            k: 100,
            confidence_threshold: 0.0,
            strategy: SearchStrategy::Hnsw,
            query_text: None,
            using_repr: None,
            hints: vec![],
        });

        let result = apply_rules(plan, &default_config(), &cost, 100);
        assert!(result.warnings.iter().any(|w| w.message.contains("large result set")));
    }

    #[test]
    fn rules_can_be_disabled() {
        let cost = ConstantCostProvider;
        let mut config = OptimiserConfig::default();
        config.scalar_pre_filter = false;
        config.confidence_pushdown = false;
        config.traverse_hop_reorder = false;
        config.on_demand_promotion = false;
        config.batched_fetch_after_search = false;

        let plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: Some(WhereClause::Eq("city".into(), Literal::String("London".into()))),
            pre_filter: None,
            k: 100,
            confidence_threshold: 0.95,
            strategy: SearchStrategy::Hnsw,
            query_text: None,
            using_repr: None,
            hints: vec![],
        });

        let result = apply_rules(plan, &config, &cost, 10_000);
        assert!(result.rules_applied.is_empty());
        assert!(result.warnings.is_empty());
    }
}
```

- [ ] **Step 3: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core optimise::tests`

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-core/src/optimise.rs crates/trondb-core/src/lib.rs
git commit -m "phase 13: optimisation rule framework with 5 rules"
```

---

## Chunk 6: Two-Pass Query Strategy

For large candidate sets, SEARCH uses a two-pass strategy: first pass with quantised vectors for fast shortlisting, second pass with full-precision for rescoring.

### Task 6: Two-Pass Strategy in Planner + Executor

**Files:**
- Modify: `crates/trondb-core/src/planner.rs`
- Modify: `crates/trondb-core/src/executor.rs`

- [ ] **Step 1: Add TwoPassConfig to SearchPlan**

In `crates/trondb-core/src/planner.rs`, add the two-pass configuration:

```rust
/// Configuration for two-pass search strategy.
/// First pass: fast shortlisting with quantised vectors.
/// Second pass: rescore survivors with full-precision vectors.
#[derive(Debug, Clone)]
pub struct TwoPassConfig {
    /// Number of candidates to shortlist in the first pass.
    /// Typically 3-5x the final k.
    pub first_pass_k: usize,
    /// Whether to use binary quantised vectors for the first pass.
    /// false = Int8 quantised, true = binary quantised.
    pub use_binary_first_pass: bool,
}
```

Add the field to `SearchPlan`:

```rust
#[derive(Debug, Clone)]
pub struct SearchPlan {
    // ... existing fields ...
    pub two_pass: Option<TwoPassConfig>,
}
```

- [ ] **Step 2: Wire two-pass selection into the planner**

In the `Statement::Search` arm of `plan()`, after constructing the SearchPlan, add two-pass selection logic. The planner selects two-pass when k >= 50 (a tunable threshold):

```rust
let two_pass = if k >= 50 {
    Some(TwoPassConfig {
        first_pass_k: k * 3,
        use_binary_first_pass: false, // Int8 by default
    })
} else {
    None
};
```

Add `two_pass` to the `SearchPlan` construction.

- [ ] **Step 3: Fix all SearchPlan constructions**

Every existing `SearchPlan { ... }` in the codebase (tests, executor, proto conversions) needs `two_pass: None` added. Search for `SearchPlan {` across the workspace.

- [ ] **Step 4: Update cost estimation for two-pass**

In `estimate_plan_cost()` in `planner.rs`, add to the `Plan::Search` arm:

```rust
if p.two_pass.is_some() {
    est.add("two_pass_rescore", 1, cost.two_pass_rescore_acu());
}
```

- [ ] **Step 5: Write tests**

Add to `crates/trondb-core/src/planner.rs` tests:

```rust
#[test]
fn two_pass_selected_for_large_k() {
    let stmt = Statement::Search(SearchStmt {
        collection: "venues".into(),
        fields: FieldList::All,
        dense_vector: Some(vec![1.0, 0.0, 0.0]),
        sparse_vector: None,
        filter: None,
        confidence: None,
        limit: Some(100),
        query_text: None,
        using_repr: None,
        hints: vec![],
    });
    let p = plan(&stmt, &empty_schemas()).unwrap();
    match p {
        Plan::Search(sp) => {
            assert!(sp.two_pass.is_some(), "two-pass should be selected for k=100");
            let tp = sp.two_pass.unwrap();
            assert_eq!(tp.first_pass_k, 300);
        }
        _ => panic!("expected SearchPlan"),
    }
}

#[test]
fn two_pass_not_selected_for_small_k() {
    let stmt = Statement::Search(SearchStmt {
        collection: "venues".into(),
        fields: FieldList::All,
        dense_vector: Some(vec![1.0, 0.0, 0.0]),
        sparse_vector: None,
        filter: None,
        confidence: None,
        limit: Some(10),
        query_text: None,
        using_repr: None,
        hints: vec![],
    });
    let p = plan(&stmt, &empty_schemas()).unwrap();
    match p {
        Plan::Search(sp) => {
            assert!(sp.two_pass.is_none(), "two-pass should NOT be selected for k=10");
        }
        _ => panic!("expected SearchPlan"),
    }
}
```

- [ ] **Step 6: Implement two-pass in executor (advisory for Phase 13)**

In the executor's `Plan::Search` arm, add the two-pass execution path. For Phase 13, the two-pass path is structural but uses the same HNSW index (full-precision) for both passes. The actual binary/Int8 first-pass using `quantise.rs` is a future refinement -- Phase 13 lays the infrastructure and cost model:

```rust
// In the HNSW search path, after computing query_f32:
let raw = if let Some(ref tp) = p.two_pass {
    // Two-pass: over-fetch in first pass, then rescore
    let first_pass = hnsw.search(&query_f32, tp.first_pass_k);
    // Phase 13: both passes use the same HNSW index.
    // Future: first pass uses Int8/Binary index, second pass rescores.
    let mut rescored = first_pass;
    rescored.truncate(p.k);
    rescored
} else {
    hnsw.search(&query_f32, fetch_k)
};
```

- [ ] **Step 7: Add two-pass info to EXPLAIN output**

In `explain_plan()`, add to the `Plan::Search` arm:

```rust
if let Some(ref tp) = p.two_pass {
    props.push(("two_pass", format!("first_pass_k={}, binary={}", tp.first_pass_k, tp.use_binary_first_pass)));
}
```

- [ ] **Step 8: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace`

Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/trondb-core/src/planner.rs crates/trondb-core/src/executor.rs
git commit -m "phase 13: two-pass query strategy infrastructure + cost model"
```

---

## Chunk 7: Optimisation Rules Integration into Executor

Wire `apply_rules()` into the executor so that optimisation rules run at execution time and their warnings are propagated to the query result.

### Task 7: Executor Runs Optimisation Rules

**Files:**
- Modify: `crates/trondb-core/src/executor.rs`

- [ ] **Step 1: Write failing tests**

Add to the test module:

```rust
#[tokio::test]
async fn explain_shows_optimisation_rules() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "venues", 3).await;
    insert_entity(&exec, "venues", "v1", "The Fleece", vec![1.0, 0.0, 0.0]).await;

    let search_plan = Plan::Search(SearchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        dense_vector: Some(vec![1.0, 0.0, 0.0]),
        sparse_vector: None,
        filter: None,
        pre_filter: None,
        k: 10,
        confidence_threshold: 0.0,
        strategy: SearchStrategy::Hnsw,
        query_text: None,
        using_repr: None,
        hints: vec![],
        two_pass: None,
    });

    let result = exec
        .execute(&Plan::Explain(Box::new(search_plan)))
        .await
        .unwrap();

    // Should have rules_applied row
    let has_rules = result.rows.iter().any(|r| {
        r.values.get("property").map(|v| v.to_string()) == Some("rules_applied".to_string())
    });
    assert!(has_rules, "EXPLAIN should show applied optimisation rules");
}

#[tokio::test]
async fn warnings_propagated_to_result() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "venues", 3).await;

    // Insert enough entities so deep TRAVERSE would trigger a warning
    insert_entity(&exec, "venues", "v1", "The Fleece", vec![1.0, 0.0, 0.0]).await;

    let plan = Plan::Traverse(TraversePlan {
        edge_type: "similar_to".into(),
        from_id: "v1".into(),
        depth: 8,
        limit: None,
    });

    // Execute directly -- the edge type won't exist so it will error,
    // but we can test the optimisation rules by going through EXPLAIN
    let explain_result = exec
        .execute(&Plan::Explain(Box::new(plan)))
        .await
        .unwrap();

    // Check warnings are in the EXPLAIN output
    let has_warnings = explain_result.rows.iter().any(|r| {
        r.values.get("property").map(|v| v.to_string()) == Some("warnings".to_string())
    });
    assert!(has_warnings, "EXPLAIN should show warnings for deep TRAVERSE");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core executor::tests::explain_shows_optimisation_rules executor::tests::warnings_propagated_to_result`

Expected: FAIL.

- [ ] **Step 3: Integrate optimisation rules into EXPLAIN**

In the `Plan::Explain(inner)` arm, after computing the cost estimate, add:

```rust
use crate::optimise::{apply_rules, OptimiserConfig};

// Run optimisation rules
let optimised = apply_rules(
    (**inner).clone(),
    &OptimiserConfig::default(),
    self.cost_provider.as_ref(),
    size,
);

// Add rules_applied row
if !optimised.rules_applied.is_empty() {
    rows.push(Row {
        values: HashMap::from([
            ("property".into(), Value::String("rules_applied".into())),
            ("value".into(), Value::String(optimised.rules_applied.join(", "))),
        ]),
        score: None,
    });
}

// Add warnings rows
if !optimised.warnings.is_empty() {
    let warnings_str = optimised.warnings.iter()
        .map(|w| format!("{}", w))
        .collect::<Vec<_>>()
        .join("; ");
    rows.push(Row {
        values: HashMap::from([
            ("property".into(), Value::String("warnings".into())),
            ("value".into(), Value::String(warnings_str)),
        ]),
        score: None,
    });
}
```

Also add optimisation rules to the regular execution path (non-EXPLAIN):

At the top of `execute()`, after cost estimation:

```rust
// Run optimisation rules (warnings are collected but don't modify the plan in Phase 13)
let opt_warnings = match plan {
    Plan::Explain(_) => vec![], // EXPLAIN handles its own
    _ => {
        let collection_name = plan_collection_name(plan);
        let size = collection_name
            .map(|c| self.collection_size(c))
            .unwrap_or(0);
        let optimised = crate::optimise::apply_rules(
            plan.clone(),
            &crate::optimise::OptimiserConfig::default(),
            self.cost_provider.as_ref(),
            size,
        );
        optimised.warnings
    }
};
```

Then, in every `Ok(QueryResult { ... })`, replace:

```rust
warnings: vec![],
```

with:

```rust
warnings: opt_warnings.clone(),
```

Note: for the `Plan::Explain` arm specifically, keep `warnings: vec![]` since the warnings are shown in the rows instead.

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test -p trondb-core executor::tests`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/trondb-core/src/executor.rs
git commit -m "phase 13: optimisation rules integrated into executor + EXPLAIN"
```

---

## Chunk 8: Proto/gRPC Extensions for Cost & Warnings

Extend the protobuf schema to carry cost and warning data across the wire.

### Task 8: Proto + Conversion for Cost and Warnings

**Files:**
- Modify: `crates/trondb-proto/proto/trondb.proto`
- Modify: `crates/trondb-proto/src/convert_plan.rs`

- [ ] **Step 1: Add proto messages**

In `crates/trondb-proto/proto/trondb.proto`, add the following messages:

```protobuf
message AcuLineItemProto {
    string operation = 1;
    uint64 count = 2;
    double unit_cost = 3;
    double total = 4;
}

message AcuEstimateProto {
    repeated AcuLineItemProto items = 1;
    double total_acu = 2;
}

message PlanWarningProto {
    string severity = 1;     // "INFO", "WARN", "CRIT"
    string message = 2;
    string suggestion = 3;   // empty if none
    double acu_impact = 4;   // 0 if none
}
```

Add optional fields to `QueryStatsProto` (or whatever the existing stats message is called):

```protobuf
message QueryStatsProto {
    // ... existing fields ...
    optional AcuEstimateProto cost = 10;
    repeated PlanWarningProto warnings = 11;
}
```

Also add `optional TwoPassConfigProto` to the SearchPlan proto message:

```protobuf
message TwoPassConfigProto {
    uint64 first_pass_k = 1;
    bool use_binary_first_pass = 2;
}
```

Add to SearchPlanProto:

```protobuf
message SearchPlanProto {
    // ... existing fields ...
    optional TwoPassConfigProto two_pass = 15;
}
```

- [ ] **Step 2: Implement bidirectional conversions**

In `crates/trondb-proto/src/convert_plan.rs`, add conversion functions:

```rust
// AcuEstimate <-> AcuEstimateProto
impl From<&AcuEstimate> for AcuEstimateProto {
    fn from(est: &AcuEstimate) -> Self {
        Self {
            items: est.items.iter().map(|item| AcuLineItemProto {
                operation: item.operation.clone(),
                count: item.count as u64,
                unit_cost: item.unit_cost,
                total: item.total,
            }).collect(),
            total_acu: est.total_acu,
        }
    }
}

impl From<&AcuEstimateProto> for AcuEstimate {
    fn from(proto: &AcuEstimateProto) -> Self {
        Self {
            items: proto.items.iter().map(|item| AcuLineItem {
                operation: item.operation.clone(),
                count: item.count as usize,
                unit_cost: item.unit_cost,
                total: item.total,
            }).collect(),
            total_acu: proto.total_acu,
        }
    }
}

// PlanWarning <-> PlanWarningProto
impl From<&PlanWarning> for PlanWarningProto {
    fn from(w: &PlanWarning) -> Self {
        Self {
            severity: w.severity.to_string(),
            message: w.message.clone(),
            suggestion: w.suggestion.clone().unwrap_or_default(),
            acu_impact: w.acu_impact.unwrap_or(0.0),
        }
    }
}

impl From<&PlanWarningProto> for PlanWarning {
    fn from(proto: &PlanWarningProto) -> Self {
        let severity = match proto.severity.as_str() {
            "WARN" => WarningSeverity::Warning,
            "CRIT" => WarningSeverity::Critical,
            _ => WarningSeverity::Info,
        };
        PlanWarning {
            severity,
            message: proto.message.clone(),
            suggestion: if proto.suggestion.is_empty() { None } else { Some(proto.suggestion.clone()) },
            acu_impact: if proto.acu_impact == 0.0 { None } else { Some(proto.acu_impact) },
        }
    }
}

// TwoPassConfig <-> TwoPassConfigProto
impl From<&TwoPassConfig> for TwoPassConfigProto {
    fn from(tp: &TwoPassConfig) -> Self {
        Self {
            first_pass_k: tp.first_pass_k as u64,
            use_binary_first_pass: tp.use_binary_first_pass,
        }
    }
}

impl From<&TwoPassConfigProto> for TwoPassConfig {
    fn from(proto: &TwoPassConfigProto) -> Self {
        Self {
            first_pass_k: proto.first_pass_k as usize,
            use_binary_first_pass: proto.use_binary_first_pass,
        }
    }
}
```

Wire these into the existing `QueryStatsProto` and `SearchPlanProto` conversions.

- [ ] **Step 3: Run tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace`

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/trondb-proto/proto/trondb.proto crates/trondb-proto/src/convert_plan.rs
git commit -m "phase 13: proto/gRPC extensions for cost model and warnings"
```

---

## Chunk 9: CLAUDE.md and Integration Test

Update project documentation and write an end-to-end integration test.

### Task 9: Update CLAUDE.md + Integration Test

**Files:**
- Modify: `CLAUDE.md`
- Modify: `crates/trondb-core/src/executor.rs` (integration test)

- [ ] **Step 1: Update CLAUDE.md**

Add the following section after the "Query Language Completions (Phase 12)" section:

```markdown
- Planner & Cost Model (Phase 13)
  - ACU (Abstract Cost Unit): base calibration 1 hot-tier FETCH = 1.0 ACU
  - CostProvider trait: per-operation ACU costs (fetch_hot, fetch_warm, search_hnsw, etc.)
  - ConstantCostProvider: hardcoded defaults, ships as sole implementation
  - AcuEstimate: itemised cost breakdown attached to every query result
  - PlanWarning: severity (Info/Warning/Critical), message, suggestion, ACU impact
  - MAX_ACU query hint enforced: estimated cost > budget → AcuBudgetExceeded error
  - EXPLAIN shows: estimated_acu, cost_breakdown, rules_applied, warnings
  - Five optimisation rules (all enabled by default, individually disableable):
    - ScalarPreFilter: ACU integration, warns on unindexed SEARCH WHERE
    - ConfidencePushdown: advisory for high-confidence early termination
    - TraverseHopReorder: warns on deep TRAVERSE, future edge reordering
    - OnDemandPromotion: respects NO_PROMOTE hint, warns on suppressed promotion
    - BatchedFetchAfterSearch: warns on large result sets (k > 50)
  - Two-pass query strategy: selected for k >= 50, first pass over-fetches 3x, structural infrastructure for future Int8/Binary first pass
  - OptimiserConfig: per-rule enable/disable
```

Also update the first line:

```markdown
Inference-first storage engine. Phase 13: Planner & Cost Model.
```

- [ ] **Step 2: Write end-to-end integration test**

Add to the test module in `crates/trondb-core/src/executor.rs`:

```rust
#[tokio::test]
async fn end_to_end_cost_model_flow() {
    let (exec, _dir) = setup_executor().await;
    create_collection(&exec, "venues", 3).await;

    // Insert entities
    for i in 0..20 {
        insert_entity(
            &exec,
            "venues",
            &format!("v{i}"),
            &format!("Venue {i}"),
            vec![1.0, 0.0, 0.0],
        )
        .await;
    }

    // 1. FETCH with full scan -- should have cost
    let fetch_plan = Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: None,
        order_by: vec![],
        limit: None,
        strategy: FetchStrategy::FullScan,
        hints: vec![],
    });
    let result = exec.execute(&fetch_plan).await.unwrap();
    assert!(result.stats.cost.is_some());
    let cost = result.stats.cost.as_ref().unwrap();
    assert!(cost.total_acu > 0.0);

    // 2. SEARCH -- should have cost
    let search_plan = Plan::Search(SearchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        dense_vector: Some(vec![1.0, 0.0, 0.0]),
        sparse_vector: None,
        filter: None,
        pre_filter: None,
        k: 5,
        confidence_threshold: 0.0,
        strategy: SearchStrategy::Hnsw,
        query_text: None,
        using_repr: None,
        hints: vec![],
        two_pass: None,
    });
    let result = exec.execute(&search_plan).await.unwrap();
    assert!(result.stats.cost.is_some());

    // 3. EXPLAIN shows cost breakdown
    let explain_result = exec
        .execute(&Plan::Explain(Box::new(search_plan.clone())))
        .await
        .unwrap();
    let has_acu = explain_result.rows.iter().any(|r| {
        r.values.get("property").map(|v| v.to_string()) == Some("estimated_acu".into())
    });
    assert!(has_acu);

    // 4. MAX_ACU enforcement
    let budget_plan = Plan::Fetch(FetchPlan {
        collection: "venues".into(),
        fields: FieldList::All,
        filter: None,
        order_by: vec![],
        limit: None,
        strategy: FetchStrategy::FullScan,
        hints: vec![QueryHint::MaxAcu(1.0)], // 1 ACU budget vs 20 * 0.5 = 10 ACU
    });
    let result = exec.execute(&budget_plan).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), EngineError::AcuBudgetExceeded { .. }));
}
```

- [ ] **Step 3: Run all tests**

Run: `CARGO_TARGET_DIR=/tmp/trondb-target cargo test --workspace`

Expected: PASS (with known flaky test `similarity_score_range` tolerated).

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md crates/trondb-core/src/executor.rs
git commit -m "phase 13: CLAUDE.md updated, end-to-end integration test"
```

---

## Summary of ACU Cost Table

| Operation | Default ACU | Rationale |
|-----------|-------------|-----------|
| `fetch_hot` | 1.0 | **Baseline**. Hot-tier Fjall read. |
| `fetch_warm` | 25.0 | Int8 dequantise + read from warm partition. |
| `fetch_archive` | 100.0 | Binary data, low quality, heavy I/O. |
| `search_hnsw` | 50.0 | In-memory graph traversal, O(log n). |
| `search_sparse` | 20.0 | RAM inverted index, typically fewer candidates. |
| `search_hybrid` | 75.0 | Dense + Sparse + RRF merge. |
| `search_natural_language` | 80.0 | Vectoriser encode + HNSW search. |
| `full_scan_per_entity` | 0.5 | Per-entity cost of reading from Fjall. |
| `field_index_lookup` | 2.0 | Fjall prefix/range scan, very fast. |
| `traverse_hop` | 3.0 | DashMap lookup per hop, O(1). |
| `infer` | 60.0 | Vector search + scoring + filtering. |
| `pre_filter` | 5.0 | Field index lookup + over-fetch overhead. |
| `two_pass_rescore` | 15.0 | Re-score survivors with full-precision. |
| `promotion` | 30.0 | Dequantise + HNSW re-insert. |
| `write_base` | 5.0 | WAL append + Fjall write + index updates. |

---

## Implementation Order

```
Chunk 1: CostProvider + ConstantCostProvider + AcuEstimate     (self-contained, no deps)
Chunk 2: PlanWarning + QueryResult extension                   (depends on Chunk 1)
Chunk 3: Planner cost estimation + MAX_ACU                     (depends on Chunk 1)
Chunk 4: Executor cost threading + EXPLAIN                     (depends on Chunks 1-3)
Chunk 5: Optimisation rules framework                          (depends on Chunks 1-2)
Chunk 6: Two-pass query strategy                               (depends on Chunks 1-3)
Chunk 7: Optimisation rules in executor                        (depends on Chunks 4-5)
Chunk 8: Proto/gRPC extensions                                 (depends on Chunks 1-2, 6)
Chunk 9: CLAUDE.md + integration test                          (depends on all)
```

---

**Plan saved to:** `docs/superpowers/plans/2026-03-13-phase-13-planner-cost-model.md`

**Next step:** Annotate this plan with `
