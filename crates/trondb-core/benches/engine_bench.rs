//! TronDB engine benchmark suite.
//!
//! Two tiers of workload:
//!
//! Substrate (what every vector or graph store does):
//! - INSERT sustained rate
//! - FETCH point lookup and full scan
//! - SEARCH dense HNSW, pre-filtered, and hybrid dense+sparse
//! - TRAVERSE multi-hop
//!
//! Differentiator (what TronDB claims that other engines do not do):
//! - INFER candidate generation over a populated vector index
//! - Probabilistic JOIN, edge-based with a confidence threshold
//!
//! The second tier is the point of this file. Substrate numbers exist to give
//! the differentiator numbers a floor to be read against: if INFER over 10k
//! candidates costs the same as a dense SEARCH over the same 10k, the
//! inference layer is close to free, and that is the claim worth measuring.
//!
//! Plans are built with `parse_and_plan` outside the timing loop, so what is
//! measured is engine execution, not TQL parsing or planning.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use tokio::runtime::Runtime;

use trondb_core::planner::*;
use trondb_core::Engine;
use trondb_tql::{Literal, VectorLiteral};

/// Vector width for the retrieval and inference workloads.
const DIMS: usize = 64;
/// Out-edges per node in the synthetic traversal graph.
const GRAPH_FANOUT: usize = 5;

/// Corpus sizes are env-overridable so CI can run the harness in `--test`
/// mode cheaply (`TRONDB_BENCH_SCALE=small`) while published numbers come
/// from the default scale on known hardware. A smoke run that takes ten
/// minutes stops being run, and a harness nothing runs stops compiling.
fn scaled(default: usize) -> usize {
    match std::env::var("TRONDB_BENCH_SCALE").as_deref() {
        Ok("small") => (default / 50).max(20),
        _ => default,
    }
}

/// Entities inserted for the retrieval and inference workloads.
fn corpus() -> usize {
    scaled(10_000)
}

/// Nodes in the synthetic traversal graph.
fn graph_nodes() -> usize {
    scaled(2_000)
}

fn setup_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn setup_engine(dir: &std::path::Path) -> Engine {
    let config = trondb_core::EngineConfig {
        data_dir: dir.to_path_buf(),
        wal: trondb_wal::WalConfig {
            wal_dir: dir.join("wal"),
            ..Default::default()
        },
        // Long intervals: snapshotting mid-benchmark would show up as noise in
        // the timings rather than as a property of the query being measured.
        snapshot_interval_secs: 3600,
        hnsw_snapshot_interval_secs: 3600,
    };
    let (engine, _) = Engine::open(config).await.unwrap();
    engine
}

/// Deterministic pseudo-random unit-ish vector, so every run indexes the same
/// corpus and recall is comparable between runs.
fn vector_for(seed: usize) -> Vec<f64> {
    (0..DIMS)
        .map(|d| {
            let x = ((seed * 2_654_435_761usize + d * 40_503usize) % 10_007) as f64;
            x / 10_007.0
        })
        .collect()
}

fn vector_literal(seed: usize) -> String {
    let parts: Vec<String> = vector_for(seed).iter().map(|v| format!("{v:.4}")).collect();
    parts.join(", ")
}

// ---------------------------------------------------------------------------
// Substrate: writes
// ---------------------------------------------------------------------------

fn bench_insert(c: &mut Criterion) {
    let rt = setup_runtime();
    let dir = tempfile::TempDir::new().unwrap();
    let engine = rt.block_on(setup_engine(dir.path()));

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
                        valid_from: None,
                        valid_to: None,
                    }))
                    .await
                    .unwrap();
            });
        });
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Substrate: reads
// ---------------------------------------------------------------------------

fn bench_fetch(c: &mut Criterion) {
    let rt = setup_runtime();
    let dir = tempfile::TempDir::new().unwrap();
    let engine = rt.block_on(setup_engine(dir.path()));

    // An index on `name`, so the point-lookup case can actually exercise the
    // field-index path. Hard-coding FetchStrategy::FullScan for both cases
    // makes "point lookup" measure a full scan with an early exit, and report
    // it as slower than the scan it is supposed to beat.
    rt.block_on(engine.execute_tql(
        "CREATE COLLECTION bench_fetch (\
            FIELD name TEXT,\
            REPRESENTATION default DIMENSIONS 4 METRIC COSINE,\
            INDEX idx_name ON (name)\
        );",
    ))
    .unwrap();
    for i in 0..100 {
        rt.block_on(engine.execute_tql(&format!(
            "INSERT INTO bench_fetch (id, name) VALUES ('e{i}', 'entity_{i}') \
             REPRESENTATION default VECTOR [{i}.0, {i}.0, {i}.0, {i}.0];"
        )))
        .unwrap();
    }

    // Planned by the planner, not hand-built, so the measured path is the one
    // a real query takes.
    let scan = engine.parse_and_plan("FETCH * FROM bench_fetch;").unwrap();
    let indexed = engine
        .parse_and_plan("FETCH * FROM bench_fetch WHERE name = 'entity_50' LIMIT 1;")
        .unwrap();

    let mut group = c.benchmark_group("fetch");
    group.sample_size(100);

    for (name, plan) in [("full_scan_100", &scan), ("indexed_lookup", &indexed)] {
        group.bench_function(name, |b| {
            b.iter(|| {
                rt.block_on(async { engine.execute(plan).await.unwrap() });
            });
        });
    }
    group.finish();
}

/// Build the corpus used by both SEARCH and INFER: two collections of
/// `corpus()`/2 entities each, dense plus sparse representations, one indexed
/// scalar field for the pre-filter path, and no edges between them so INFER
/// has a full candidate set to rank.
async fn build_corpus(engine: &Engine) {
    engine
        .execute_tql(
            "CREATE COLLECTION acts (\
                FIELD name TEXT,\
                FIELD genre TEXT,\
                REPRESENTATION default DIMENSIONS 64 METRIC COSINE,\
                REPRESENTATION keywords METRIC INNER_PRODUCT SPARSE true,\
                INDEX idx_genre ON (genre)\
            );",
        )
        .await
        .unwrap();
    engine
        .execute_tql(
            "CREATE COLLECTION venues (\
                FIELD name TEXT,\
                FIELD genre TEXT,\
                REPRESENTATION default DIMENSIONS 64 METRIC COSINE,\
                REPRESENTATION keywords METRIC INNER_PRODUCT SPARSE true,\
                INDEX idx_genre ON (genre)\
            );",
        )
        .await
        .unwrap();
    engine
        .execute_tql(
            "CREATE EDGE performs_at FROM acts TO venues INFER AUTO CONFIDENCE > 0.5 LIMIT 10;",
        )
        .await
        .unwrap();

    let genres = ["jazz", "metal", "folk", "electronic"];
    let half = corpus() / 2;
    for i in 0..half {
        let genre = genres[i % genres.len()];
        for coll in ["acts", "venues"] {
            let prefix = if coll == "acts" { "a" } else { "v" };
            engine
                .execute_tql(&format!(
                    "INSERT INTO {coll} (id, name, genre) \
                     VALUES ('{prefix}{i}', 'entity_{i}', '{genre}') \
                     REPRESENTATION default VECTOR [{}] \
                     REPRESENTATION keywords SPARSE [{}:0.9, {}:0.4];",
                    vector_literal(if coll == "acts" { i } else { i + 7 }),
                    i % 512,
                    (i * 3) % 512,
                ))
                .await
                .unwrap();
        }
    }
}

fn bench_search(c: &mut Criterion) {
    let rt = setup_runtime();
    let dir = tempfile::TempDir::new().unwrap();
    let engine = rt.block_on(setup_engine(dir.path()));
    rt.block_on(build_corpus(&engine));

    let query = vector_literal(42);
    let dense = engine
        .parse_and_plan(&format!("SEARCH venues NEAR VECTOR [{query}] LIMIT 10;"))
        .unwrap();
    let prefiltered = engine
        .parse_and_plan(&format!(
            "SEARCH venues WHERE genre = 'jazz' NEAR VECTOR [{query}] LIMIT 10;"
        ))
        .unwrap();
    let hybrid = engine
        .parse_and_plan(&format!(
            "SEARCH venues NEAR VECTOR [{query}] NEAR SPARSE [42:0.9, 126:0.4] LIMIT 10;"
        ))
        .unwrap();

    let mut group = c.benchmark_group("search");
    group.sample_size(100);

    for (name, plan) in [
        ("dense_hnsw_5k", &dense),
        ("dense_prefiltered_5k", &prefiltered),
        ("hybrid_rrf_5k", &hybrid),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                rt.block_on(async { engine.execute(plan).await.unwrap() });
            });
        });
    }
    group.finish();
}

fn bench_traverse(c: &mut Criterion) {
    let rt = setup_runtime();
    let dir = tempfile::TempDir::new().unwrap();
    let engine = rt.block_on(setup_engine(dir.path()));

    rt.block_on(async {
        engine
            .execute_tql(
                "CREATE COLLECTION nodes (\
                    FIELD name TEXT,\
                    REPRESENTATION default DIMENSIONS 4 METRIC COSINE\
                );",
            )
            .await
            .unwrap();
        engine
            .execute_tql("CREATE EDGE linked FROM nodes TO nodes;")
            .await
            .unwrap();

        let graph_nodes = graph_nodes();
        for i in 0..graph_nodes {
            engine
                .execute_tql(&format!(
                    "INSERT INTO nodes (id, name) VALUES ('n{i}', 'node_{i}') \
                     REPRESENTATION default VECTOR [0.1, 0.2, 0.3, 0.4];"
                ))
                .await
                .unwrap();
        }
        // Fan-out graph: every node points at GRAPH_FANOUT others, so hop
        // count grows geometrically and depth actually costs something.
        for i in 0..graph_nodes {
            for f in 1..=GRAPH_FANOUT {
                let target = (i * GRAPH_FANOUT + f * 7) % graph_nodes;
                engine
                    .execute_tql(&format!("INSERT EDGE linked FROM 'n{i}' TO 'n{target}';"))
                    .await
                    .unwrap();
            }
        }
    });

    let mut group = c.benchmark_group("traverse");
    group.sample_size(50);

    for depth in [1usize, 2, 3] {
        let plan = Plan::Traverse(TraversePlan {
            edge_type: "linked".into(),
            from_id: "n0".into(),
            depth,
            limit: None,
        });
        group.bench_function(format!("depth_{depth}"), |b| {
            b.iter(|| {
                rt.block_on(async { engine.execute(&plan).await.unwrap() });
            });
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Differentiator: inference
// ---------------------------------------------------------------------------

fn bench_infer(c: &mut Criterion) {
    let rt = setup_runtime();
    let dir = tempfile::TempDir::new().unwrap();
    let engine = rt.block_on(setup_engine(dir.path()));
    rt.block_on(build_corpus(&engine));

    let top10 = engine
        .parse_and_plan("INFER EDGES FROM 'a0' VIA performs_at RETURNING TOP 10;")
        .unwrap();
    let top100 = engine
        .parse_and_plan("INFER EDGES FROM 'a0' VIA performs_at RETURNING TOP 100;")
        .unwrap();
    let gated = engine
        .parse_and_plan("INFER EDGES FROM 'a0' VIA performs_at RETURNING TOP 10 CONFIDENCE > 0.90;")
        .unwrap();

    // Sanity: a benchmark that silently returns nothing measures an error path,
    // not the workload. Fail loudly here rather than publish an empty number.
    let rows = rt
        .block_on(async { engine.execute(&top10).await.unwrap() })
        .rows
        .len();
    assert!(
        rows > 0,
        "INFER returned no candidates; benchmark would be measuring an empty result path"
    );

    let mut group = c.benchmark_group("infer");
    group.sample_size(50);

    for (name, plan) in [
        ("top10_over_5k", &top10),
        ("top100_over_5k", &top100),
        ("top10_confidence_gate", &gated),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                rt.block_on(async { engine.execute(plan).await.unwrap() });
            });
        });
    }
    group.finish();
}

fn bench_confirm(c: &mut Criterion) {
    let rt = setup_runtime();
    let dir = tempfile::TempDir::new().unwrap();
    let engine = rt.block_on(setup_engine(dir.path()));
    rt.block_on(build_corpus(&engine));

    let mut group = c.benchmark_group("confirm");
    group.sample_size(50);

    // CONFIRM is a write: it promotes an inferred candidate to a durable edge
    // and WAL-logs it. Each iteration needs a fresh pair, so the edge insert
    // is not measuring an idempotent no-op.
    let mut counter = 0usize;
    group.bench_function("promote_inferred_edge", |b| {
        b.iter_batched(
            || {
                counter += 1;
                (
                    format!("a{}", counter % (corpus() / 2)),
                    format!("v{}", (counter * 13) % (corpus() / 2)),
                )
            },
            |(from, to)| {
                rt.block_on(async {
                    engine
                        .execute(&Plan::ConfirmEdge(ConfirmEdgePlan {
                            from_id: from,
                            to_id: to,
                            edge_type: "performs_at".into(),
                            confidence: 0.9,
                        }))
                        .await
                        .unwrap();
                });
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_insert,
    bench_fetch,
    bench_search,
    bench_traverse,
    bench_infer,
    bench_confirm
);
criterion_main!(benches);
