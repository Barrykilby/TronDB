//! QueryResult <-> Proto conversions.
//!
//! Implements `From<&QueryResult> for pb::QueryResponse` and
//! `TryFrom<pb::QueryResponse> for QueryResult` with helpers for
//! `Value` ↔ `ValueMessage`.

use std::collections::HashMap;
use std::time::Duration;

use crate::pb;
use trondb_core::cost::{AcuEstimate, AcuLineItem};
use trondb_core::result::{QueryMode, QueryResult, QueryStats, Row};
use trondb_core::types::Value;
use trondb_core::warning::{PlanWarning, WarningSeverity};

// ---------------------------------------------------------------------------
// Value helpers
// ---------------------------------------------------------------------------

fn value_to_proto(val: &Value) -> pb::ValueMessage {
    use pb::value_message::Value as PbValue;
    pb::ValueMessage {
        value: Some(match val {
            Value::String(s) => PbValue::StringVal(s.clone()),
            Value::Int(n) => PbValue::IntVal(*n),
            Value::Float(f) => PbValue::FloatVal(*f),
            Value::Bool(b) => PbValue::BoolVal(*b),
            Value::Null => PbValue::NullVal(true),
        }),
    }
}

fn proto_to_value(proto: &pb::ValueMessage) -> Result<Value, String> {
    use pb::value_message::Value as PbValue;
    match &proto.value {
        Some(PbValue::StringVal(s)) => Ok(Value::String(s.clone())),
        Some(PbValue::IntVal(n)) => Ok(Value::Int(*n)),
        Some(PbValue::FloatVal(f)) => Ok(Value::Float(*f)),
        Some(PbValue::BoolVal(b)) => Ok(Value::Bool(*b)),
        Some(PbValue::NullVal(_)) => Ok(Value::Null),
        None => Err("missing value in ValueMessage".into()),
    }
}

// ---------------------------------------------------------------------------
// QueryMode helpers
// ---------------------------------------------------------------------------

fn mode_to_proto(mode: &QueryMode) -> pb::QueryModeProto {
    match mode {
        QueryMode::Deterministic => pb::QueryModeProto::QueryModeDeterministic,
        QueryMode::Probabilistic => pb::QueryModeProto::QueryModeProbabilistic,
    }
}

fn proto_to_mode(proto: i32) -> Result<QueryMode, String> {
    match pb::QueryModeProto::try_from(proto) {
        Ok(pb::QueryModeProto::QueryModeDeterministic) => Ok(QueryMode::Deterministic),
        Ok(pb::QueryModeProto::QueryModeProbabilistic) => Ok(QueryMode::Probabilistic),
        Err(_) => Err(format!("unknown QueryModeProto value: {proto}")),
    }
}

// ---------------------------------------------------------------------------
// AcuEstimate helpers
// ---------------------------------------------------------------------------

fn acu_estimate_to_proto(est: &AcuEstimate) -> pb::AcuEstimateProto {
    pb::AcuEstimateProto {
        items: est.items.iter().map(|item| pb::AcuLineItemProto {
            operation: item.operation.clone(),
            count: item.count as u64,
            unit_cost: item.unit_cost,
            total: item.total,
        }).collect(),
        total_acu: est.total_acu,
    }
}

fn proto_to_acu_estimate(proto: &pb::AcuEstimateProto) -> AcuEstimate {
    AcuEstimate {
        items: proto.items.iter().map(|item| AcuLineItem {
            operation: item.operation.clone(),
            count: item.count as usize,
            unit_cost: item.unit_cost,
            total: item.total,
        }).collect(),
        total_acu: proto.total_acu,
    }
}

// ---------------------------------------------------------------------------
// PlanWarning helpers
// ---------------------------------------------------------------------------

fn plan_warning_to_proto(w: &PlanWarning) -> pb::PlanWarningProto {
    pb::PlanWarningProto {
        severity: w.severity.to_string(),
        message: w.message.clone(),
        suggestion: w.suggestion.clone().unwrap_or_default(),
        acu_impact: w.acu_impact.unwrap_or(0.0),
    }
}

fn proto_to_plan_warning(proto: &pb::PlanWarningProto) -> PlanWarning {
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

// ---------------------------------------------------------------------------
// QueryResult -> QueryResponse
// ---------------------------------------------------------------------------

impl From<&QueryResult> for pb::QueryResponse {
    fn from(result: &QueryResult) -> Self {
        let rows = result
            .rows
            .iter()
            .map(|row| {
                let values = row
                    .values
                    .iter()
                    .map(|(k, v)| (k.clone(), value_to_proto(v)))
                    .collect::<HashMap<_, _>>();
                pb::RowMessage {
                    values,
                    score: row.score,
                }
            })
            .collect();

        let stats = pb::QueryStatsMessage {
            elapsed_nanos: result.stats.elapsed.as_nanos() as u64,
            entities_scanned: result.stats.entities_scanned as u64,
            mode: mode_to_proto(&result.stats.mode) as i32,
            tier: result.stats.tier.clone(),
            cost: result.stats.cost.as_ref().map(acu_estimate_to_proto),
            warnings: result.stats.warnings.iter().map(plan_warning_to_proto).collect(),
        };

        pb::QueryResponse {
            columns: result.columns.clone(),
            rows,
            stats: Some(stats),
        }
    }
}

// ---------------------------------------------------------------------------
// QueryResponse -> QueryResult
// ---------------------------------------------------------------------------

impl TryFrom<pb::QueryResponse> for QueryResult {
    type Error = String;

    fn try_from(proto: pb::QueryResponse) -> Result<Self, Self::Error> {
        let rows = proto
            .rows
            .into_iter()
            .map(|row| {
                let values = row
                    .values
                    .into_iter()
                    .map(|(k, v)| proto_to_value(&v).map(|val| (k, val)))
                    .collect::<Result<HashMap<_, _>, _>>()?;
                Ok(Row {
                    values,
                    score: row.score,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        let stats_proto = proto
            .stats
            .ok_or_else(|| "missing stats in QueryResponse".to_string())?;

        let stats = QueryStats {
            elapsed: Duration::from_nanos(stats_proto.elapsed_nanos),
            entities_scanned: stats_proto.entities_scanned as usize,
            mode: proto_to_mode(stats_proto.mode)?,
            tier: stats_proto.tier,
            cost: stats_proto.cost.as_ref().map(proto_to_acu_estimate),
            warnings: stats_proto.warnings.iter().map(proto_to_plan_warning).collect(),
        };

        Ok(QueryResult {
            columns: proto.columns,
            rows,
            stats,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;
    use trondb_core::result::{QueryMode, QueryResult, QueryStats, Row};
    use trondb_core::types::Value;

    #[test]
    fn query_result_round_trip() {
        let result = QueryResult {
            columns: vec!["name".into(), "score".into()],
            rows: vec![Row {
                values: HashMap::from([
                    ("name".into(), Value::String("Alice".into())),
                    ("score".into(), Value::Int(42)),
                ]),
                score: Some(0.95),
            }],
            stats: QueryStats {
                elapsed: Duration::from_millis(150),
                entities_scanned: 1000,
                mode: QueryMode::Probabilistic,
                tier: "hot".into(),
                cost: None,
                warnings: vec![],
            },
        };
        let proto: pb::QueryResponse = (&result).into();
        let restored = QueryResult::try_from(proto).unwrap();
        assert_eq!(restored.columns, result.columns);
        assert_eq!(restored.rows.len(), 1);
        assert_eq!(restored.rows[0].score, Some(0.95));
        assert_eq!(restored.stats.entities_scanned, 1000);
        assert_eq!(restored.stats.mode, QueryMode::Probabilistic);
    }

    #[test]
    fn value_round_trip_all_variants() {
        let values = vec![
            Value::String("hello".into()),
            Value::Int(-42),
            Value::Float(3.14),
            Value::Bool(true),
            Value::Null,
        ];
        for val in &values {
            let proto = value_to_proto(val);
            let restored = proto_to_value(&proto).unwrap();
            assert_eq!(&restored, val);
        }
    }

    #[test]
    fn query_mode_deterministic_round_trip() {
        let result = QueryResult {
            columns: vec!["id".into()],
            rows: vec![],
            stats: QueryStats {
                elapsed: Duration::from_millis(5),
                entities_scanned: 0,
                mode: QueryMode::Deterministic,
                tier: "hot".into(),
                cost: None,
                warnings: vec![],
            },
        };
        let proto: pb::QueryResponse = (&result).into();
        let restored = QueryResult::try_from(proto).unwrap();
        assert_eq!(restored.stats.mode, QueryMode::Deterministic);
    }

    #[test]
    fn elapsed_nanos_preserved() {
        let nanos = 123_456_789u64;
        let result = QueryResult {
            columns: vec![],
            rows: vec![],
            stats: QueryStats {
                elapsed: Duration::from_nanos(nanos),
                entities_scanned: 0,
                mode: QueryMode::Deterministic,
                tier: "hot".into(),
                cost: None,
                warnings: vec![],
            },
        };
        let proto: pb::QueryResponse = (&result).into();
        let restored = QueryResult::try_from(proto).unwrap();
        assert_eq!(restored.stats.elapsed.as_nanos() as u64, nanos);
    }

    #[test]
    fn acu_estimate_round_trip() {
        let mut est = AcuEstimate::zero();
        est.add("search_hnsw", 1, 50.0);
        est.add("fetch_hot", 10, 1.0);

        let proto = acu_estimate_to_proto(&est);
        let restored = proto_to_acu_estimate(&proto);

        assert_eq!(restored.items.len(), 2);
        assert!((restored.total_acu - 60.0).abs() < f64::EPSILON);
        assert_eq!(restored.items[0].operation, "search_hnsw");
        assert_eq!(restored.items[0].count, 1);
        assert!((restored.items[0].unit_cost - 50.0).abs() < f64::EPSILON);
        assert_eq!(restored.items[1].operation, "fetch_hot");
        assert_eq!(restored.items[1].count, 10);
    }

    #[test]
    fn plan_warning_round_trip() {
        let warning = PlanWarning::warning("full scan on 10k entities")
            .with_suggestion("add an index on 'city'")
            .with_acu_impact(5000.0);

        let proto = plan_warning_to_proto(&warning);
        let restored = proto_to_plan_warning(&proto);

        assert_eq!(restored.severity, WarningSeverity::Warning);
        assert_eq!(restored.message, "full scan on 10k entities");
        assert_eq!(restored.suggestion.as_deref(), Some("add an index on 'city'"));
        assert!((restored.acu_impact.unwrap() - 5000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn plan_warning_info_no_suggestion_round_trip() {
        let warning = PlanWarning::info("using over-fetch 4x for pre-filter");

        let proto = plan_warning_to_proto(&warning);
        let restored = proto_to_plan_warning(&proto);

        assert_eq!(restored.severity, WarningSeverity::Info);
        assert!(restored.suggestion.is_none());
        assert!(restored.acu_impact.is_none());
    }

    #[test]
    fn plan_warning_critical_round_trip() {
        let warning = PlanWarning::critical("archive-only results")
            .with_acu_impact(100.0);

        let proto = plan_warning_to_proto(&warning);
        let restored = proto_to_plan_warning(&proto);

        assert_eq!(restored.severity, WarningSeverity::Critical);
    }

    #[test]
    fn query_result_with_cost_and_warnings_round_trip() {
        let mut cost = AcuEstimate::zero();
        cost.add("search_hnsw", 1, 50.0);
        cost.add("fetch_hot", 5, 1.0);

        let warnings = vec![
            PlanWarning::warning("large result set (k=100)")
                .with_acu_impact(100.0),
            PlanWarning::info("using ScalarPreFilter"),
        ];

        let result = QueryResult {
            columns: vec!["name".into()],
            rows: vec![Row {
                values: HashMap::from([
                    ("name".into(), Value::String("Alice".into())),
                ]),
                score: Some(0.95),
            }],
            stats: QueryStats {
                elapsed: Duration::from_millis(150),
                entities_scanned: 1000,
                mode: QueryMode::Probabilistic,
                tier: "hot".into(),
                cost: Some(cost),
                warnings,
            },
        };

        let proto: pb::QueryResponse = (&result).into();
        let restored = QueryResult::try_from(proto).unwrap();

        // Verify cost round-trip
        let restored_cost = restored.stats.cost.as_ref().expect("cost should be present");
        assert!((restored_cost.total_acu - 55.0).abs() < f64::EPSILON);
        assert_eq!(restored_cost.items.len(), 2);

        // Verify warnings round-trip
        assert_eq!(restored.stats.warnings.len(), 2);
        assert_eq!(restored.stats.warnings[0].severity, WarningSeverity::Warning);
        assert!(restored.stats.warnings[0].message.contains("large result set"));
        assert_eq!(restored.stats.warnings[1].severity, WarningSeverity::Info);
    }
}
