//! TronDB engine benchmark suite.
//!
//! Workloads:
//! - INSERT sustained rate
//! - FETCH throughput (point lookup + full scan)

use criterion::{criterion_group, criterion_main, Criterion};
use tokio::runtime::Runtime;

use trondb_core::planner::*;
use trondb_tql::{FieldList, Literal, VectorLiteral, WhereClause};

fn setup_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn setup_engine(dir: &std::path::Path) -> trondb_core::Engine {
    let config = trondb_core::EngineConfig {
        data_dir: dir.to_path_buf(),
        wal: trondb_wal::WalConfig {
            wal_dir: dir.join("wal"),
            ..Default::default()
        },
        snapshot_interval_secs: 3600,
        hnsw_snapshot_interval_secs: 3600,
    };
    let (engine, _) = trondb_core::Engine::open(config).await.unwrap();
    engine
}

fn bench_insert(c: &mut Criterion) {
    let rt = setup_runtime();
    let dir = tempfile::TempDir::new().unwrap();
    let engine = rt.block_on(setup_engine(dir.path()));

    // Create collection
    rt.block_on(engine.execute_tql(
        "CREATE COLLECTION bench_insert (REPRESENTATION default DIMENSIONS 8 METRIC COSINE);",
    ))
    .unwrap();

    let mut group = c.benchmark_group("insert");
    group.sample_size(50);

    let mut counter = 0u64;
    group.bench_function("single_insert", |b| {
        b.iter(|| {
            counter += 1;
            let id = format!("e{counter}");
            let vec: Vec<f64> = (0..8).map(|i| (i as f64 + counter as f64) * 0.1).collect();
            rt.block_on(async {
                engine
                    .execute(&Plan::Insert(InsertPlan {
                        collection: "bench_insert".into(),
                        fields: vec!["id".into()],
                        values: vec![Literal::String(id)],
                        vectors: vec![("default".into(), VectorLiteral::Dense(vec))],
                        collocate_with: None,
                        affinity_group: None,
                    }))
                    .await
                    .unwrap();
            });
        });
    });
    group.finish();
}

fn bench_fetch(c: &mut Criterion) {
    let rt = setup_runtime();
    let dir = tempfile::TempDir::new().unwrap();
    let engine = rt.block_on(setup_engine(dir.path()));

    // Create and populate collection
    rt.block_on(engine.execute_tql(
        "CREATE COLLECTION bench_fetch (REPRESENTATION default DIMENSIONS 4 METRIC COSINE);",
    ))
    .unwrap();
    for i in 0..100 {
        let id = format!("e{i}");
        rt.block_on(
            engine.execute(&Plan::Insert(InsertPlan {
                collection: "bench_fetch".into(),
                fields: vec!["id".into(), "name".into()],
                values: vec![
                    Literal::String(id),
                    Literal::String(format!("entity_{i}")),
                ],
                vectors: vec![(
                    "default".into(),
                    VectorLiteral::Dense(vec![i as f64; 4]),
                )],
                collocate_with: None,
                affinity_group: None,
            })),
        )
        .unwrap();
    }

    let mut group = c.benchmark_group("fetch");
    group.sample_size(100);

    group.bench_function("full_scan_100", |b| {
        b.iter(|| {
            rt.block_on(async {
                engine
                    .execute(&Plan::Fetch(FetchPlan {
                        collection: "bench_fetch".into(),
                        fields: FieldList::All,
                        filter: None,
                        order_by: vec![],
                        limit: None,
                        strategy: FetchStrategy::FullScan,
                        hints: vec![],
                    }))
                    .await
                    .unwrap();
            });
        });
    });

    group.bench_function("point_lookup", |b| {
        b.iter(|| {
            rt.block_on(async {
                engine
                    .execute(&Plan::Fetch(FetchPlan {
                        collection: "bench_fetch".into(),
                        fields: FieldList::All,
                        filter: Some(WhereClause::Eq(
                            "id".into(),
                            Literal::String("e50".into()),
                        )),
                        order_by: vec![],
                        limit: Some(1),
                        strategy: FetchStrategy::FullScan,
                        hints: vec![],
                    }))
                    .await
                    .unwrap();
            });
        });
    });
    group.finish();
}

criterion_group!(benches, bench_insert, bench_fetch);
criterion_main!(benches);
