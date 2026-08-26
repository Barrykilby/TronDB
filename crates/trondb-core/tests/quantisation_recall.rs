//! Measured recall for each storage tier's vector encoding.
//!
//! The README publishes a recall figure per tier, and this is where it comes
//! from. Cosine fidelity on a single reconstructed vector is not the same
//! property and must not be used as a proxy: a vector can reconstruct at
//! cosine 0.95 and still lose half its top-10 neighbours to reordering.
//!
//! This test measures the published property directly: for each encoding,
//! rank the whole corpus and count how much of the exact top-10 survives.
//!
//! Slow in debug (the exact baseline is O(corpus x queries)); prefer --release.
//!
//! Run with output:
//!   cargo test -p trondb-core --test quantisation_recall -- --nocapture

use trondb_core::quantise::*;

const CORPUS: usize = 5_000;
const QUERIES: usize = 200;
const DIMS: usize = 384;
const K: usize = 10;

/// Number of clusters in the realistic corpus.
const CLUSTERS: usize = 50;

/// Deterministic LCG, so published numbers are reproducible rather than a
/// sample of one lucky seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1))
    }

    /// Uniform in [-1, 1).
    fn next_signed(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (((self.0 >> 33) as f64 / (1u64 << 30) as f64) - 1.0) as f32
    }
}

fn normalise(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    v
}

/// Independent uniform coordinates. This is the pathological case for any
/// quantiser and NOT representative of embedding data: in 384 dimensions
/// i.i.d. vectors are all near-orthogonal, so the exact top-10 is a near-tie
/// among thousands of candidates separated by cosine noise. Recall@10 against
/// a near-tied ranking measures how a coin landed, not encoder quality. It is
/// reported here only as a floor, to make that distinction explicit.
fn uniform_vector(seed: usize) -> Vec<f32> {
    let mut rng = Rng::new(seed as u64);
    normalise((0..DIMS).map(|_| rng.next_signed()).collect())
}

/// Clustered, anisotropic coordinates: a cluster centroid plus small noise.
/// Real sentence and image embeddings sit on a low-dimensional manifold like
/// this, with genuine separation between a query's near neighbours and the
/// rest of the corpus. This is the distribution the published recall figures
/// should be read against.
fn clustered_vector(seed: usize) -> Vec<f32> {
    let cluster = seed % CLUSTERS;
    let mut centroid_rng = Rng::new(0xC0FFEE + cluster as u64);
    let mut noise_rng = Rng::new(0x5EED + seed as u64);
    normalise(
        (0..DIMS)
            .map(|_| centroid_rng.next_signed() + 0.35 * noise_rng.next_signed())
            .collect(),
    )
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn hamming(a: &[u8], b: &[u8]) -> u32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x ^ y).count_ones())
        .sum()
}

/// Indices of the top-K by descending score.
fn top_k(scores: &mut [(usize, f32)]) -> Vec<usize> {
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scores.iter().take(K).map(|(i, _)| *i).collect()
}

fn recall(exact: &[usize], approx: &[usize]) -> f64 {
    let hits = approx.iter().filter(|i| exact.contains(i)).count();
    hits as f64 / exact.len() as f64
}

/// Mean recall@K for each tier encoding over one corpus/query distribution.
struct TierRecall {
    polar3_dequantised: f64,
    polar3_inner_product: f64,
    polar4: f64,
    int8: f64,
    binary: f64,
}

fn measure(gen: fn(usize) -> Vec<f32>, query_gen: fn(usize) -> Vec<f32>) -> TierRecall {
    let corpus: Vec<Vec<f32>> = (0..CORPUS).map(gen).collect();
    let queries: Vec<Vec<f32>> = (0..QUERIES).map(|q| query_gen(CORPUS + q * 977)).collect();

    let polar_cfg = PolarQuantConfig::new(3, DIMS, 0xA5A5_A5A5);
    let polar4_cfg = PolarQuantConfig::new(4, DIMS, 0xA5A5_A5A5);

    let int8: Vec<_> = corpus.iter().map(|v| quantise_int8(v)).collect();
    let polar: Vec<_> = corpus
        .iter()
        .map(|v| quantise_polar(v, &polar_cfg))
        .collect();
    let polar4: Vec<_> = corpus
        .iter()
        .map(|v| quantise_polar(v, &polar4_cfg))
        .collect();
    let binary: Vec<_> = corpus.iter().map(|v| quantise_binary(v)).collect();

    let int8_vecs: Vec<Vec<f32>> = int8.iter().map(dequantise_int8).collect();
    let polar_vecs: Vec<Vec<f32>> = polar
        .iter()
        .map(|q| dequantise_polar(q, &polar_cfg))
        .collect();
    let polar4_vecs: Vec<Vec<f32>> = polar4
        .iter()
        .map(|q| dequantise_polar(q, &polar4_cfg))
        .collect();

    let mut acc = TierRecall {
        polar3_dequantised: 0.0,
        polar3_inner_product: 0.0,
        polar4: 0.0,
        int8: 0.0,
        binary: 0.0,
    };

    for query in &queries {
        let mut exact_scores: Vec<(usize, f32)> = corpus
            .iter()
            .enumerate()
            .map(|(i, v)| (i, cosine(query, v)))
            .collect();
        let exact = top_k(&mut exact_scores);

        let mut s: Vec<(usize, f32)> = int8_vecs
            .iter()
            .enumerate()
            .map(|(i, v)| (i, cosine(query, v)))
            .collect();
        acc.int8 += recall(&exact, &top_k(&mut s));

        let mut s: Vec<(usize, f32)> = polar_vecs
            .iter()
            .enumerate()
            .map(|(i, v)| (i, cosine(query, v)))
            .collect();
        acc.polar3_dequantised += recall(&exact, &top_k(&mut s));

        let mut s: Vec<(usize, f32)> = polar4_vecs
            .iter()
            .enumerate()
            .map(|(i, v)| (i, cosine(query, v)))
            .collect();
        acc.polar4 += recall(&exact, &top_k(&mut s));

        // Second, independent path to the same answer: the engine's own
        // approximate inner product against the packed codes. If these two
        // disagree the fault is in the encoder or in this harness, and either
        // way the number should not be published.
        let mut s: Vec<(usize, f32)> = polar
            .iter()
            .enumerate()
            .map(|(i, q)| (i, approx_inner_product_raw(query, q, &polar_cfg)))
            .collect();
        acc.polar3_inner_product += recall(&exact, &top_k(&mut s));

        let qbin = quantise_binary(query);
        let mut s: Vec<(usize, f32)> = binary
            .iter()
            .enumerate()
            .map(|(i, b)| (i, -(hamming(&qbin.data, &b.data) as f32)))
            .collect();
        acc.binary += recall(&exact, &top_k(&mut s));
    }

    let n = QUERIES as f64;
    TierRecall {
        polar3_dequantised: acc.polar3_dequantised / n,
        polar3_inner_product: acc.polar3_inner_product / n,
        polar4: acc.polar4 / n,
        int8: acc.int8 / n,
        binary: acc.binary / n,
    }
}

#[test]
fn tier_encodings_recall_at_10() {
    let clustered = measure(clustered_vector, clustered_vector);
    let uniform = measure(uniform_vector, uniform_vector);

    let polar_cfg = PolarQuantConfig::new(3, DIMS, 0xA5A5_A5A5);
    let probe = clustered_vector(1);
    let f32_bytes = DIMS * 4;
    let int8_bytes = quantise_int8(&probe).to_bytes().len();
    let polar_bytes = quantise_polar(&probe, &polar_cfg).to_bytes().len();
    let polar4_bytes = quantise_polar(&probe, &PolarQuantConfig::new(4, DIMS, 0xA5A5_A5A5))
        .to_bytes()
        .len();
    let binary_bytes = quantise_binary(&probe).to_bytes().len();

    println!();
    println!("recall@{K}, {CORPUS} vectors, {DIMS} dims, {QUERIES} queries, exact cosine baseline");
    println!();
    println!("  tier      encoding      bytes   compression   clustered   i.i.d. uniform");
    println!("  hot       float32       {f32_bytes:>5}         1.0x      100.0%           100.0%");
    println!(
        "  warm      polar 3-bit   {polar_bytes:>5}   {:>9.1}x   {:>8.1}%   {:>14.1}%",
        f32_bytes as f64 / polar_bytes as f64,
        clustered.polar3_dequantised * 100.0,
        uniform.polar3_dequantised * 100.0
    );
    println!(
        "  warm      polar 4-bit   {polar4_bytes:>5}   {:>9.1}x   {:>8.1}%   {:>14.1}%",
        f32_bytes as f64 / polar4_bytes as f64,
        clustered.polar4 * 100.0,
        uniform.polar4 * 100.0
    );
    println!(
        "  cool      int8          {int8_bytes:>5}   {:>9.1}x   {:>8.1}%   {:>14.1}%",
        f32_bytes as f64 / int8_bytes as f64,
        clustered.int8 * 100.0,
        uniform.int8 * 100.0
    );
    println!(
        "  archive   binary        {binary_bytes:>5}   {:>9.1}x   {:>8.1}%   {:>14.1}%",
        f32_bytes as f64 / binary_bytes as f64,
        clustered.binary * 100.0,
        uniform.binary * 100.0
    );
    println!();
    // Diagnostic: is the corpus actually unit-norm, and does the reconstruction
    // preserve that? If reconstruction norm drifts, an inner-product ranking
    // over reconstructions is ranking partly by norm error.
    {
        let cfg = PolarQuantConfig::new(3, DIMS, 0xA5A5_A5A5);
        let sample: Vec<Vec<f32>> = (0..200).map(clustered_vector).collect();
        let orig: f64 = sample
            .iter()
            .map(|v| v.iter().map(|x| x * x).sum::<f32>().sqrt() as f64)
            .sum::<f64>()
            / 200.0;
        let recon_norms: Vec<f64> = sample
            .iter()
            .map(|v| {
                let r = dequantise_polar(&quantise_polar(v, &cfg), &cfg);
                r.iter().map(|x| x * x).sum::<f32>().sqrt() as f64
            })
            .collect();
        let mean = recon_norms.iter().sum::<f64>() / 200.0;
        let spread = recon_norms
            .iter()
            .map(|n| (n - mean).abs())
            .fold(0.0f64, f64::max);
        println!(
            "  norms: original {orig:.4}, reconstructed mean {mean:.4}, max deviation {spread:.4}"
        );
    }
    println!();
    println!(
        "  polar cross-check (approx_inner_product_raw vs dequantise+cosine): \
         clustered {:.1}% vs {:.1}%, uniform {:.1}% vs {:.1}%",
        clustered.polar3_inner_product * 100.0,
        clustered.polar3_dequantised * 100.0,
        uniform.polar3_inner_product * 100.0,
        uniform.polar3_dequantised * 100.0
    );
    println!();

    // The two polar paths do NOT agree, and the reason is understood rather
    // than mysterious: PolarQuant reconstruction does not preserve the norm
    // (see the norms line above and tests/polar_padding_probe.rs). Cosine
    // divides that error out per candidate; a raw inner product does not, so
    // it ranks partly by reconstruction-norm error and loses recall.
    //
    // Asserted as an ordering rather than an equality, so the gap is recorded
    // and a regression that made the fast path worse still fails the build.
    assert!(
        clustered.polar3_inner_product <= clustered.polar3_dequantised + 0.02,
        "approx_inner_product_raw ({:.3}) beat cosine-on-reconstruction ({:.3}); \
         the norm-error explanation for their gap no longer holds and both \
         numbers need re-deriving",
        clustered.polar3_inner_product,
        clustered.polar3_dequantised
    );
    assert!(
        clustered.polar3_inner_product >= 0.15,
        "approx_inner_product_raw recall@{K} regressed to {:.3} (floor 0.15)",
        clustered.polar3_inner_product
    );

    // Floors on the clustered distribution only. Uniform i.i.d. data is a
    // near-tied ranking, so a floor there would be a coin-flip gate that
    // fires in normal operation and stops being read as a signal.
    // Each floor sits below the observed value, not at it.
    assert!(
        clustered.int8 >= 0.90,
        "int8 recall@{K} regressed to {:.3} (floor 0.90)",
        clustered.int8
    );
    assert!(
        clustered.polar3_dequantised >= 0.40,
        "polar 3-bit recall@{K} regressed to {:.3} (floor 0.40)",
        clustered.polar3_dequantised
    );
    assert!(
        clustered.polar4 >= 0.45,
        "polar 4-bit recall@{K} regressed to {:.3} (floor 0.45)",
        clustered.polar4
    );
    assert!(
        clustered.polar4 > clustered.polar3_dequantised,
        "ordering violated: 4-bit ({:.3}) should beat 3-bit ({:.3})",
        clustered.polar4,
        clustered.polar3_dequantised
    );
    assert!(
        clustered.binary >= 0.15,
        "binary recall@{K} regressed to {:.3} (floor 0.15)",
        clustered.binary
    );
    assert!(
        clustered.int8 > clustered.binary,
        "ordering violated: int8 ({:.3}) should beat binary ({:.3})",
        clustered.int8,
        clustered.binary
    );
}
