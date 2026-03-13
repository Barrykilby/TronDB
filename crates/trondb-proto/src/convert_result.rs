//! QueryResult <-> Proto conversions.
//!
//! Implements `From<&QueryResult> for pb::QueryResponse` and
//! `TryFrom<pb::QueryResponse> for QueryResult` with helpers for
//! `Value` ↔ `ValueMessage`.

use std::collections::HashMap;
use std::time::Duration;

use crate::pb;
use trondb_core::result::{QueryMode, QueryResult, QueryStats, Row};
use trondb_core::types::Value;

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
            cost: None,
            warnings: vec![],
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
}
