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
