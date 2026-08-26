//! Probe: PolarQuant truncates rotated coefficients to `dimensions`, but the
//! Walsh-Hadamard rotation spreads energy uniformly across `padded_dim`. For
//! any dimension that is not a power of two, the difference is discarded.
//!
//! If that is the dominant recall loss, then a power-of-two dimension (where
//! padded_dim == dimensions, so nothing is discarded) should recall far better
//! than a nearby non-power-of-two dimension. This test measures exactly that
//! contrast, so the diagnosis is falsifiable rather than asserted.
//!
//! Run with:
//!   cargo test -p trondb-core --test polar_padding_probe -- --nocapture

use trondb_core::quantise::*;

const CORPUS: usize = 2_000;
const QUERIES: usize = 100;
const K: usize = 10;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1))
    }
    fn next_signed(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (((self.0 >> 33) as f64 / (1u64 << 30) as f64) - 1.0) as f32
    }
}

fn clustered(seed: usize, dims: usize) -> Vec<f32> {
    let mut c = Rng::new(0xC0FFEE + (seed % 50) as u64);
    let mut n = Rng::new(0x5EED + seed as u64);
    let v: Vec<f32> = (0..dims)
        .map(|_| c.next_signed() + 0.35 * n.next_signed())
        .collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.into_iter().map(|x| x / norm).collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

fn top_k(mut scores: Vec<(usize, f32)>) -> Vec<usize> {
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scores.into_iter().take(K).map(|(i, _)| i).collect()
}

/// Returns (mean recall@K, mean reconstructed norm, padded_dim).
fn probe(dims: usize, bits: u8) -> (f64, f64, usize) {
    let cfg = PolarQuantConfig::new(bits, dims, 0xA5A5_A5A5);
    let corpus: Vec<Vec<f32>> = (0..CORPUS).map(|i| clustered(i, dims)).collect();
    let recon: Vec<Vec<f32>> = corpus
        .iter()
        .map(|v| dequantise_polar(&quantise_polar(v, &cfg), &cfg))
        .collect();

    let mean_norm = recon
        .iter()
        .map(|v| v.iter().map(|x| x * x).sum::<f32>().sqrt() as f64)
        .sum::<f64>()
        / CORPUS as f64;

    let mut total = 0.0;
    for q in 0..QUERIES {
        let query = clustered(CORPUS + q * 977, dims);
        let exact = top_k(
            corpus
                .iter()
                .enumerate()
                .map(|(i, v)| (i, cosine(&query, v)))
                .collect(),
        );
        let approx = top_k(
            recon
                .iter()
                .enumerate()
                .map(|(i, v)| (i, cosine(&query, v)))
                .collect(),
        );
        total += approx.iter().filter(|i| exact.contains(i)).count() as f64 / K as f64;
    }

    (total / QUERIES as f64, mean_norm, cfg.padded_dim)
}

#[test]
fn power_of_two_dimensions_do_not_truncate() {
    println!();
    println!("PolarQuant 3-bit, recall@{K} over {CORPUS} clustered vectors, {QUERIES} queries");
    println!();
    println!("   dims   padded   discarded   recon norm   sqrt(kept)   recall@{K}");

    let mut results = Vec::new();
    for &dims in &[256usize, 320, 384, 448, 512, 640, 768, 1024] {
        let (recall, norm, padded) = probe(dims, 3);
        let kept = dims as f64 / padded as f64;
        println!(
            "  {dims:>5}   {padded:>6}   {:>8.1}%   {norm:>10.4}   {:>10.4}   {:>9.1}%",
            (1.0 - kept) * 100.0,
            kept.sqrt(),
            recall * 100.0
        );
        results.push((dims, padded, recall));
    }
    println!();

    let pow2: Vec<f64> = results
        .iter()
        .filter(|(d, p, _)| d == p)
        .map(|(_, _, r)| *r)
        .collect();
    let non_pow2: Vec<f64> = results
        .iter()
        .filter(|(d, p, _)| d != p)
        .map(|(_, _, r)| *r)
        .collect();
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;

    println!(
        "  power-of-two dims mean recall {:.1}%, other dims mean recall {:.1}%",
        mean(&pow2) * 100.0,
        mean(&non_pow2) * 100.0
    );
    println!();

    assert!(
        mean(&pow2) > mean(&non_pow2),
        "diagnosis not supported: power-of-two dims ({:.3}) did not beat padded dims ({:.3})",
        mean(&pow2),
        mean(&non_pow2)
    );
}
