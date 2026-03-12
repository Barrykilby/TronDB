//! Plan <-> Proto conversions.
//!
//! Implements `From<&Plan> for pb::PlanRequest` and `TryFrom<pb::PlanRequest> for Plan`
//! with all sub-conversions for nested TQL types.

use crate::pb;
use trondb_core::planner::*;
use trondb_tql::ast::{
    DecayConfigDecl, DecayFnDecl, FieldDecl, FieldList, FieldType, IndexDecl, Literal, Metric,
    RepresentationDecl, TierTarget, VectorLiteral, WhereClause,
};

// ---------------------------------------------------------------------------
// Leaf type helpers
// ---------------------------------------------------------------------------

fn literal_to_proto(lit: &Literal) -> pb::LiteralValue {
    use pb::literal_value::Value;
    pb::LiteralValue {
        value: Some(match lit {
            Literal::String(s) => Value::StringVal(s.clone()),
            Literal::Int(n) => Value::IntVal(*n),
            Literal::Float(f) => Value::FloatVal(*f),
            Literal::Bool(b) => Value::BoolVal(*b),
            Literal::Null => Value::NullVal(true),
        }),
    }
}

fn proto_to_literal(proto: &pb::LiteralValue) -> Result<Literal, String> {
    use pb::literal_value::Value;
    match &proto.value {
        Some(Value::StringVal(s)) => Ok(Literal::String(s.clone())),
        Some(Value::IntVal(n)) => Ok(Literal::Int(*n)),
        Some(Value::FloatVal(f)) => Ok(Literal::Float(*f)),
        Some(Value::BoolVal(b)) => Ok(Literal::Bool(*b)),
        Some(Value::NullVal(_)) => Ok(Literal::Null),
        None => Err("missing literal value".into()),
    }
}

fn where_clause_to_proto(clause: &WhereClause) -> pb::WhereClauseProto {
    use pb::where_clause_proto::Clause;

    let make_cmp = |field: &str, value: &Literal| pb::ComparisonClause {
        field: field.into(),
        value: Some(literal_to_proto(value)),
    };

    let make_bin = |left: &WhereClause, right: &WhereClause| pb::BinaryClause {
        left: Some(Box::new(where_clause_to_proto(left))),
        right: Some(Box::new(where_clause_to_proto(right))),
    };

    pb::WhereClauseProto {
        clause: Some(match clause {
            WhereClause::Eq(f, v) => Clause::Eq(make_cmp(f, v)),
            WhereClause::Gt(f, v) => Clause::Gt(make_cmp(f, v)),
            WhereClause::Lt(f, v) => Clause::Lt(make_cmp(f, v)),
            WhereClause::Gte(f, v) => Clause::Gte(make_cmp(f, v)),
            WhereClause::Lte(f, v) => Clause::Lte(make_cmp(f, v)),
            WhereClause::And(l, r) => Clause::And(Box::new(make_bin(l, r))),
            WhereClause::Or(l, r) => Clause::Or(Box::new(make_bin(l, r))),
        }),
    }
}

fn proto_to_where_clause(proto: &pb::WhereClauseProto) -> Result<WhereClause, String> {
    use pb::where_clause_proto::Clause;

    let extract_cmp = |cmp: &pb::ComparisonClause| -> Result<(String, Literal), String> {
        let val = proto_to_literal(cmp.value.as_ref().ok_or("missing comparison value")?)?;
        Ok((cmp.field.clone(), val))
    };

    let extract_bin =
        |bin: &pb::BinaryClause| -> Result<(Box<WhereClause>, Box<WhereClause>), String> {
            let left = proto_to_where_clause(
                bin.left.as_ref().ok_or("missing binary left")?.as_ref(),
            )?;
            let right = proto_to_where_clause(
                bin.right.as_ref().ok_or("missing binary right")?.as_ref(),
            )?;
            Ok((Box::new(left), Box::new(right)))
        };

    match proto.clause.as_ref().ok_or("missing where clause")? {
        Clause::Eq(c) => {
            let (f, v) = extract_cmp(c)?;
            Ok(WhereClause::Eq(f, v))
        }
        Clause::Gt(c) => {
            let (f, v) = extract_cmp(c)?;
            Ok(WhereClause::Gt(f, v))
        }
        Clause::Lt(c) => {
            let (f, v) = extract_cmp(c)?;
            Ok(WhereClause::Lt(f, v))
        }
        Clause::Gte(c) => {
            let (f, v) = extract_cmp(c)?;
            Ok(WhereClause::Gte(f, v))
        }
        Clause::Lte(c) => {
            let (f, v) = extract_cmp(c)?;
            Ok(WhereClause::Lte(f, v))
        }
        Clause::And(b) => {
            let (l, r) = extract_bin(b)?;
            Ok(WhereClause::And(l, r))
        }
        Clause::Or(b) => {
            let (l, r) = extract_bin(b)?;
            Ok(WhereClause::Or(l, r))
        }
    }
}

fn field_list_to_proto(fl: &FieldList) -> pb::FieldListProto {
    match fl {
        FieldList::All => pb::FieldListProto {
            all: true,
            names: vec![],
        },
        FieldList::Named(names) => pb::FieldListProto {
            all: false,
            names: names.clone(),
        },
    }
}

fn proto_to_field_list(proto: &pb::FieldListProto) -> FieldList {
    if proto.all {
        FieldList::All
    } else {
        FieldList::Named(proto.names.clone())
    }
}

fn vector_literal_to_proto(vl: &VectorLiteral) -> pb::VectorLiteralProto {
    use pb::vector_literal_proto::Vector;
    pb::VectorLiteralProto {
        vector: Some(match vl {
            VectorLiteral::Dense(v) => Vector::Dense(pb::DenseVector {
                values: v.clone(),
            }),
            VectorLiteral::Sparse(entries) => Vector::Sparse(pb::SparseVector {
                entries: entries
                    .iter()
                    .map(|(idx, val)| pb::SparseEntry {
                        index: *idx,
                        value: *val,
                    })
                    .collect(),
            }),
        }),
    }
}

fn proto_to_vector_literal(proto: &pb::VectorLiteralProto) -> Result<VectorLiteral, String> {
    use pb::vector_literal_proto::Vector;
    match proto.vector.as_ref().ok_or("missing vector literal")? {
        Vector::Dense(d) => Ok(VectorLiteral::Dense(d.values.clone())),
        Vector::Sparse(s) => Ok(VectorLiteral::Sparse(
            s.entries.iter().map(|e| (e.index, e.value)).collect(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Strategy + config helpers
// ---------------------------------------------------------------------------

fn fetch_strategy_to_proto(fs: &FetchStrategy) -> (i32, String) {
    match fs {
        FetchStrategy::FullScan => (pb::FetchStrategyProto::FetchStrategyFullScan as i32, String::new()),
        FetchStrategy::FieldIndexLookup(name) => {
            (pb::FetchStrategyProto::FetchStrategyFieldIndexLookup as i32, name.clone())
        }
        FetchStrategy::FieldIndexRange(name) => {
            (pb::FetchStrategyProto::FetchStrategyFieldIndexRange as i32, name.clone())
        }
    }
}

fn proto_to_fetch_strategy(proto_enum: i32, index_name: &str) -> FetchStrategy {
    match proto_enum {
        x if x == pb::FetchStrategyProto::FetchStrategyFieldIndexLookup as i32 => {
            FetchStrategy::FieldIndexLookup(index_name.into())
        }
        x if x == pb::FetchStrategyProto::FetchStrategyFieldIndexRange as i32 => {
            FetchStrategy::FieldIndexRange(index_name.into())
        }
        _ => FetchStrategy::FullScan,
    }
}

fn search_strategy_to_proto(ss: &SearchStrategy) -> i32 {
    match ss {
        SearchStrategy::Hnsw => pb::SearchStrategyProto::SearchStrategyHnsw as i32,
        SearchStrategy::Sparse => pb::SearchStrategyProto::SearchStrategySparse as i32,
        SearchStrategy::Hybrid => pb::SearchStrategyProto::SearchStrategyHybrid as i32,
    }
}

fn proto_to_search_strategy(val: i32) -> SearchStrategy {
    match val {
        x if x == pb::SearchStrategyProto::SearchStrategySparse as i32 => SearchStrategy::Sparse,
        x if x == pb::SearchStrategyProto::SearchStrategyHybrid as i32 => SearchStrategy::Hybrid,
        _ => SearchStrategy::Hnsw,
    }
}

fn repr_decl_to_proto(rd: &RepresentationDecl) -> pb::RepresentationDecl {
    pb::RepresentationDecl {
        name: rd.name.clone(),
        model: rd.model.clone(),
        dimensions: rd.dimensions.map(|d| d as u64),
        metric: match rd.metric {
            Metric::Cosine => pb::MetricProto::MetricCosine as i32,
            Metric::InnerProduct => pb::MetricProto::MetricInnerProduct as i32,
        },
        sparse: rd.sparse,
    }
}

fn proto_to_repr_decl(proto: &pb::RepresentationDecl) -> RepresentationDecl {
    RepresentationDecl {
        name: proto.name.clone(),
        model: proto.model.clone(),
        dimensions: proto.dimensions.map(|d| d as usize),
        metric: if proto.metric == pb::MetricProto::MetricInnerProduct as i32 {
            Metric::InnerProduct
        } else {
            Metric::Cosine
        },
        sparse: proto.sparse,
    }
}

fn field_decl_to_proto(fd: &FieldDecl) -> pb::FieldDecl {
    let (ft, entity_ref) = match &fd.field_type {
        FieldType::Text => (pb::FieldTypeProto::FieldTypeText as i32, None),
        FieldType::DateTime => (pb::FieldTypeProto::FieldTypeDatetime as i32, None),
        FieldType::Bool => (pb::FieldTypeProto::FieldTypeBool as i32, None),
        FieldType::Int => (pb::FieldTypeProto::FieldTypeInt as i32, None),
        FieldType::Float => (pb::FieldTypeProto::FieldTypeFloat as i32, None),
        FieldType::EntityRef(coll) => {
            (pb::FieldTypeProto::FieldTypeEntityRef as i32, Some(coll.clone()))
        }
    };
    pb::FieldDecl {
        name: fd.name.clone(),
        field_type: ft,
        entity_ref_collection: entity_ref,
    }
}

fn proto_to_field_decl(proto: &pb::FieldDecl) -> Result<FieldDecl, String> {
    let field_type = match proto.field_type {
        x if x == pb::FieldTypeProto::FieldTypeText as i32 => FieldType::Text,
        x if x == pb::FieldTypeProto::FieldTypeDatetime as i32 => FieldType::DateTime,
        x if x == pb::FieldTypeProto::FieldTypeBool as i32 => FieldType::Bool,
        x if x == pb::FieldTypeProto::FieldTypeInt as i32 => FieldType::Int,
        x if x == pb::FieldTypeProto::FieldTypeFloat as i32 => FieldType::Float,
        x if x == pb::FieldTypeProto::FieldTypeEntityRef as i32 => {
            FieldType::EntityRef(
                proto
                    .entity_ref_collection
                    .clone()
                    .ok_or("missing entity_ref_collection for EntityRef field type")?,
            )
        }
        other => return Err(format!("unknown field type: {other}")),
    };
    Ok(FieldDecl {
        name: proto.name.clone(),
        field_type,
    })
}

fn index_decl_to_proto(idx: &IndexDecl) -> pb::IndexDecl {
    pb::IndexDecl {
        name: idx.name.clone(),
        fields: idx.fields.clone(),
        partial_condition: idx.partial_condition.as_ref().map(where_clause_to_proto),
    }
}

fn proto_to_index_decl(proto: &pb::IndexDecl) -> Result<IndexDecl, String> {
    Ok(IndexDecl {
        name: proto.name.clone(),
        fields: proto.fields.clone(),
        partial_condition: proto
            .partial_condition
            .as_ref()
            .map(proto_to_where_clause)
            .transpose()?,
    })
}

fn decay_config_to_proto(dc: &DecayConfigDecl) -> pb::DecayConfigProto {
    pb::DecayConfigProto {
        decay_fn: dc.decay_fn.as_ref().map(|f| match f {
            DecayFnDecl::Exponential => pb::DecayFnProto::DecayFnExponential as i32,
            DecayFnDecl::Linear => pb::DecayFnProto::DecayFnLinear as i32,
            DecayFnDecl::Step => pb::DecayFnProto::DecayFnStep as i32,
        }),
        decay_rate: dc.decay_rate,
        floor: dc.floor,
        promote_threshold: dc.promote_threshold,
        prune_threshold: dc.prune_threshold,
    }
}

fn proto_to_decay_config(proto: &pb::DecayConfigProto) -> DecayConfigDecl {
    DecayConfigDecl {
        decay_fn: proto.decay_fn.map(|v| match v {
            x if x == pb::DecayFnProto::DecayFnLinear as i32 => DecayFnDecl::Linear,
            x if x == pb::DecayFnProto::DecayFnStep as i32 => DecayFnDecl::Step,
            _ => DecayFnDecl::Exponential,
        }),
        decay_rate: proto.decay_rate,
        floor: proto.floor,
        promote_threshold: proto.promote_threshold,
        prune_threshold: proto.prune_threshold,
    }
}

fn tier_target_to_proto(tt: &TierTarget) -> i32 {
    match tt {
        TierTarget::Warm => pb::TierTargetProto::TierTargetWarm as i32,
        TierTarget::Archive => pb::TierTargetProto::TierTargetArchive as i32,
    }
}

fn proto_to_tier_target(val: i32) -> TierTarget {
    if val == pb::TierTargetProto::TierTargetArchive as i32 {
        TierTarget::Archive
    } else {
        TierTarget::Warm
    }
}

fn pre_filter_to_proto(pf: &PreFilter) -> pb::PreFilterProto {
    pb::PreFilterProto {
        index_name: pf.index_name.clone(),
        clause: Some(where_clause_to_proto(&pf.clause)),
    }
}

fn proto_to_pre_filter(proto: &pb::PreFilterProto) -> Result<PreFilter, String> {
    Ok(PreFilter {
        index_name: proto.index_name.clone(),
        clause: proto_to_where_clause(
            proto.clause.as_ref().ok_or("missing pre_filter clause")?,
        )?,
    })
}

// ---------------------------------------------------------------------------
// Plan -> Proto
// ---------------------------------------------------------------------------

impl From<&Plan> for pb::PlanRequest {
    fn from(plan: &Plan) -> Self {
        use pb::plan_request::Plan as PP;
        pb::PlanRequest {
            plan: Some(match plan {
                Plan::CreateCollection(cp) => PP::CreateCollection(pb::CreateCollectionPlan {
                    name: cp.name.clone(),
                    representations: cp.representations.iter().map(repr_decl_to_proto).collect(),
                    fields: cp.fields.iter().map(field_decl_to_proto).collect(),
                    indexes: cp.indexes.iter().map(index_decl_to_proto).collect(),
                }),

                Plan::Insert(ip) => {
                    let collocate_with = ip.collocate_with.clone().unwrap_or_default();
                    PP::Insert(pb::InsertPlan {
                        collection: ip.collection.clone(),
                        fields: ip.fields.clone(),
                        values: ip.values.iter().map(literal_to_proto).collect(),
                        vectors: ip
                            .vectors
                            .iter()
                            .map(|(name, vl)| pb::NamedVector {
                                name: name.clone(),
                                vector: Some(vector_literal_to_proto(vl)),
                            })
                            .collect(),
                        collocate_with,
                        affinity_group: ip.affinity_group.clone(),
                    })
                }

                Plan::Fetch(fp) => {
                    let (strategy, strategy_index_name) = fetch_strategy_to_proto(&fp.strategy);
                    PP::Fetch(pb::FetchPlan {
                        collection: fp.collection.clone(),
                        fields: Some(field_list_to_proto(&fp.fields)),
                        filter: fp.filter.as_ref().map(where_clause_to_proto),
                        limit: fp.limit.map(|l| l as u64),
                        strategy,
                        strategy_index_name,
                    })
                }

                Plan::Search(sp) => PP::Search(pb::SearchPlan {
                    collection: sp.collection.clone(),
                    fields: Some(field_list_to_proto(&sp.fields)),
                    dense_vector: sp.dense_vector.clone().unwrap_or_default(),
                    sparse_vector: sp
                        .sparse_vector
                        .as_ref()
                        .map(|sv| {
                            sv.iter()
                                .map(|(idx, val)| pb::SparseEntry {
                                    index: *idx,
                                    value: *val,
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    filter: sp.filter.as_ref().map(where_clause_to_proto),
                    pre_filter: sp.pre_filter.as_ref().map(pre_filter_to_proto),
                    k: sp.k as u64,
                    confidence_threshold: sp.confidence_threshold,
                    strategy: search_strategy_to_proto(&sp.strategy),
                    has_dense: sp.dense_vector.is_some(),
                    has_sparse: sp.sparse_vector.is_some(),
                }),

                Plan::Explain(inner) => PP::Explain(Box::new(pb::ExplainPlan {
                    inner: Some(Box::new(pb::PlanRequest::from(inner.as_ref()))),
                })),

                Plan::CreateEdgeType(ce) => PP::CreateEdgeType(pb::CreateEdgeTypePlan {
                    name: ce.name.clone(),
                    from_collection: ce.from_collection.clone(),
                    to_collection: ce.to_collection.clone(),
                    decay_config: ce.decay_config.as_ref().map(decay_config_to_proto),
                }),

                Plan::InsertEdge(ie) => PP::InsertEdge(pb::InsertEdgePlan {
                    edge_type: ie.edge_type.clone(),
                    from_id: ie.from_id.clone(),
                    to_id: ie.to_id.clone(),
                    metadata: ie
                        .metadata
                        .iter()
                        .map(|(f, v)| pb::FieldAssignment {
                            field: f.clone(),
                            value: Some(literal_to_proto(v)),
                        })
                        .collect(),
                }),

                Plan::DeleteEntity(de) => PP::DeleteEntity(pb::DeleteEntityPlan {
                    entity_id: de.entity_id.clone(),
                    collection: de.collection.clone(),
                }),

                Plan::DeleteEdge(de) => PP::DeleteEdge(pb::DeleteEdgePlan {
                    edge_type: de.edge_type.clone(),
                    from_id: de.from_id.clone(),
                    to_id: de.to_id.clone(),
                }),

                Plan::Traverse(tp) => PP::Traverse(pb::TraversePlan {
                    edge_type: tp.edge_type.clone(),
                    from_id: tp.from_id.clone(),
                    depth: tp.depth as u64,
                    limit: tp.limit.map(|l| l as u64),
                }),

                Plan::CreateAffinityGroup(cag) => {
                    PP::CreateAffinityGroup(pb::CreateAffinityGroupPlan {
                        name: cag.name.clone(),
                    })
                }

                Plan::AlterEntityDropAffinity(ae) => {
                    PP::AlterEntityDropAffinity(pb::AlterEntityDropAffinityPlan {
                        entity_id: ae.entity_id.clone(),
                    })
                }

                Plan::Demote(dp) => PP::Demote(pb::DemotePlan {
                    entity_id: dp.entity_id.clone(),
                    collection: dp.collection.clone(),
                    target_tier: tier_target_to_proto(&dp.target_tier),
                }),

                Plan::Promote(pp) => PP::Promote(pb::PromotePlan {
                    entity_id: pp.entity_id.clone(),
                    collection: pp.collection.clone(),
                }),

                Plan::ExplainTiers(et) => PP::ExplainTiers(pb::ExplainTiersPlan {
                    collection: et.collection.clone(),
                }),

                Plan::UpdateEntity(up) => PP::UpdateEntity(pb::UpdateEntityPlan {
                    entity_id: up.entity_id.clone(),
                    collection: up.collection.clone(),
                    assignments: up
                        .assignments
                        .iter()
                        .map(|(f, v)| pb::FieldAssignment {
                            field: f.clone(),
                            value: Some(literal_to_proto(v)),
                        })
                        .collect(),
                }),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Proto -> Plan
// ---------------------------------------------------------------------------

impl TryFrom<pb::PlanRequest> for Plan {
    type Error = String;

    fn try_from(proto: pb::PlanRequest) -> Result<Self, String> {
        use pb::plan_request::Plan as PP;
        match proto.plan.ok_or("missing plan")? {
            PP::CreateCollection(cp) => Ok(Plan::CreateCollection(CreateCollectionPlan {
                name: cp.name,
                representations: cp.representations.iter().map(proto_to_repr_decl).collect(),
                fields: cp
                    .fields
                    .iter()
                    .map(proto_to_field_decl)
                    .collect::<Result<Vec<_>, _>>()?,
                indexes: cp
                    .indexes
                    .iter()
                    .map(proto_to_index_decl)
                    .collect::<Result<Vec<_>, _>>()?,
            })),

            PP::Insert(ip) => {
                let collocate_with = if ip.collocate_with.is_empty() {
                    None
                } else {
                    Some(ip.collocate_with)
                };
                let vectors = ip
                    .vectors
                    .iter()
                    .map(|nv| {
                        let vl = proto_to_vector_literal(
                            nv.vector.as_ref().ok_or("missing named vector")?,
                        )?;
                        Ok((nv.name.clone(), vl))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                let values = ip
                    .values
                    .iter()
                    .map(proto_to_literal)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Plan::Insert(InsertPlan {
                    collection: ip.collection,
                    fields: ip.fields,
                    values,
                    vectors,
                    collocate_with,
                    affinity_group: ip.affinity_group,
                }))
            }

            PP::Fetch(fp) => Ok(Plan::Fetch(FetchPlan {
                collection: fp.collection,
                fields: proto_to_field_list(&fp.fields.ok_or("missing fields")?),
                filter: fp.filter.as_ref().map(proto_to_where_clause).transpose()?,
                limit: fp.limit.map(|l| l as usize),
                strategy: proto_to_fetch_strategy(fp.strategy, &fp.strategy_index_name),
            })),

            PP::Search(sp) => {
                let dense_vector = if sp.has_dense {
                    Some(sp.dense_vector)
                } else {
                    None
                };
                let sparse_vector = if sp.has_sparse {
                    Some(
                        sp.sparse_vector
                            .iter()
                            .map(|e| (e.index, e.value))
                            .collect(),
                    )
                } else {
                    None
                };
                Ok(Plan::Search(SearchPlan {
                    collection: sp.collection,
                    fields: proto_to_field_list(&sp.fields.ok_or("missing search fields")?),
                    dense_vector,
                    sparse_vector,
                    filter: sp.filter.as_ref().map(proto_to_where_clause).transpose()?,
                    pre_filter: sp
                        .pre_filter
                        .as_ref()
                        .map(proto_to_pre_filter)
                        .transpose()?,
                    k: sp.k as usize,
                    confidence_threshold: sp.confidence_threshold,
                    strategy: proto_to_search_strategy(sp.strategy),
                }))
            }

            PP::Explain(ep) => {
                let inner = Plan::try_from(*ep.inner.ok_or("missing inner plan")?)?;
                Ok(Plan::Explain(Box::new(inner)))
            }

            PP::CreateEdgeType(ce) => Ok(Plan::CreateEdgeType(CreateEdgeTypePlan {
                name: ce.name,
                from_collection: ce.from_collection,
                to_collection: ce.to_collection,
                decay_config: ce.decay_config.as_ref().map(proto_to_decay_config),
            })),

            PP::InsertEdge(ie) => {
                let metadata = ie
                    .metadata
                    .iter()
                    .map(|fa| {
                        let lit =
                            proto_to_literal(fa.value.as_ref().ok_or("missing field value")?)?;
                        Ok((fa.field.clone(), lit))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                Ok(Plan::InsertEdge(InsertEdgePlan {
                    edge_type: ie.edge_type,
                    from_id: ie.from_id,
                    to_id: ie.to_id,
                    metadata,
                }))
            }

            PP::DeleteEntity(de) => Ok(Plan::DeleteEntity(DeleteEntityPlan {
                entity_id: de.entity_id,
                collection: de.collection,
            })),

            PP::DeleteEdge(de) => Ok(Plan::DeleteEdge(DeleteEdgePlan {
                edge_type: de.edge_type,
                from_id: de.from_id,
                to_id: de.to_id,
            })),

            PP::Traverse(tp) => Ok(Plan::Traverse(TraversePlan {
                edge_type: tp.edge_type,
                from_id: tp.from_id,
                depth: tp.depth as usize,
                limit: tp.limit.map(|l| l as usize),
            })),

            PP::CreateAffinityGroup(cag) => {
                Ok(Plan::CreateAffinityGroup(CreateAffinityGroupPlan {
                    name: cag.name,
                }))
            }

            PP::AlterEntityDropAffinity(ae) => {
                Ok(Plan::AlterEntityDropAffinity(AlterEntityDropAffinityPlan {
                    entity_id: ae.entity_id,
                }))
            }

            PP::Demote(dp) => Ok(Plan::Demote(DemotePlan {
                entity_id: dp.entity_id,
                collection: dp.collection,
                target_tier: proto_to_tier_target(dp.target_tier),
            })),

            PP::Promote(pp) => Ok(Plan::Promote(PromotePlan {
                entity_id: pp.entity_id,
                collection: pp.collection,
            })),

            PP::ExplainTiers(et) => Ok(Plan::ExplainTiers(ExplainTiersPlan {
                collection: et.collection,
            })),

            PP::UpdateEntity(up) => {
                let assignments = up
                    .assignments
                    .iter()
                    .map(|fa| {
                        let lit =
                            proto_to_literal(fa.value.as_ref().ok_or("missing assignment value")?)?;
                        Ok((fa.field.clone(), lit))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                Ok(Plan::UpdateEntity(UpdateEntityPlan {
                    entity_id: up.entity_id,
                    collection: up.collection,
                    assignments,
                }))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use trondb_tql::{FieldList, Literal, VectorLiteral, WhereClause};

    fn round_trip(plan: Plan) -> Plan {
        let proto: pb::PlanRequest = (&plan).into();
        Plan::try_from(proto).unwrap()
    }

    #[test]
    fn round_trip_fetch_all() {
        let plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            limit: Some(10),
            strategy: FetchStrategy::FullScan,
        });
        let restored = round_trip(plan);
        match restored {
            Plan::Fetch(fp) => {
                assert_eq!(fp.collection, "venues");
                assert_eq!(fp.fields, FieldList::All);
                assert_eq!(fp.limit, Some(10));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn round_trip_search_hybrid() {
        let plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::Named(vec!["name".into()]),
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: Some(vec![(1, 0.5), (42, 0.8)]),
            filter: Some(WhereClause::Eq(
                "city".into(),
                Literal::String("London".into()),
            )),
            pre_filter: Some(PreFilter {
                index_name: "idx_city".into(),
                clause: WhereClause::Eq("city".into(), Literal::String("London".into())),
            }),
            k: 5,
            confidence_threshold: 0.8,
            strategy: SearchStrategy::Hybrid,
        });
        let restored = round_trip(plan);
        match restored {
            Plan::Search(sp) => {
                assert_eq!(sp.collection, "venues");
                assert_eq!(sp.k, 5);
                assert_eq!(sp.strategy, SearchStrategy::Hybrid);
                assert!(sp.dense_vector.is_some());
                assert!(sp.sparse_vector.is_some());
                assert!(sp.filter.is_some());
                assert!(sp.pre_filter.is_some());
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn round_trip_insert() {
        let plan = Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["name".into(), "active".into()],
            values: vec![Literal::String("Gym".into()), Literal::Bool(true)],
            vectors: vec![("default".into(), VectorLiteral::Dense(vec![1.0, 2.0]))],
            collocate_with: Some(vec!["v2".into()]),
            affinity_group: Some("group-1".into()),
        });
        let restored = round_trip(plan);
        match restored {
            Plan::Insert(ip) => {
                assert_eq!(ip.collection, "venues");
                assert_eq!(ip.fields.len(), 2);
                assert_eq!(ip.values.len(), 2);
                assert_eq!(ip.vectors.len(), 1);
                assert!(ip.collocate_with.is_some());
                assert_eq!(ip.affinity_group, Some("group-1".into()));
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn round_trip_explain_wraps_inner() {
        let inner = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: None,
            limit: None,
            strategy: FetchStrategy::FullScan,
        });
        let plan = Plan::Explain(Box::new(inner));
        let restored = round_trip(plan);
        match restored {
            Plan::Explain(inner) => match *inner {
                Plan::Fetch(fp) => assert_eq!(fp.collection, "venues"),
                _ => panic!("expected Fetch inside Explain"),
            },
            _ => panic!("expected Explain"),
        }
    }

    #[test]
    fn round_trip_update() {
        let plan = Plan::UpdateEntity(UpdateEntityPlan {
            entity_id: "v1".into(),
            collection: "venues".into(),
            assignments: vec![
                ("name".into(), Literal::String("New".into())),
                ("score".into(), Literal::Int(42)),
            ],
        });
        let restored = round_trip(plan);
        match restored {
            Plan::UpdateEntity(up) => {
                assert_eq!(up.entity_id, "v1");
                assert_eq!(up.assignments.len(), 2);
            }
            _ => panic!("expected UpdateEntity"),
        }
    }

    #[test]
    fn round_trip_where_clause_and() {
        let plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::And(
                Box::new(WhereClause::Gte("score".into(), Literal::Int(50))),
                Box::new(WhereClause::Lt("score".into(), Literal::Int(100))),
            )),
            limit: None,
            strategy: FetchStrategy::FieldIndexRange("idx_score".into()),
        });
        let restored = round_trip(plan);
        match restored {
            Plan::Fetch(fp) => {
                assert!(matches!(fp.filter, Some(WhereClause::And(_, _))));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn round_trip_all_simple_plans() {
        // DeleteEntity
        let plan = Plan::DeleteEntity(DeleteEntityPlan {
            entity_id: "v1".into(),
            collection: "venues".into(),
        });
        assert!(matches!(round_trip(plan), Plan::DeleteEntity(_)));

        // CreateAffinityGroup
        let plan = Plan::CreateAffinityGroup(CreateAffinityGroupPlan {
            name: "g1".into(),
        });
        assert!(matches!(round_trip(plan), Plan::CreateAffinityGroup(_)));

        // Demote
        let plan = Plan::Demote(DemotePlan {
            entity_id: "v1".into(),
            collection: "venues".into(),
            target_tier: TierTarget::Warm,
        });
        assert!(matches!(round_trip(plan), Plan::Demote(_)));

        // Promote
        let plan = Plan::Promote(PromotePlan {
            entity_id: "v1".into(),
            collection: "venues".into(),
        });
        assert!(matches!(round_trip(plan), Plan::Promote(_)));

        // ExplainTiers
        let plan = Plan::ExplainTiers(ExplainTiersPlan {
            collection: "venues".into(),
        });
        assert!(matches!(round_trip(plan), Plan::ExplainTiers(_)));
    }

    #[test]
    fn round_trip_create_collection() {
        let plan = Plan::CreateCollection(CreateCollectionPlan {
            name: "venues".into(),
            representations: vec![RepresentationDecl {
                name: "default".into(),
                model: Some("text-embedding-3-small".into()),
                dimensions: Some(384),
                metric: Metric::Cosine,
                sparse: false,
            }],
            fields: vec![
                FieldDecl {
                    name: "city".into(),
                    field_type: FieldType::Text,
                },
                FieldDecl {
                    name: "score".into(),
                    field_type: FieldType::Int,
                },
                FieldDecl {
                    name: "ref".into(),
                    field_type: FieldType::EntityRef("other".into()),
                },
            ],
            indexes: vec![IndexDecl {
                name: "idx_city".into(),
                fields: vec!["city".into()],
                partial_condition: Some(WhereClause::Eq(
                    "active".into(),
                    Literal::Bool(true),
                )),
            }],
        });
        let restored = round_trip(plan);
        match restored {
            Plan::CreateCollection(cp) => {
                assert_eq!(cp.name, "venues");
                assert_eq!(cp.representations.len(), 1);
                assert_eq!(cp.representations[0].model, Some("text-embedding-3-small".into()));
                assert_eq!(cp.fields.len(), 3);
                assert!(matches!(cp.fields[2].field_type, FieldType::EntityRef(ref c) if c == "other"));
                assert_eq!(cp.indexes.len(), 1);
                assert!(cp.indexes[0].partial_condition.is_some());
            }
            _ => panic!("expected CreateCollection"),
        }
    }

    #[test]
    fn round_trip_create_edge_type_with_decay() {
        let plan = Plan::CreateEdgeType(CreateEdgeTypePlan {
            name: "likes".into(),
            from_collection: "users".into(),
            to_collection: "venues".into(),
            decay_config: Some(DecayConfigDecl {
                decay_fn: Some(DecayFnDecl::Exponential),
                decay_rate: Some(0.01),
                floor: Some(0.1),
                promote_threshold: Some(0.9),
                prune_threshold: Some(0.05),
            }),
        });
        let restored = round_trip(plan);
        match restored {
            Plan::CreateEdgeType(ce) => {
                assert_eq!(ce.name, "likes");
                let dc = ce.decay_config.unwrap();
                assert_eq!(dc.decay_fn, Some(DecayFnDecl::Exponential));
                assert_eq!(dc.decay_rate, Some(0.01));
                assert_eq!(dc.floor, Some(0.1));
            }
            _ => panic!("expected CreateEdgeType"),
        }
    }

    #[test]
    fn round_trip_insert_edge() {
        let plan = Plan::InsertEdge(InsertEdgePlan {
            edge_type: "likes".into(),
            from_id: "u1".into(),
            to_id: "v1".into(),
            metadata: vec![("weight".into(), Literal::Float(0.9))],
        });
        let restored = round_trip(plan);
        match restored {
            Plan::InsertEdge(ie) => {
                assert_eq!(ie.edge_type, "likes");
                assert_eq!(ie.metadata.len(), 1);
                assert_eq!(ie.metadata[0].0, "weight");
            }
            _ => panic!("expected InsertEdge"),
        }
    }

    #[test]
    fn round_trip_traverse() {
        let plan = Plan::Traverse(TraversePlan {
            edge_type: "likes".into(),
            from_id: "u1".into(),
            depth: 3,
            limit: Some(50),
        });
        let restored = round_trip(plan);
        match restored {
            Plan::Traverse(tp) => {
                assert_eq!(tp.edge_type, "likes");
                assert_eq!(tp.depth, 3);
                assert_eq!(tp.limit, Some(50));
            }
            _ => panic!("expected Traverse"),
        }
    }

    #[test]
    fn round_trip_delete_edge() {
        let plan = Plan::DeleteEdge(DeleteEdgePlan {
            edge_type: "likes".into(),
            from_id: "u1".into(),
            to_id: "v1".into(),
        });
        let restored = round_trip(plan);
        match restored {
            Plan::DeleteEdge(de) => {
                assert_eq!(de.edge_type, "likes");
                assert_eq!(de.from_id, "u1");
                assert_eq!(de.to_id, "v1");
            }
            _ => panic!("expected DeleteEdge"),
        }
    }

    #[test]
    fn round_trip_alter_entity_drop_affinity() {
        let plan = Plan::AlterEntityDropAffinity(AlterEntityDropAffinityPlan {
            entity_id: "v1".into(),
        });
        let restored = round_trip(plan);
        match restored {
            Plan::AlterEntityDropAffinity(ae) => {
                assert_eq!(ae.entity_id, "v1");
            }
            _ => panic!("expected AlterEntityDropAffinity"),
        }
    }

    #[test]
    fn round_trip_search_dense_only() {
        let plan = Plan::Search(SearchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            dense_vector: Some(vec![1.0, 0.0, 0.0]),
            sparse_vector: None,
            filter: None,
            pre_filter: None,
            k: 10,
            confidence_threshold: 0.0,
            strategy: SearchStrategy::Hnsw,
        });
        let restored = round_trip(plan);
        match restored {
            Plan::Search(sp) => {
                assert!(sp.dense_vector.is_some());
                assert!(sp.sparse_vector.is_none());
                assert_eq!(sp.strategy, SearchStrategy::Hnsw);
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn round_trip_insert_no_collocate() {
        let plan = Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec!["name".into()],
            values: vec![Literal::String("X".into())],
            vectors: vec![],
            collocate_with: None,
            affinity_group: None,
        });
        let restored = round_trip(plan);
        match restored {
            Plan::Insert(ip) => {
                assert!(ip.collocate_with.is_none());
                assert!(ip.affinity_group.is_none());
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn round_trip_where_clause_or() {
        let plan = Plan::Fetch(FetchPlan {
            collection: "venues".into(),
            fields: FieldList::All,
            filter: Some(WhereClause::Or(
                Box::new(WhereClause::Eq("city".into(), Literal::String("London".into()))),
                Box::new(WhereClause::Eq("city".into(), Literal::String("Paris".into()))),
            )),
            limit: None,
            strategy: FetchStrategy::FullScan,
        });
        let restored = round_trip(plan);
        match restored {
            Plan::Fetch(fp) => {
                assert!(matches!(fp.filter, Some(WhereClause::Or(_, _))));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn round_trip_literal_null() {
        let plan = Plan::UpdateEntity(UpdateEntityPlan {
            entity_id: "v1".into(),
            collection: "venues".into(),
            assignments: vec![("name".into(), Literal::Null)],
        });
        let restored = round_trip(plan);
        match restored {
            Plan::UpdateEntity(up) => {
                assert_eq!(up.assignments[0].1, Literal::Null);
            }
            _ => panic!("expected UpdateEntity"),
        }
    }

    #[test]
    fn round_trip_sparse_vector_in_insert() {
        let plan = Plan::Insert(InsertPlan {
            collection: "venues".into(),
            fields: vec![],
            values: vec![],
            vectors: vec![(
                "sparse_repr".into(),
                VectorLiteral::Sparse(vec![(0, 0.1), (5, 0.9)]),
            )],
            collocate_with: None,
            affinity_group: None,
        });
        let restored = round_trip(plan);
        match restored {
            Plan::Insert(ip) => {
                assert_eq!(ip.vectors.len(), 1);
                match &ip.vectors[0].1 {
                    VectorLiteral::Sparse(entries) => {
                        assert_eq!(entries.len(), 2);
                        assert_eq!(entries[0], (0, 0.1));
                        assert_eq!(entries[1], (5, 0.9));
                    }
                    _ => panic!("expected Sparse vector"),
                }
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn round_trip_demote_archive() {
        let plan = Plan::Demote(DemotePlan {
            entity_id: "v1".into(),
            collection: "venues".into(),
            target_tier: TierTarget::Archive,
        });
        let restored = round_trip(plan);
        match restored {
            Plan::Demote(dp) => {
                assert_eq!(dp.target_tier, TierTarget::Archive);
            }
            _ => panic!("expected Demote"),
        }
    }
}
