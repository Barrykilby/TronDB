// ---------------------------------------------------------------------------
// Optimisation Rules
// ---------------------------------------------------------------------------
//
// Five rules, all enabled by default, all individually disableable.
// Each rule takes a Plan + context and returns a (possibly modified) Plan
// plus any PlanWarnings generated.

use crate::cost::CostProvider;
use crate::planner::Plan;
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
    _cost_provider: &dyn CostProvider,
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
    use crate::planner::{FetchPlan, FetchStrategy, PreFilter, SearchPlan, SearchStrategy, TraversePlan};
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
            two_pass: None,
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
            two_pass: None,
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
            two_pass: None,
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
            temporal: None,
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
            two_pass: None,
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
            two_pass: None,
        });

        let result = apply_rules(plan, &config, &cost, 10_000);
        assert!(result.rules_applied.is_empty());
        assert!(result.warnings.is_empty());
    }
}
