//! Probe: is small-index HNSW recall loss caused by index construction
//! randomness, or by something shared across concurrently running tests?
//!
//! Several tests assert an exact result count from a small HNSW index and fail
//! intermittently, but only under parallel execution. If construction
//! randomness alone were responsible, they would fail in isolation too. This
//! builds the same tiny index many times in one process and counts the misses.
//!
//!   cargo test -p trondb-core --test hnsw_determinism_probe -- --nocapture

use trondb_core::index::HnswIndex;
use trondb_core::types::LogicalId;

const TRIALS: usize = 500;

fn probe(label: &str, build: impl Fn(usize) -> HnswIndex, k: usize, expect: usize) {
    let mut short = 0;
    let mut worst = expect;
    for t in 0..TRIALS {
        let idx = build(t);
        let got = idx.search(&[1.0, 0.0, 0.0], k).len();
        if got < expect {
            short += 1;
            worst = worst.min(got);
        }
    }
    println!(
        "  {label:<34} {short:>4}/{TRIALS} returned < {expect} (worst {worst})  = {:.1}%",
        short as f64 * 100.0 / TRIALS as f64
    );
}

#[test]
fn small_index_recall_is_stable_when_built_repeatedly() {
    println!();
    println!("HNSW small-index recall, {TRIALS} independent index builds, single thread");

    probe(
        "2 orthogonal points, k=2",
        |_| {
            let idx = HnswIndex::new(3);
            idx.insert(&LogicalId::from_string("e1"), &[1.0, 0.0, 0.0]);
            idx.insert(&LogicalId::from_string("e2"), &[0.0, 1.0, 0.0]);
            idx
        },
        2,
        2,
    );

    probe(
        "2 near-identical points, k=10",
        |_| {
            let idx = HnswIndex::new(3);
            idx.insert(&LogicalId::from_string("v1"), &[1.0, 0.0, 0.0]);
            idx.insert(&LogicalId::from_string("v2"), &[0.9, 0.1, 0.0]);
            idx
        },
        10,
        2,
    );

    probe(
        "8 points, k=8",
        |_| {
            let idx = HnswIndex::new(3);
            for i in 0..8 {
                let f = i as f32 / 8.0;
                idx.insert(
                    &LogicalId::from_string(&format!("e{i}")),
                    &[1.0 - f, f, 0.0],
                );
            }
            idx
        },
        8,
        8,
    );
    println!();
}

/// Does raising `ef` or asking for more neighbours recover the missing
/// results? If the graph is disconnected, neither will, and the fix has to be
/// a fallback rather than a parameter.
#[test]
fn does_more_effort_recover_missing_results() {
    println!();
    println!("Recovery attempts, 500 builds of the 8-point index, expecting 8");
    for (label, k, ef) in [
        ("k=8   ef=50   (current)", 8usize, 50usize),
        ("k=8   ef=200", 8, 200),
        ("k=8   ef=800", 8, 800),
        ("k=64  ef=800", 64, 800),
    ] {
        let mut short = 0;
        for _ in 0..500 {
            let idx = HnswIndex::new(3);
            for i in 0..8 {
                let f = i as f32 / 8.0;
                idx.insert(
                    &LogicalId::from_string(&format!("e{i}")),
                    &[1.0 - f, f, 0.0],
                );
            }
            if idx.search_with_ef(&[1.0, 0.0, 0.0], k, ef).len() < 8 {
                short += 1;
            }
        }
        println!(
            "  {label:<26} {short:>4}/500 short  = {:.1}%",
            short as f64 / 5.0
        );
    }
    println!();
}
